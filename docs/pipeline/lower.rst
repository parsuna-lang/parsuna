Pass 3 — Lowering
=================

Entry point: ``lowering::lower`` (``src/lowering/mod.rs``).
Output: a ``StateTable`` — the flat, backend-agnostic representation
of both the parser and the lexer.

Lowering produces two things: the **lexer DFA** (what the runtime
consults to turn bytes into tokens) and the parser **state machine**
(what consumes those tokens). One DFA per declared lexer mode, one
state machine for the whole grammar.

The state machine is built by four sub-passes — **build**, **layout**,
**optimize**, **validate** — plus a small **mark-references** post-pass
that flags which FIRST tables backends actually need to emit. The
layout sub-pass also triggers DFA compilation as part of laying out
the final ``StateTable``.

The StateTable
--------------

The artifact handed to code generation holds:

* ``grammar_name`` — copied from the ``Grammar``, used by backends
  for file and package names.
* ``tokens: Vec<TokenInfo>`` — non-fragment tokens, each with its
  resolved pattern (fragments inlined), its ``skip`` flag, a stable
  numeric kind id (1-based; ``0`` is EOF), and lexer-mode metadata
  (``mode_id`` for which mode the token lives in, ``mode_actions``
  for the ``-> push`` / ``-> pop`` actions to fire on a match). Lex
  failures are surfaced per-language: ``Option<TK>`` (``None``) for
  Rust / TypeScript / Python, and the unsigned sentinel ``0xFFFF`` for
  C / Java / C# / Go. Either way, no grammar-declared kind id collides
  with the failure marker.
* ``rule_kinds: Vec<String>`` — names of the non-fragment rules in
  declaration order. A rule's ``RuleKind`` id is its index here.
* ``first_sets: Vec<FirstSet>`` — the interned FIRST-set pool. Each
  entry is ``{ id, seqs: Vec<Vec<u16>>, has_references: bool }``: a
  set of token-id sequences plus a flag set during a post-optimize
  pass to mark the entries the generated runtime will actually
  reference (see *Sub-pass 3d* below). Backends emit a constant for
  an entry only when the flag is set.
* ``sync_sets: Vec<SyncSet>`` — the interned SYNC-set pool. Each
  entry is ``{ id, kinds: Vec<u16> }``: a flat list of token ids.
* ``states: BTreeMap<StateId, State>`` — every parser state, keyed
  by id. Each ``State`` holds a label and a ``Body`` (a sequence of
  ``Instr`` plus a terminating ``Tail``).
* ``entry_states: Vec<(String, StateId)>`` — public entry points,
  one per non-fragment rule.
* ``k`` — the grammar's LL(k).
* ``modes: Vec<ModeInfo>`` — one entry per declared lexer mode.
  ``modes[0]`` is always the default (anonymous) mode; further
  entries come from ``@mode(name)`` pre-annotations in declaration
  order. Each ``ModeInfo`` carries the mode's id, name, and
  compiled DFA. A grammar without modes has a single-entry vec
  whose DFA matches every token. ``StateTable::lexer_dfa()`` is a
  shorthand for ``modes[0].dfa``.

EOF's id is the constant ``parsuna_rt::TOKEN_EOF`` (= ``0``); it is
not stored on the state table because every backend can read the
constant directly. ``TERMINATED`` (``u32::MAX``) is the sentinel
state id meaning "the parser has finished" — also not stored on the
table.

The lexer DFAs
--------------

File: ``src/lowering/lexer_dfa.rs``.

At runtime the lexer turns the source bytes into a stream of tokens
before the parser state machine ever runs, so it is the natural place
to start. Each declared lexer mode gets its own DFA, compiled from the
list of non-fragment tokens whose ``mode_id`` matches — with fragment
references resolved by the build phase beforehand — using standard
Thompson construction plus subset determinization:

1. **NFA construction.** Each token's ``TokenPattern`` is compiled to
   an NFA fragment whose end state accepts that token's kind id.
   ``Seq`` concatenates fragments; ``Alt`` ε-joins them at both ends;
   ``Opt`` / ``Star`` / ``Plus`` use the classical ε-transition
   patterns. ``Ref`` patterns are unreachable at this point —
   fragments are resolved earlier — so reaching one is a bug.
2. **Top-level alternation.** All tokens in the mode share a single
   start state via ε-transitions. This is what lets the lexer try
   every pattern in parallel.
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
4. **Minimization** (gated on ``DfaOpts::minimize``). Subset
   construction over a UTF-8 NFA produces many nearly-identical
   states — for example four separate "I'm scanning whitespace"
   states because each entry byte arrives at a fresh DFA state.
   ``minimize`` partitions states by ``accept`` value, then iteratively
   splits each block by 256-byte transition signature against the
   current partitioning, until partitions stabilize. Equivalent
   states collapse into one. The pass preserves longest-match
   semantics — accept visits along any input trace stay on the same
   input position — and it makes the next step substantially more
   effective.
