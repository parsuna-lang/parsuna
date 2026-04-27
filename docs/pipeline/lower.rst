Pass 3 — Lowering
=================

Entry point: ``lowering::lower`` (``src/lowering/mod.rs``).
Output: a ``StateTable`` — the flat, backend-agnostic representation
of both the parser and the lexer.

Lowering produces two things: the **lexer DFA** (what the runtime
consults to turn bytes into tokens) and the parser **state machine**
(what consumes those tokens). The DFA is the simpler of the two and
runs first at parse time, so this chapter covers it first. The state
machine is then built by four sub-passes — **build**, **layout**,
**fuse**, and a small **mark-references** post-pass that flags which
FIRST tables backends actually need to emit. The layout sub-pass also
triggers DFA compilation as part of laying out the final
``StateTable``.

The StateTable
--------------

The artifact handed to code generation holds:

* ``grammar_name`` — copied from the ``Grammar``, used by backends
  for file and package names.
* ``tokens: Vec<TokenInfo>`` — non-fragment tokens, each with its
  resolved pattern (fragments inlined), its ``skip`` flag, and a
  stable numeric kind id (1-based; ``0`` is EOF). Lex failures are
  surfaced per-language: ``Option<TK>`` (``None``) for Rust /
  TypeScript / Python, and the unsigned sentinel ``0xFFFF`` for C /
  Java / C# / Go. Either way, no grammar-declared kind id collides
  with the failure marker.
* ``rule_kinds: Vec<String>`` — names of the non-fragment rules in
  declaration order. A rule's ``RuleKind`` id is its index here.
* ``first_sets: Vec<FirstSet>`` — the interned FIRST-set pool. Each
  entry is ``{ id, seqs: Vec<Vec<u16>>, has_references: bool }``: a
  set of token-id sequences plus a flag set during a post-fuse pass
  to mark the entries the generated runtime will actually reference
  (see *Sub-pass 3d* below). Backends emit a constant for an entry
  only when the flag is set.
* ``sync_sets: Vec<SyncSet>`` — the interned SYNC-set pool. Each
  entry is ``{ id, kinds: Vec<u16> }``: a flat list of token ids.
* ``states: BTreeMap<StateId, State>`` — every parser state, keyed
  by id. Each ``State`` holds a label and a short straight-line
  program of ``Op``\ s.
* ``entry_states: Vec<(String, StateId)>`` — public entry points,
  one per non-fragment rule.
* ``k`` — the grammar's LL(k).
* ``lexer_dfa`` — the compiled lexer DFA.

EOF's id is the constant ``parsuna_rt::TOKEN_EOF`` (= ``0``); it is
not stored on the state table because every backend can read the
constant directly.

The lexer DFA
-------------

File: ``src/lowering/lexer_dfa.rs``.

At runtime the lexer turns the source bytes into a stream of tokens
before the parser state machine ever runs, so it is the natural place
to start. The DFA is compiled from the list of non-fragment tokens —
with fragment references resolved by the build phase beforehand —
using standard Thompson construction plus subset determinization:

1. **NFA construction.** Each token's ``TokenPattern`` is compiled to
   an NFA fragment whose end state accepts that token's kind id.
   ``Seq`` concatenates fragments; ``Alt`` ε-joins them at both ends;
   ``Opt`` / ``Star`` / ``Plus`` use the classical ε-transition
   patterns. ``Ref`` patterns are unreachable at this point —
   fragments are resolved earlier — so reaching one is a bug.
2. **Top-level alternation.** All tokens share a single start state
   via ε-transitions. This is what lets the lexer try every pattern
   in parallel.
3. **Subset construction.** The NFA is determinized into a flat
   ``Vec<DfaState>``. Each state carries an optional
   ``accept: Option<u16>`` plus its byte transitions already grouped
   into ``ByteArm`` runs (bytes sharing a target collapse into one
   arm; contiguous bytes within an arm collapse into ranges). State
   ``0`` ([``DEAD``]) is reserved as the dead sink — every missing
   transition lands there, so the runtime's inner loop is a single
   branch ("exit on 0"). The start state is always ``1``
   ([``START``]). The accept kind for a DFA state is the **minimum**
   token id present in the collapsed NFA states, which encodes
   "declaration order = priority": tokens declared earlier win on
   ties.
