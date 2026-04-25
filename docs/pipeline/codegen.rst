Pass 4 — Code generation
========================

Entry points: ``codegen::find`` (look up a backend by name) and
``codegen::emit`` (run lowering and then the backend). Backend
implementations live under ``src/codegen/<target>.rs``.

Inputs
------

A backend is a pure function::

    fn(&StateTable) -> Vec<EmittedFile>

Every backend reads the same ``StateTable``. An ``EmittedFile`` is
just a relative path plus UTF-8 contents — backends never touch the
filesystem themselves; the CLI handles that.

The shape each backend produces
-------------------------------

All backends produce the same logical artifacts, encoded in the
target language's idioms:

1. **TokenKind enum.** One variant per entry in ``state_table.tokens``,
   with ``EOF = 0`` and ``ERROR = -1``. The representation is ``i16``
   (or the language's closest equivalent).
2. **RuleKind enum.** One variant per entry in
   ``state_table.rule_kinds``, representation ``u16``.
3. **Name-lookup helpers.** ``tokenKindName`` / ``ruleKindName`` —
   useful for diagnostics and test output.
4. **Compiled lexer DFA.** Not a packed table — the DFA is emitted
   as code. Each state becomes one arm of a ``match``/``switch``
   that reads the current byte and dispatches on it; the whole
   machine is wrapped in a ``longest_match`` function exposed
   through the runtime's ``DfaMatcher`` (or equivalent) interface.
   Byte ranges sharing the same target collapse into single arms
   (so an ``[a-z]+`` token compiles to ``Some(&(b'a'..=b'z'))``
   rather than 26 individual byte arms). See
   :ref:`compiled-lexer-dfa` below for the shape and rationale.
5. **FIRST and SYNC intern pools.** Each entry from
   ``state_table.first_sets`` / ``state_table.sync_sets`` becomes a
   named constant (``FIRST_N``, ``SYNC_N``). States refer to them by
   index, so syntactically-similar grammar pieces share tables.
6. **Entry-state constants.** One ``ENTRY_<RULE>`` per public rule.
7. **State-dispatch function.** A single function — in most backends
   a large ``switch`` on the current state id — that runs one state
   of the parser and either consumes an event or transfers control.
8. **parse_<rule> entry points.** One convenience constructor per
   public rule, wrapping the runtime's ``Parser::new`` with the
   correct entry-state constant and a configured lexer.
9. **A small amount of backend-specific plumbing.** Package
   declarations, import blocks, module wrappers, and whatever else
   the target language needs. Each backend keeps this to the
   minimum.

The state-dispatch function: how ops become code
------------------------------------------------

The core translation every backend performs is from ``Op`` values to
statements in the target language. The mapping is direct:

``Enter(kind)`` / ``Exit(kind)``
  A call into the runtime: ``parser.enter(RuleKind::X)`` /
  ``parser.exit(RuleKind::X)``. The runtime emits the structural
  event and positions it at the current parse cursor.

``Expect { kind, token_name, sync }``
  ``parser.expect(TokenKind::X, SYNC_N, "expected X")``. The runtime
  handles the match/recover/retry protocol (see
  :doc:`../event_model`).

``PushRet(s)``
  ``parser.push_ret(s)``.

``Jump(s)``
  Set the current state to ``s`` and break out of the dispatch
  iteration (or, in languages where it's simpler, fall through to
  the next case by numeric adjacency).

``Ret``
  ``parser.state = parser.ret()``.

``Star { first, body, next }`` / ``Opt { first, body, next }``
  An ``if parser.matches_first(FIRST_N)`` branch: on a match, push
  the appropriate return state and jump into ``body``; on a miss,
  jump to ``next``.

``Dispatch { tree, sync, next }``
  A nested ``switch``. Each ``DispatchTree::Switch`` becomes one
  level of ``switch (parser.look(depth).kind)``; each ``Leaf``
  becomes an action — take an arm (push ``next``, jump to the arm's
  body), fall through to ``next``, or ``error+recover(sync)``. The
  trie structure is what lets a k-token lookahead compile to k
  nested switches rather than a linear scan.

Because the ops are already linear and the state ids are dense
integers, most backends can emit the whole parser as a single
``switch`` in a single function.

.. _compiled-lexer-dfa:

The compiled lexer DFA
----------------------

The lexer DFA is **compiled to code**, not to data. Each backend
walks ``state_table.lexer_dfa.states`` and emits a ``longest_match``
function shaped like:

.. parsed-literal::

    fn longest_match(buf, start) -> DfaMatch:
        pos        = start
        best_len   = 0
        best_kind  = TokenKind.Error
        state      = <dfa.start>
        loop:
            match state:
                1 => match buf[pos]:
                    'a'..='z' => { pos += 1; state = 2;
                                   best_len = pos - start;
                                   best_kind = TokenKind.Ident; }
                    _         => break
                2 => match buf[pos]:
                    ...
                ...
            ...
        return DfaMatch { len: best_len, kind: best_kind }

One outer ``switch`` arm per DFA state, one inner ``switch`` arm
per byte target. Whenever a transition lands on an accept state,
the surrounding code records ``(best_len, best_kind)`` so the
classic longest-match rule falls out of the structure.

Byte-range collapsing
~~~~~~~~~~~~~~~~~~~~~

A naive emission would produce 256 arms per DFA state. Instead the
DFA compiler in ``lowering::lexer_dfa`` returns its states with
transitions already grouped: each ``DfaState`` carries its
``arms: Vec<ByteArm>``, where bytes sharing a target collapse into
one arm and contiguous bytes within an arm collapse into ranges.
So ``IDENT = ('a'..'z' | '_')+;`` compiles to two arms per state —
one for ``b'a'..=b'z'``, one for ``b'_'`` — not 27. Every backend
works from the same pre-grouped shape; no per-backend collapsing
logic is needed.

Why code instead of tables
~~~~~~~~~~~~~~~~~~~~~~~~~~

* **Constant folding by the host compiler.** The byte-range
  patterns and state transitions are visible to LLVM, javac, the
  Go SSA pass, and friends; they compile to jump tables, branch
  tables, or inlined comparisons depending on the target. A
  data-driven walk over a packed transition array is opaque to
  those optimisers.
* **Dense binaries for sparse DFAs.** Most DFA states have only a
  handful of live transitions; emitting just those arms is much
  smaller than reserving 256 entries per state.
* **Locality.** The parser dispatch is already one big
  ``switch``; the DFA matcher is the same shape in the same file,
  so a reader sees the whole machine as code rather than as
  opaque tables threaded through a runtime helper.

Each backend supplies its compiled matcher to the runtime through
a ``DfaMatcher`` trait (Rust), interface (Go, TypeScript, Java,
C#), or function pointer (C). The runtime calls
``longest_match(buf, pos)`` once per token; everything else about
the lexer (input buffering, position tracking, EOF handling) stays
generic in the runtime crate.

Shared helpers
--------------

File: ``src/codegen/common.rs``.

Language-neutral string helpers live here: naming conversions
(``pascal``, ``camel``, ``snake``, ``screaming_snake``), token
literal escaping, FIRST/SYNC-set formatting helpers, and a few
small utilities the backends share. A backend's job is mostly
deciding what layout to produce; ``common`` exists so two backends
producing the same name convention do so consistently.

Differences between backends
----------------------------

The public surface is the same everywhere. The per-backend variation
is limited to:

* **File layout.** Rust and C# emit a single file; Java emits one
  file per public type plus a ``Grammar`` class with the DFA tables;
  C emits a header plus implementation; Go emits a single package
  file with helper types re-exported from the runtime.
* **Build file.** Python wraps the generated Rust in a ``pyo3``
  extension module and emits a ``Cargo.toml`` plus ``pyproject.toml``
  so ``maturin`` can build it. The other backends assume an existing
  build system in the consumer project.
* **Runtime dependency.** Rust, Python, Go, Java, C#, and TypeScript
  import a pre-built runtime crate/package (``parsuna-rt``,
  ``parsuna.dev/parsuna-rt-go``, etc.). C inlines the runtime into
  ``parsuna_rt.h`` so a generated parser is self-contained.

The TypeScript, Go, Java, and C# backends also differ in how they
encode the ``Event`` tagged union — an idiomatic discriminated union
in TypeScript, a struct with an ``EventTag`` field in Go, a sealed
class hierarchy in Java, a ``record`` hierarchy in C# — but the
semantics are the same in each case.

The tree-sitter backend
-----------------------

File: ``src/tree_sitter.rs``.

Parsuna can also emit a ``grammar.js`` for tree-sitter, via the
``parsuna <grammar> tree-sitter`` subcommand. This is not a full
backend — it does not use the ``StateTable`` at all, only the
``AnalyzedGrammar`` — because tree-sitter has its own parser
generator. The emitted file is a declarative transliteration of the
parsuna grammar into tree-sitter's combinator DSL, intended for
editor tooling (syntax highlighting, code folding). It does not
share the pull-parser runtime.

Putting it all together
-----------------------

The CLI's ``generate`` subcommand is therefore tiny: parse the
grammar, analyze it, lower it, look up the backend by name, call
``(backend.emit)(&state_table)``, and write each ``EmittedFile`` to
disk under the output directory. The passes documented in this
section are everything the generator does.