5. **Self-loop detection.** For each state, ``self_loop_ranges``
   returns the byte ranges whose arms loop the state back to itself
   (every byte ``b`` for which the matching arm's ``target`` equals
   ``state.id``). Backends use this to emit a "scan past every byte
   in this set" prologue before the per-byte switch — turning the
   byte-by-byte hot loop on ``[a-z]+``-like states into one bulk scan
   that the optimizer can autovectorize. The arms themselves are
   unchanged (the self-loop arm stays in the dispatch table for
   backends that don't implement the prologue, or for debug
   fall-through).

At runtime the lexer implements **longest match**: advance until the
transition table lands on dead, then back up to the last DFA state
that had an accept. The ``skip`` flag on ``TokenInfo`` is read by
the parser's pump, not the DFA — skip tokens are matched the same
way as any other, they are just routed differently on the way into
the event stream. Mode actions (``-> push(name)`` / ``-> pop``) are
applied by the runtime via the generated ``apply_actions`` callback
once a token has matched, so the active mode stays in sync with the
token stream.

The bytes the DFA consumes are UTF-8 octets. Character classes with
multi-byte codepoints expand to multi-byte NFA paths, so the DFA
implicitly handles UTF-8 without a separate decoder. (The character
class lowering performs the canonical Russ-Cox split into UTF-8 byte
sequences before the NFA is built — see ``class_byte_seqs`` in
``lexer_dfa.rs``.)

The state-machine op set
------------------------

A state's body is split into two halves: a list of **instructions**
that must run in order (none of them transfer control out of the
body), and exactly one **tail** at the end (which always does). The
type ``Body { instrs: Vec<Instr>, tail: Tail }`` encodes the split
structurally — there's no way to put a terminator in the middle of
``instrs`` or to omit the tail.

``Instr`` (non-terminating, sequenceable):

``Enter(kind)`` / ``Exit(kind)``
  Emit a structural event for the given rule-kind id.

``Expect { kind, token_name, sync }``
  Consume a token of ``kind``; on mismatch, emit an error and
  recover to the given SYNC set.

``PushRet(state)``
  Push a return address onto the call stack. Pairs with a future
  ``Tail::Ret`` somewhere down the call chain.

``Tail`` (terminating, exactly one per body, always last):

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

Inside a ``Tail::Star`` / ``Tail::Opt`` / ``Tail::Dispatch`` arm,
the *body* is also a ``Body`` — same ``instrs`` + ``tail`` split,
recursively. ``DispatchTree`` is the flat decision tree:
``Leaf(action)`` (``Arm(Body)`` / ``Fallthrough`` / ``Error``) or
``Switch { depth, arms, default }``. Each ``Switch`` inspects
``look(depth).kind`` and branches into sub-trees — this maps directly
onto nested ``switch`` statements in every target.

The runtime invariants
----------------------

Two semantic rules apply to every body in the table — both verified
by ``src/lowering/validate.rs`` after the optimizer finishes:

1. **At most one event per execution path** through the body.
   ``Drive::step`` runs one match arm and returns the event the
   body's path produced. Two events on the same path would mean
   one of them got lost.
2. **No lookahead-reading op after an ``Expect``** in the same body.
   An ``Expect`` consume leaves slot ``K-1`` empty; the runtime can
   only refill between ``step`` calls, so any subsequent
   ``Star`` / ``Opt`` / ``Dispatch`` / ``Expect`` would observe an
   unfilled slot.

Zero-event paths *are* allowed — ``step`` returns ``None`` and the
runtime's pull loop calls ``step`` again. They show up in
``Tail::Star`` / ``Tail::Opt`` miss paths (loop exit, optional skip)
and in pure-control bodies (``[Jump(s)]``) that the optimizer hasn't
folded.

Layout produces invariant-correct output on its own; every optimizer
pass is also gated on ``is_valid_body`` so a rewrite never produces
a body that violates either rule. ``assert_runtime_invariants``
runs as the final, mandatory step of ``lower_with_opts`` and panics
if anything slips through — it's contract enforcement, not an
optimization, and is always on regardless of which optimizer flags
are set.

Sub-pass 3a — Build
-------------------

File: ``src/lowering/build.rs``.
Output: a ``Program`` (blocks with symbolic block ids, interned
FIRST/SYNC pools, token metadata, mode-name table).

The build phase walks each rule body and emits a block of block-level
ops (``Enter`` / ``Exit`` / ``Expect`` / ``Call`` / ``Opt`` / ``Star``
/ ``Dispatch``). It is a faithful translation of the ``Expr`` tree:

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