4. **Minimization.** Subset construction over a UTF-8 NFA produces
   many nearly-identical states — for example four separate
   "I'm scanning whitespace" states because each entry byte arrives
   at a fresh DFA state. ``minimize`` partitions states by ``accept``
   value, then iteratively splits each block by 256-byte transition
   signature against the current partitioning, until partitions
   stabilize. Equivalent states collapse into one. The pass preserves
   longest-match semantics — accept visits along any input trace
   stay on the same input position — and it makes the next step
   substantially more effective.
5. **Self-loop detection.** For each state, the ``self_loop`` field
   is filled with the byte ranges whose arms loop the state back to
   itself (every byte ``b`` for which the matching arm's ``target``
   equals ``state.id``). Backends use this to emit a "scan past
   every byte in this set" prologue before the per-byte switch —
   turning the byte-by-byte hot loop on ``[a-z]+``-like states into
   one bulk scan that the optimizer can autovectorize. The arms
   themselves are unchanged (the self-loop arm stays in the dispatch
   table for backends that don't implement the prologue, or for
   debug fall-through).

At runtime the lexer implements **longest match**: advance until the
transition table lands on dead, then back up to the last DFA state
that had an accept. The ``skip`` flag on ``TokenInfo`` is read by
the parser's pump, not the DFA — skip tokens are matched the same
way as any other, they are just routed to a side queue on their way
into the event stream.

The bytes the DFA consumes are UTF-8 octets. Character classes with
multi-byte codepoints expand to multi-byte NFA paths, so the DFA
implicitly handles UTF-8 without a separate decoder. (The character
class lowering performs the canonical Russ-Cox split into UTF-8 byte
sequences before the NFA is built — see ``class_byte_seqs`` in
``lexer_dfa.rs``.)

The state-machine op set
------------------------

Each state is a small straight-line program over these ops:

``Enter(kind)`` / ``Exit(kind)``
  Emit a structural event.

``Expect { kind, token_name, sync }``
  Consume a token of ``kind``; on mismatch, emit an error and
  recover to the given SYNC set.

``PushRet(state)``
  Push a return address onto the call stack. At layout time it is
  followed by a ``Jump`` into the callee, but fuse may splice that
  ``Jump`` away by inlining the callee's leading straight-line ops,
  in which case ``PushRet`` is followed directly by whatever ops
  were absorbed (``Enter``/``Exit``/``Expect``/another ``PushRet``/
  ``Ret``). The semantics are unchanged — the return address is on
  the stack before any absorbed op runs.

``Jump(state)``
  Unconditional transfer.

``Ret``
  Pop the top return address (or terminate if the stack is empty).

``Star { first, body, cont, head }``
  While the lookahead matches ``first``, push ``head`` (the loop-back
  state, normally the state hosting this op) and call ``body``.
  Otherwise transfer to ``cont``: if ``Some(s)``, ``cur = s``; if
  ``None``, ``cur = ret()`` directly — see :ref:`tail-call-elimination`.

``Opt { first, body, cont }``
  If the lookahead matches ``first``, push the continuation and call
  ``body``; otherwise transfer to it directly. ``cont`` is
  ``Some(state)`` for the original push-and-jump shape, or ``None``
  for a tail call (``cur = body`` on match, ``cur = ret()`` on miss).

``Dispatch { tree, sync, cont }``
  Pick one ``Alt`` arm using a ``DispatchTree`` over up to ``k``
  tokens of lookahead, or recover via ``sync`` on no match. ``cont``
  carries the same encoding as ``Opt``: ``Some(state)`` means each
  arm pushes that state before jumping to its body and the
  ``Fallthrough``/``Error`` paths jump to it; ``None`` means tail
  call across the board.

