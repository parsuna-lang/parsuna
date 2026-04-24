Introduction
============

What parsuna gives you
----------------------

A generated parsuna parser is:

* **LL(k), with k chosen for you.** The analyzer iteratively raises the
  lookahead depth until every alternative in the grammar is unambiguous,
  so you never have to annotate a rule with a manual lookahead count. If
  no finite ``k`` works, the grammar is rejected with a conflict report
  naming the ambiguous prefixes.
* **Pull-based.** Parsing advances one step per call to ``next_event``.
  The parser never materializes a tree; it emits a flat stream of
  ``Enter``/``Exit`` markers, tokens, and errors in source order. You
  build whatever structure you want from that stream.
* **Recoverable.** On an unexpected token the parser synthesizes an
  error event, skips forward to a synchronization point computed from
  the enclosing rule's ``FOLLOW`` set, and keeps going. A file with a
  hundred errors still produces a usable stream.
* **Multi-target.** The grammar is compiled to a language-agnostic state
  table; each backend is a pure function from that table to a file bundle.
  Rust, Python, TypeScript, Go, Java, C#, and C are supported today.

Design principles
-----------------

* **One grammar, one parser.** The DSL and the runtime were designed
  together. The DSL deliberately omits features that would compromise
  LL(k) (left recursion, context-dependent lexing, semantic predicates)
  and in return every grammar that passes the analyzer gets a
  deterministic, table-driven parser.
* **Explicit tokens.** The grammar writer lists every token. The lexer
  is a single DFA built from those declarations; there is no implicit
  tokenization of string literals in rules.
* **Stable tree shape.** The generated parse tree is the one your grammar
  describes. Fragment rules (``_name``) are inlined with no trace, so you
  can factor grammars without perturbing the structural events downstream
  consumers see.
* **No runtime code generation.** All work happens at generator time.
  Generated parsers are plain source files — review them, read them, ship
  them.

Where to go next
----------------

If you want to write a grammar, start at :doc:`grammar_language`. If you
already have one and want to generate code, jump to :doc:`usage`. If you
are integrating the generated parser into an application, the event
model lives at :doc:`event_model`. To understand how the generator turns
a grammar into code, read through the :doc:`pipeline/index` in order.