**Mode-name table.** The build phase scans every token's ``mode``
field and assembles ``mode_names``: ``mode_names[0]`` is always the
default (id 0), and any ``@mode(name)`` annotation contributes one
entry per unique name in declaration order. Each token's ``mode_id``
is filled in from this table, and ``-> push(name)`` actions resolve
the mode name to a numeric ``ModeActionInfo::Push(id)`` so codegen
and the runtime never deal with mode names directly.

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
2. **Entry-id assignment.** Each block reserves exactly ``ops.len()``
   slots — one state per op. Ids start at ``1``; ``0`` is the
   ``TERMINATED`` sentinel (real states never share it). An empty
   block reserves a single slot for a ``Body { instrs: [], tail:
   Ret }``.
3. **Op lowering.** Each block-level op is translated to a state
   ``Body``. The block's *last* op is emitted in **tail form** —
   straight-line ops (Enter/Exit/Expect/Call) take ``Tail::Ret``
   instead of ``Tail::Jump`` to a separate trailing-Ret state, and
   branchy ops (Opt/Star/Dispatch) take ``cont: None`` (tail call,
   the body's trailing ``Ret`` pops the *caller's* continuation
   directly). Layout therefore never produces a standalone
   ``[Ret]``-only state, and the one-event-per-step invariant holds
   with zero optimizer passes run.

   * Straight-line ops emit a body whose ``instrs`` is the action
     and whose ``tail`` is ``Jump(fall)`` (non-tail) or ``Ret``
     (tail-of-block).
   * ``Op::Call { target }`` becomes ``Body { instrs:
     [PushRet(fall)], tail: Jump(callee) }``, or ``Body { instrs:
     [], tail: Jump(callee) }`` when the call is the block's last
     op (the callee's eventual ``Ret`` pops *our* caller directly).
   * Branchy ops live in the tail and encode their post-op
     transition via their own ``cont`` field.

4. **Dispatch-tree construction.** The block-level ``Op::Dispatch``
   carries a flat list of ``(first_set_id, target)`` arms. Layout
   expands those into a ``DispatchTree`` via ``build_dispatch_tree``:
   each arm contributes one entry per sequence in its FIRST set, and
   the trie groups entries by their ``depth``-th token at each level.
   An arm whose prediction sequence runs out at depth *d* terminates
   the branch with that arm's id, shadowing the outer default for
   any deeper prefixes.
5. **Lexer DFA, per mode.** ``lexer_dfa::compile_with_opts`` is called
   once per declared mode, on the subset of tokens whose ``mode_id``
   matches. The resulting per-mode ``Vec<DfaState>`` lands in
   ``ModeInfo``; the runtime selects which DFA to use based on the
   top of its mode stack.

Output: a ``StateTable`` — complete, invariant-correct, but not yet
size-optimized.

.. _tail-call-elimination:

Sub-pass 3c — Optimize
----------------------

File: ``src/lowering/optimize.rs``.

The optimizer is **purely** about size and dispatch-hop reduction;
the parser produced by layout alone is already correct (and already
satisfies the runtime invariants). Each pass is gated by a flag on
``LoweringOpts`` — turning any flag off is safe; the generated parser
still works, just with more state hops.

The four passes run in a fixpoint loop because they feed each other:
inlining shrinks reference counts (which dead-state elimination then
drops); dropping a state's last reference can turn neighbouring
states into trampolines; folding trampolines exposes more bodies that
``inline_branch_bodies`` can simplify.

* **inline_jumps** — when a body's ``tail`` is ``Jump(N)``, replace
  it with ``N``'s body (extending instrs with N's instrs, taking
  N's tail), as long as the result still satisfies ``is_valid_body``.
  Cycles are guarded by a visited set; chains stop at the first
  invariant-violating splice. The original target stays alive only
  if some other reference still points at it; ``eliminate_dead``
  picks up any leftovers.

* **fold_trampolines** — drops references to states whose body is
  exactly ``Body { instrs: [], tail: Ret }``. Two shapes get cleaned
  up:

  - **PushRet to a Ret state** — the push pairs with a future
    ``Ret`` that immediately re-pops. Drop the ``PushRet`` outright
    and the eventual ``Ret`` pops *our* caller's continuation
    directly.
  - **``cont: Some(s)`` where ``s = [Ret]``** — ``Tail::Star`` /
    ``Tail::Opt`` / ``Tail::Dispatch`` rewrite to ``cont: None``.
    Backends emit ``cur = ret()`` on the miss / fall-through path
    instead of bouncing through the trampoline.

  Crucially, the rewrite does not require the eliminated push to be
  in "tail position" with respect to its enclosing rule call: even
  if an earlier op in the same body already pushed something onto
  the stack, the analysis still holds. The trampoline's only
  behaviour is to immediately re-pop, so whether the calling body's
  trailing ``Ret`` pops via "trampoline-then-the-frame-below" or
  "the-frame-below-directly" produces the same end state. The
  optimisation only needs the continuation to be a pure ``Ret``,
  not the ambient stack to be empty.

  Entry states are never treated as trampolines: they need to remain
  callable by id from outside the dispatch loop even when their
  body has been spliced down to a single ``Ret``.

* **inline_branch_bodies** — replaces any sub-body that's just
  ``Body { instrs: [], tail: Jump(s) }`` inside a ``Tail::Star`` /
  ``Tail::Opt`` / dispatch arm with a copy of ``s``'s body, again
  under the ``is_valid_body`` gate. Multi-predecessor targets get
  duplicated into each caller; the original stays alive only if some
  other reference still points at it. A rewrite that pushes its host
  past one event (or violates the post-Expect rule) is reverted, so
  other branchy ops in the host can still inline.

* **eliminate_dead** — BFS from the public entry points, following
  every reachable target (``Jump``, ``PushRet``, ``Opt.body``,
  ``Opt.cont`` *when ``Some``*, every leaf of every ``DispatchTree``,
  ``Star.body`` / ``Star.cont`` / ``Star.head``). Any state not
  reached is removed.

After the fixpoint, **state-id compaction** renumbers the surviving
states to a dense ``1..N`` range. Splicing and DCE leave the
original layout-time ids sparse (e.g. ``1, 2, 3, 17, …``); compaction
is a pure renumber that lets backends emit tighter ``match`` /
``switch`` tables and per-state arrays.

Sub-pass 3d — Mark FIRST-set references
---------------------------------------

File: ``src/lowering/mod.rs`` (``mark_first_set_references``).

A small post-pass that decides which FIRST sets the generated runtime
will actually reference. The codegens use this to skip emitting
constants for unreferenced entries.

The rule, encoded as the ``FirstSet::has_references`` flag:

* For LL(1) grammars, **no** FIRST set is ever referenced. The
  ``Tail::Star`` / ``Tail::Opt`` codegen for ``k = 1`` inlines the
  FIRST set into a ``match`` arm pattern (one alternative per token
  kind) rather than calling ``matches_first(FIRST_n)``.
* For LL(k > 1) grammars, only ``Tail::Star`` and ``Tail::Opt``
  consult the pool — ``matches_first(FIRST_n)`` is the only
  reference site. ``Tail::Dispatch`` arms also point at FIRST sets,
  but those are consumed at lowering time when the ``DispatchTree``
  is built; the resulting nested switch arms carry concrete token
  kinds, not pool ids, so the original FIRST set id is unreferenced
  after lowering.

The pass walks every body's tail, collects the ids reached by
``Tail::Star`` / ``Tail::Opt``, and sets ``has_references`` on the
matching ``FirstSet`` entries (no-op when ``k == 1``).

Sub-pass 3e — Validate
----------------------

File: ``src/lowering/validate.rs``.

``assert_runtime_invariants`` walks every body in the table and
panics if the per-body invariants don't hold (more than one event
per path, or a lookahead-reading op after an ``Expect``). This is
the final step of ``lower_with_opts`` — mandatory, always on.
Catches source bugs (a layout / optimizer pass producing a
malformed body) before the table reaches codegen.

