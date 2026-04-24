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
  ``python``, ``typescript``, ``go``, ``java``, ``csharp``, ``c``,
  and the meta-target ``all`` which emits every backend. With
  ``-o OUT``, files are written under that directory (one
  sub-directory per backend when multiple are emitted). Without
  ``-o``, files are written into the current working directory.

``tree-sitter [-o OUT]``
  Emit a tree-sitter ``grammar.js`` for editor tooling. The emitted
  grammar is purely declarative; it does not share the pull-parser
  runtime. Useful for syntax highlighting and code folding in
  editors that speak tree-sitter.

``debug <sub>``
  Dump internal state. The sub-commands are ``stats``, ``tokens``,
  ``rules --format tree|dot``, ``analysis``, ``lowering``, and
  ``dfa [--full] [--format plain|dot]``. Use ``rules --format dot``
  piped into Graphviz to view rule railroad diagrams; use ``dfa
  --format dot`` for the lexer DFA. These dumps are intended as a
  debugging aid while developing a grammar — the :doc:`pipeline/index`
  describes each layer in full.

The ``--name NAME`` option, accepted at any position, overrides the
identifier the backend uses for file and package names. By default
the name is the grammar file's stem (``foo.parsuna`` → ``foo``).

The shape of a generated parser
-------------------------------

Every backend produces the same five things, spelled in the idioms of
the target language:

* A **TokenKind** enumeration with one variant per declared token,
  plus the reserved ``EOF`` and ``ERROR`` sentinels. Skip tokens
  appear here like any other token; fragments do not.
* A **RuleKind** enumeration with one variant per non-fragment rule.
  Attached to every structural event so consumers can identify
  subtrees.
* A **parse_<rule>** entry point per non-fragment rule, accepting a
  source string or (where the target runtime supports it) a stream.
  The entry point returns a **Parser** object — the generated driver
  wrapped around the runtime's pull loop.
* The **Parser** object, which yields **Event** values one at a time.
  Every target spells this as its native iterator protocol
  (``Iterator`` in Rust, ``Iterable`` in Python, ``Iterator<T>`` in
  TypeScript, a ``NextEvent`` method in Go, etc.).
* **Event** itself: a tagged union with four cases (``Enter``,
  ``Exit``, ``Token``, ``Error``). See :doc:`event_model` for the
  full payload.

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
            case "error":
                on_error(event.error)

Two rules to keep in mind while writing the driver:

1. **Events are final in source order.** The parser never retracts or
   reorders events; once you have seen one, it will not be un-emitted.
2. **Error events do not stop the stream.** The parser recovers and
   keeps going. An application that wants to abort on the first error
   must do so in its own driver — the parser will happily continue.

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
   leaves; handle ``Error`` events by attaching a diagnostic to the
   surrounding construct.

Regenerating is cheap and should be fully automated — wire
``parsuna generate`` into your build system so the committed files
never drift from the grammar.

Tokens, skips, and whitespace
-----------------------------

Skip tokens (``?WS``, ``?COMMENT``) are re-attached to the event
stream just before the next structural event that follows them in
source order. Consumers who only care about structure can filter by
event tag; consumers building a formatter or a highlighter see the
skips in the correct positions.

``Error`` events do not consume the token they attach to — the parser
still either consumes it (if recovery synchronizes on it) or skips it
as part of recovery. Application code should treat ``Error`` as a
diagnostic carrier, not a replacement for a token.

Interpreting token text
-----------------------

The parser does not post-process token text. ``STRING`` tokens are
delivered with their quotes and escapes intact; ``NUMBER`` tokens are
delivered as the raw lexeme. Un-escaping and numeric conversion are
the consumer's job — this keeps the parser's source text faithful so
tools like formatters and go-to-definition work without losing
information.