``DispatchTree`` is a flat decision tree: ``Leaf(action)`` or
``Switch { depth, arms, default }``. Each ``Switch`` inspects
``look(depth).kind`` and branches into sub-trees — this maps
directly onto nested ``switch`` statements in every target.

Sub-pass 3a — Build
-------------------

File: ``src/lowering/build.rs``.
Output: a ``Program`` (blocks with symbolic block ids, interned
FIRST/SYNC pools, token metadata).

The build phase walks each rule body and emits a block of block-level
ops. It is a faithful translation of the ``Expr`` tree:

* ``Expr::Token(n)`` → ``Op::Expect``, carrying the token name and
  the current rule's SYNC id.
* ``Expr::Rule(n)`` → ``Op::Call { target: Target::Rule(n) }``. The
  call target is symbolic — the actual block id is resolved in
  sub-pass 3b.
* ``Expr::Seq(xs)`` → the children are emitted into the same block,
  each with an accurately computed tail (FIRST of the remainder plus
  the outer tail) so their prediction sets are precise.
* ``Expr::Alt(xs)`` → one ``Op::Dispatch``. Each non-wholly-nullable
  arm becomes a sub-block whose entry id is stored alongside its
  FIRST-set id. ``has_eps`` summarizes whether any arm is nullable,
  which is what turns the outer dispatch default from "error" to
  "fall through".
* ``Expr::Opt(x)`` / ``Expr::Star(x)`` → one ``Op::Opt`` / ``Op::Star``
  plus a sub-block for the body.
* ``Expr::Plus(x)`` → one mandatory ``Op::Call`` into the body's
  sub-block, followed by an ``Op::Star`` over the same sub-block.
  The body is lowered once and shared.

Public rules are wrapped in ``Op::Enter`` / ``Op::Exit`` pairs so
their subtree appears in the event stream. Fragment rules are emitted
raw — calling into a fragment is indistinguishable from inlining its
body.

**FIRST-set interning.** Every prediction set that appears in an op
is deduplicated through a hash table and replaced with a numeric id.
This lets two syntactically-similar grammar fragments share a single
table entry in the final output, which keeps the emitted source
small.

**SYNC-set computation.** At the start of each rule, the build phase
computes the rule's recovery set (``FOLLOW(rule) ∪ {EOF}``), interns
it, and caches the id in ``current_sync`` for the duration of that
rule's lowering. Every ``Expect`` the builder emits stamps this id
on itself, so recovery at any point in the rule hits the same set.

**Labels.** Each op carries a human-readable label
(``expr:alt0:call:atom``) used in debug dumps and as a comment in the
generated code. Sub-blocks extend the parent's label so the hierarchy
is visible without extra bookkeeping.

Sub-pass 3b — Layout
--------------------

File: ``src/lowering/layout.rs``.

Layout turns the block-level IR into numbered parser states and
resolves every symbolic target into a concrete state id.

Steps:

1. **Block ordering.** BFS from each public rule's entry block; each
   block reached is appended to an ``order`` list. This keeps a rule
   and everything it transitively reaches adjacent in the state
   numbering, which is what makes debug dumps and generated switch
   tables readable.
2. **Entry-id assignment.** Each block is assigned a starting
   ``StateId``, reserving one slot per op plus one for a trailing
   ``Ret`` state. Id ``0`` is reserved for the ``TERMINATED``
   sentinel; real ids start at ``1``.
3. **Op lowering.** Each block-level op is translated to a
   state-level op sequence:

   * Straight-line ops (``Enter``, ``Exit``, ``Expect``, ``Call``)
     are followed by an explicit ``Jump(fall)`` so each state has a
     visible exit.
   * A ``Call { target }`` becomes ``PushRet(fall); Jump(resolve(target))``.
   * Branchy ops (``Opt``, ``Star``, ``Dispatch``) encode the
     fall-through inside the op (``cont``, initially
     ``Some(fall)``) and do not need a trailing jump. The
     :ref:`tail-call-elimination` pass may later rewrite ``cont`` to
     ``None``.