The IR's ``Body { instrs, tail }`` split makes the structural rule
"every body ends in a terminator" unrepresentable to violate; the
validator covers the two semantic rules the type system can't
express.

Interactions between build, layout, and optimize
------------------------------------------------

A few design notes that matter when reading the source:

* ``BlockId`` and ``FirstSetId`` / ``SyncSetId`` let the build phase
  stay symbolic. The layout phase resolves block ids into
  ``StateId``; the intern pools are passed through to the state
  table verbatim. Backends never see block ids.
* Every ``Expect`` inside a rule refers to the same SYNC set —
  computed once from the rule's FOLLOW set. That's why the intern
  pool stays small even for grammars with many productions.
* Layout already emits the block's last op in tail form (``Tail::Ret``
  for straight-line ops, ``cont: None`` for branchy ones). The
  one-event-per-step invariant therefore holds before optimize ever
  runs — turning every optimizer flag off still produces a
  correctly-validating parser, just with more state hops.
* The optimizer's fixpoint runs the four passes in a fixed order
  (inline_jumps → fold_trampolines → inline_branch_bodies →
  eliminate_dead) and re-runs them as long as anything changed.
  Convergence is guaranteed because every pass either shrinks the
  table or rewrites a body to a simpler shape, and there are
  finitely many bodies.

What comes out
--------------

A ``StateTable`` is all a backend needs. From this point forward
nothing about the grammar source, the FIRST/FOLLOW computation, or
the block-level IR is visible — the code-generation pass sees only
states, ops, intern pools, mode metadata, and the per-mode lexer DFAs.
