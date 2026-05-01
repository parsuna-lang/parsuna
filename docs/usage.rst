Using parsuna
=============

This page is language-agnostic: it covers the generator CLI, the shape
of a generated parser, and how consumers drive one. Backend specifics
are called out only when they matter.

The CLI
-------

The parsuna executable takes a grammar file plus a subcommand::

    parsuna <grammar.parsuna> <subcommand> [options]

The useful subcommands for day-to-day work are:

``check``
  Load, parse, and analyze the grammar. Print a one-line summary
  (``grammar `NAME' OK: N tokens, M rules, LL(k)``) and exit 0, or
  print diagnostics and exit non-zero. Use this as a pre-commit or
  CI gate.

``generate <target> [-o OUT]``
  Emit a parser for ``<target>``. Valid targets are ``rust``,
  ``python``, ``typescript``, ``go``, ``java``, ``csharp``, and
  ``c``. With ``-o OUT``, files are written under that directory;
  without ``-o``, they go in the current working directory.
  Per-backend flags exist for targets that need them — ``java
  --package com.example.foo`` controls the Java package and the
  on-disk directory layout, ``csharp --namespace MyApp.Parser`` does
  the same for C#.

``tree-sitter [-o OUT]``
  Emit a tree-sitter ``grammar.js`` for editor tooling. The emitted
  grammar is purely declarative; it does not share the pull-parser
  runtime. Useful for syntax highlighting and code folding in
  editors that speak tree-sitter.

``debug <sub>``
  Dump internal state. The sub-commands are ``stats``, ``tokens``,
  ``rules --format tree|dot``, ``analysis``, ``lowering``, and
  ``dfa --format plain|dot``. Use ``rules --format dot`` piped into
  Graphviz to view rule railroad diagrams; use ``dfa --format dot``
  for the lexer DFA. These dumps are intended as a debugging aid
  while developing a grammar — the :doc:`pipeline/index` describes
  each layer in full.

Two global options work on any subcommand:

* ``--name NAME`` — overrides the identifier the backend uses for
  file and package names. By default the name is the grammar file's
  stem (``foo.parsuna`` → ``foo``).
* ``--warnings warn|error`` — promotes lint warnings (unused
  fragments, etc.) to errors when set to ``error``. Default is
  ``warn``: warnings print but the build still succeeds.

Optimizer toggles
-----------------

Five global flags disable individual lowering / DFA passes. They
are pure performance switches — turning any of them off still
produces a working parser, just with a larger state table or DFA:

* ``--no-inline-jumps`` — leave each block-level op in its own state
  instead of absorbing trailing ``Jump(N)`` chains into the body.
* ``--no-fold-trampolines`` — keep pure-``Ret`` states alive instead
  of dropping ``PushRet`` references and rewriting branchy
  continuations to tail-call form.
* ``--no-inline-branch-bodies`` — leave a ``[Jump(s)]``-shaped
  branch body alone instead of replacing it with ``s``'s body
  directly.
* ``--no-eliminate-dead`` — keep states no entry can reach in the
  emitted output. The generated parser still works (those states
  are never entered), but the emitted source is bigger.
* ``--no-dfa-minimize`` — skip lexer-DFA partition-refinement
  minimization. Keeps the raw subset-construction output, which
  has many duplicate near-identical states.

These are mainly useful for diffing the lowered table against a
known-good baseline when something looks wrong; in production builds
the defaults are what you want.

The shape of a generated parser
-------------------------------

Every backend produces the same five things, spelled in the idioms of
the target language:

* A **TokenKind** enumeration with one variant per declared token,
  plus the reserved ``EOF`` sentinel (kind id ``0``). Lex failures are
  not a TokenKind variant — see :doc:`event_model` for the per-language
  representation. Skip tokens appear in the enum like any other token;
  fragments do not.
* A **RuleKind** enumeration with one variant per non-fragment rule.
  Attached to every structural event so consumers can identify
  subtrees.
* A **parse_<rule>** entry point per non-fragment rule, accepting a
  source string or (where the target runtime supports it) a stream.
  The entry point returns a **Parser** object — the runtime's
  pull-loop iterator over the generated state machine.
* The **Parser** object, which yields **Event** values one at a time.
  Every target spells this as its native iterator protocol
  (``Iterator`` in Rust, ``Iterable`` in Python, ``Iterator<T>`` in
  TypeScript, a ``Next() (Event, bool)`` method in Go, etc.).
  Internal dispatch hooks (lookahead access, return-stack
  manipulation, event builders) are *not* on the Parser — they live
  on a sealed ``Cursor`` type that only the runtime's pull loop ever
  constructs, so you can't accidentally poke at parser state from
  the outside.
* **Event** itself: a tagged union with five cases (``Enter``,
  ``Exit``, ``Token``, ``Garbage``, ``Error``). See
  :doc:`event_model` for the full payload.

All of these come from the same state table, so whatever backend you
pick, the sequence of events you observe for a given input is the
same up to language-level encoding differences.

A minimal driver
----------------

The pattern is identical in every language: call the entry point,
iterate, switch on the event tag. In pseudocode::

    parser = parse_<rule>(source)
    for event in parser:
        match event.tag:
            case "enter":  # event.rule is a RuleKind
                on_enter(event.rule, event.pos)
            case "exit":
                on_exit(event.rule, event.pos)
            case "token":  # event.token.kind is a TokenKind
                on_token(event.token)
            case "garbage":  # token consumed by error recovery
                on_garbage(event.token)
            case "error":
                on_error(event.error)

Three things to keep in mind while writing the driver:

1. **Events are final in source order.** The parser never retracts or
   reorders events; once you have seen one, it will not be un-emitted.
2. **Error events do not stop the stream.** The parser recovers and
   keeps going. An application that wants to abort on the first error
   must do so in its own driver — the parser will happily continue.
3. **Garbage is distinct from Token.** Tokens consumed during error
   recovery come through tagged ``garbage`` so AST builders can drop
   them (or mark them as error spans) without tracking recovery state
   externally. A normal ``Token`` event is always legitimate parse
   data — including the "synced-to-expected" token after a recovery.

Skip emission policy
--------------------

By default, skip-kind tokens (whitespace, comments — anything declared
with ``-> skip``) are interleaved into the event stream as ``Token``
events between structural events. Consumers building a formatter or
highlighter want them; consumers building an AST typically don't.

Every backend exposes a way to turn skip emission off at the source:

* **Rust** — pass a compile-time config: ``parse_foo_from_str_with::<DropSkips>(src)``.
  ``EmitSkips`` is the default. The choice is a zero-sized type
  parameter, so monomorphization removes the skip-emit branch
  entirely when you pick ``DropSkips``.
* **Go / Java / C# / TypeScript** — pass a runtime ``Options`` (or
  ``ParserOptions``) with the ``DropSkips`` flag set when constructing
  the parser. Most bindings provide a ``parse_*_with_options``
  variant alongside the default ``parse_*`` constructor.
* **C / Python** — currently emit-only; if you need to drop skips,
  filter them out in the consumer.

The structural event stream is identical either way — only ``Token``
events for skip kinds are affected.

Starting from a rule other than the default
-------------------------------------------

Every non-fragment rule has an entry point. The first rule declared
is the *default start*, but nothing stops you from calling
``parse_member`` or ``parse_number`` directly to parse a fragment of
input as if that rule were the top. This is useful for tests, for
editor tooling that parses at the cursor, and for composing parsers
(parse a request body with one entry, then parse its contents with
another).

Typical integration workflow
----------------------------

1. Write the grammar in a ``.parsuna`` file.
2. Run ``parsuna grammar.parsuna check`` until it reports OK. Fix
   undefined references, left recursion, or LL(k) conflicts as the
   checker reports them.
3. Run ``parsuna grammar.parsuna generate <target> -o src``. Commit
   the emitted files into your repository — they are plain source,
   and diffing them is how you notice grammar changes you did not
   intend.
4. In your application, call ``parse_<rule>`` and walk the event
   stream. Translate ``Enter``/``Exit`` pairs into whatever
   domain-specific tree you want; translate ``Token`` events into
   leaves; handle ``Error`` (and ``Garbage``) events by attaching a
   diagnostic to the surrounding construct.

Regenerating is cheap and should be fully automated — wire
``parsuna generate`` into your build system so the committed files
never drift from the grammar.

Tokens, skips, and whitespace
-----------------------------

Skip tokens (those declared with the ``-> skip`` action, such as
``WS`` and ``COMMENT``) are interleaved into the event stream just
before the next structural event that follows them in source order.
Consumers who only care about structure can either filter by kind
on the consuming side or pick the drop-skips option (above) and
have them silently consumed at the source.

``Error`` events do not consume the token they attach to — the parser
still either consumes it (if recovery synchronizes on it, in which
case it comes through as a ``Token``) or skips it as part of recovery
(``Garbage``). Application code should treat ``Error`` as a
diagnostic carrier, not a replacement for a token.

Interpreting token text
-----------------------

The parser does not post-process token text. ``STRING`` tokens are
delivered with their quotes and escapes intact; ``NUMBER`` tokens are
delivered as the raw lexeme. Un-escaping and numeric conversion are
the consumer's job — this keeps the parser's source text faithful so
tools like formatters and go-to-definition work without losing
information.