4. **Dispatch-tree construction.** The block-level ``Op::Dispatch``
   carries a flat list of ``(first_set_id, target)`` arms. Layout
   expands those into a ``DispatchTree`` via ``build_dispatch_tree``:
   each arm contributes one entry per sequence in its FIRST set, and
   the trie groups entries by their ``depth``-th token at each level.
   An arm whose prediction sequence runs out at depth *d* terminates
   the branch with that arm's id, shadowing the outer default for
   any deeper prefixes.
5. **Lexer DFA.** ``lexer_dfa::compile`` is called on the token list
   (see the `The lexer DFA`_ section above). The resulting
   ``Vec<DfaState>`` is stored on the state table.

Output: a ``StateTable`` — complete but not yet optimized.

Sub-pass 3c — Fuse
------------------

File: ``src/lowering/fuse.rs``.

Fuse runs three small clean-ups in order:

* **Jump-chain splicing.** A state whose last op is ``Jump(T)`` can
  often absorb ``T``'s ops directly, turning ``A → B → C → D`` into
  a single ``A``. The trailing ``Jump`` is popped and ``T``'s ops are
  appended in its place. Two splice regimes:

  - **Single-predecessor splice.** ``T`` is referenced from exactly
    one place (this very ``Jump``). Inlining moves ``T``'s ops into
    the predecessor and leaves ``T`` dead, so dead-state elimination
    drops it. Pure code motion: unbounded depth, allowed even when
    ``T``'s first op is branchy because no other caller depends on a
    shared dispatch entry.
  - **Multi-predecessor splice.** ``T`` has other callers, so
    inlining duplicates its ops at this call site while keeping the
    original. Bounded by ``DUPLICATION_BUDGET`` (currently ``6``) and
    gated to non-branchy first ops to avoid splintering a shared
    ``Dispatch``/``Opt``/``Star`` entry across callers.

  Reference counting is *external* — a state's self-references
  (e.g. an ``Op::Star`` whose ``head`` points back to its containing
  state) don't pin it in place. A visited-set guards against jump
  chains that loop back on themselves.

  One consequence worth calling out: the ``[PushRet(fall),
  Jump(callee)]`` pair that ``Call`` layouts into is a normal splice
  candidate. After fuse, the ``Jump`` into the callee is often gone
  and the callee's leading ops are pasted directly after the
  ``PushRet``. The return address still sits on the stack before any
  of the absorbed ops run, so the semantics are unchanged.

* **Tail-call elimination.** See :ref:`tail-call-elimination` below
  for the rewrite that drops ``PushRet``\ s and rewires
  ``Opt``/``Dispatch``/``Star`` continuations whose target is a
  pure-``Ret`` trampoline.

* **Dead-state elimination.** BFS from the public entry points,
  following every reachable target (``Jump``, ``PushRet``,
  ``Opt.body``, ``Opt.cont`` *when ``Some``*, every leaf of every
  ``DispatchTree``, ``Star.body``/``Star.cont``/``Star.head``). Any
  state not reached is removed from the table.

.. _tail-call-elimination:

Tail-call elimination
~~~~~~~~~~~~~~~~~~~~~

Splicing tends to leave a state whose ops are exactly ``[Op::Ret]``.
Such a *trampoline* state does nothing observable — when reached, it
pops the next frame off the return stack — but it still costs a
dispatch loop iteration each time control bounces through it.
Eliminating the push that lands on a trampoline lets the called
code's trailing ``Ret`` pop the caller's continuation directly.

Four patterns get optimised, all with the same justification:

1. **Explicit ``Op::PushRet(B)``** where ``B = [Op::Ret]`` — the
   push is dropped outright. Typical post-splice shape:

   .. code-block:: text

      state A: [PushRet(B), Enter(R), Expect(...), Jump(C)]
      state B: [Ret]

2. **``Op::Opt`` whose ``cont`` is ``[Op::Ret]``** — the codegen
   would have emitted ``push_ret(cont); cur = body`` on match and
   ``cur = cont`` on miss. After this pass ``cont`` is ``None``;
   backends emit ``cur = body`` (no push) on match and ``cur =
   ret()`` on miss.
3. **``Op::Dispatch`` whose ``cont`` is ``[Op::Ret]``** — same
   shape as ``Opt``, applied across every ``Arm`` leaf and the
   ``Fallthrough`` / post-recovery ``Error`` paths.
4. **``Op::Star`` whose ``cont`` is ``[Op::Ret]``** — only the
   miss / fall-through path is rewritten. ``Star``'s match path
   pushes ``head`` (the loop-back state holding this very ``Star``
   op), which is therefore never a pure-``Ret`` trampoline. By the
   time the loop misses, every iteration has already pushed *and*
   popped its ``head`` frame, so the stack is back to whatever was
   there when the loop was entered — a direct ``ret()`` lands on
   the same frame the trampoline-then-ret bounce would have.

Safety:

* The trampoline state must be exactly ``[Op::Ret]``. ``Enter`` /
  ``Exit`` / ``Expect`` are observable and can't be skipped.
* Entry states are never optimised away — they must remain
  callable from outside the dispatch even if their body has been
  spliced down to a single ``Ret``.

Once ``cont`` flips to ``None``, dead-state elimination's
reachability walk no longer follows the field, so the trampoline
state goes unreferenced and is removed on the same pass.

Sub-pass 3d — Mark FIRST-set references
---------------------------------------

File: ``src/lowering/mod.rs`` (``mark_first_set_references``).

A small post-pass that decides which FIRST sets the generated runtime
will actually reference. The codegens use this to skip emitting
constants for unreferenced entries.

The rule, encoded as the ``FirstSet::has_references`` flag:

* For LL(1) grammars, **no** FIRST set is ever referenced. The
  ``Op::Star`` / ``Op::Opt`` codegen for ``k = 1`` inlines the FIRST
  set into a ``match`` arm pattern (one alternative per token kind)
  rather than calling ``matches_first(FIRST_n)``.
* For LL(k > 1) grammars, only ``Op::Star`` and ``Op::Opt`` consult
  the pool — ``matches_first(FIRST_n)`` is the only reference site.
  ``Op::Dispatch`` arms also point at FIRST sets, but those are
  consumed at lowering time when the ``DispatchTree`` is built; the
  resulting nested switch arms carry concrete token kinds, not pool
  ids, so the original FIRST set id is unreferenced after lowering.

The pass walks every state's ops, collects the ids reached by
``Op::Star`` / ``Op::Opt``, and sets ``has_references`` on the matching
``FirstSet`` entries (no-op when ``k == 1``).

The result is the ``StateTable`` that reaches code generation.

Interactions between build, layout, and fuse
--------------------------------------------

A few design notes that matter when reading the source:

* ``BlockId`` and ``FirstSetId`` / ``SyncSetId`` let the build phase
  stay symbolic. The layout phase resolves block ids into
  ``StateId``; the intern pools are passed through to the state
  table verbatim. Backends never see block ids.
* Every ``Expect`` inside a rule refers to the same SYNC set —
  computed once from the rule's FOLLOW set. That's why the intern
  pool stays small even for grammars with many productions.
* Fuse runs last on purpose. Splicing before dispatch-tree
  construction would obscure the control-flow shape the tree is
  built around; splicing after eliminates the single-op, single-exit
  states that the naïve layout produces so the emitted ``switch``
  has fewer labels without changing the semantics. Tail-call
  elimination then runs on the spliced output — splicing is what
  exposes most of the ``[Op::Ret]`` trampolines TCE removes.

What comes out
--------------

A ``StateTable`` is all a backend needs. From this point forward
nothing about the grammar source, the FIRST/FOLLOW computation, or
the block-level IR is visible — the code-generation pass sees only
states, ops, intern pools, and the lexer DFA.
