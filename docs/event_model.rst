The event model
================

Generated parsers do not materialize a parse tree. They emit a flat
sequence of **events**, and a program that consumes events can
reconstruct whatever tree (or no tree at all) it needs. This page
specifies the event stream — the contract every backend implements.

The five events
---------------

Every event is one of:

``Enter``
  Opens the subtree of a rule. Carries the rule's ``RuleKind`` and a
  ``Pos`` marking the start of the subtree (the position of its first
  child, or the position of its matching ``Exit`` for an empty rule).
  Only non-fragment rules produce this event — fragment rules are
  inlined without any ``Enter``/``Exit`` markers.

``Exit``
  Closes the matching ``Enter``. Carries the same ``RuleKind`` and a
  ``Pos`` marking the end of the subtree (the position just past the
  last consumed token of the rule's content, equal to the enter
  position if nothing was consumed).

``Token``
  A lexed token consumed from the input. Carries:

  * ``kind`` — a ``TokenKind`` value identifying which token
    declaration this matches, or a nullable / sentinel value when the
    lexer failed to match at the current position; see below.
  * ``span`` — a ``Span`` covering the matched input.
  * ``text`` — the matched source text, exactly as it appeared.
    Un-escaping, numeric conversion, and other transforms are not
    performed by the parser.

  ``Token`` events always carry legitimate parse data, including the
  "synced-to-expected" token after a recovery (when an ``expect``
  mismatched and the recovery's sync set landed on the kind it was
  expecting).

``Garbage``
  A token consumed by error recovery — emitted between an ``Error``
  and the recovery's sync point. Carries the same payload shape as
  ``Token`` (kind, span, text), but is distinct so consumers can
  drop these from their AST or render them as error spans without
  tracking recovery state externally.

``Error``
  A recoverable diagnostic. Carries a human-readable message and a
  ``Span`` pointing at the offending lookahead. The parser continues
  emitting events after an error, so a file with many errors still
  yields a useful stream.

Every backend names these five cases the same way in its idiomatic
tagged-union form — in TypeScript they are ``{tag: "enter" | "exit"
| "token" | "garbage" | "error", ...}``; in Rust they are
``Event::Enter { .. }`` / ``Event::Exit { .. }`` / ``Event::Token(..)``
/ ``Event::Garbage(..)`` / ``Event::Error(..)``; in Python they are
``Event`` objects with a ``.tag`` string attribute; in Go they are
distinguished by an ``EventTag`` constant (``EvEnter``, ``EvExit``,
``EvToken``, ``EvGarbage``, ``EvError``); in C# they are sealed
records (``EnterEvent``, ``ExitEvent``, ``TokenEvent``,
``GarbageEvent``, ``ErrorEvent``); in Java they are sealed
sub-classes of ``Event``; in C they are ``EventTag`` constants
(``EV_ENTER``, ``EV_EXIT``, ``EV_TOKEN``, ``EV_GARBAGE``, ``EV_ERROR``).

Ordering guarantees
-------------------

* **Source order.** Events are emitted in the order their source
  bytes appear. Skip tokens (see below) are interleaved with
  structural events accordingly.
* **Balanced structure.** Every ``Enter`` is matched by exactly one
  ``Exit`` for the same ``RuleKind``. Errors or recovery do not cause
  unmatched ``Enter``/``Exit`` pairs — if the parser commits to a
  rule, it finishes the rule.
* **Finality.** Events are never retracted or reordered. A consumer
  can commit to a side-effect on each event as it arrives.
* **Termination.** The stream ends when the parser reaches the end of
  input. If there are trailing bytes after the start rule completes,
  the parser emits an "expected end of input" error and consumes the
  remaining tokens (as ``Garbage``) before terminating.

Building a tree from events
---------------------------

The canonical consumer keeps a stack: push a new node on ``Enter``,
attach tokens as children of the top-of-stack node, and pop on
``Exit``. In pseudocode::

    stack = [root]
    for ev in parser:
        match ev.tag:
            case "enter":
                node = make_node(ev.rule)
                stack[-1].children.append(node)
                stack.push(node)
            case "token":
                stack[-1].children.append(ev.token)
            case "exit":
                stack.pop()
            case "garbage":
                # token consumed by recovery — typically dropped, or
                # collected on the side as an error span
                continue
            case "error":
                errors.append(ev.error)

This is the direct, mechanical translation — consumers that want a
typed AST typically switch on ``ev.rule`` inside ``enter`` to pick
the right node type, and switch on ``ev.token.kind`` inside
``token`` to decode the leaf.

Skip tokens
-----------

Tokens declared with the ``-> skip`` action (whitespace, comments)
are **skips**. The parser's state machine does not see them — they are
never consumed by ``Expect`` or examined by lookahead. The runtime
re-inserts them into the event stream just before the next structural
event, so consumers that want trivia (formatters, highlighters) see
skips in their correct source position by default.

Consumers that don't want skips can opt into drop-skips mode at
parser construction (a compile-time ``ParserConfig`` in Rust, a
runtime ``Options`` flag in the other backends — see :doc:`usage`).
With drop-skips on, the lexer still matches skip tokens (they
delimit structural ones), but the parser silently consumes them
instead of yielding them as ``Token`` events. The structural event
stream is unchanged either way.

The ``Pos`` and ``Span`` types
------------------------------

Every backend exposes the same two shapes:

``Pos``
  ``{offset, line, column}``. ``offset`` is a 0-based byte offset
  into the source. ``line`` is 1-based. ``column`` is 1-based and
  counted in Unicode codepoints within the line (not bytes, not
  grapheme clusters).

``Span``
  A half-open ``[start, end)`` pair of ``Pos`` values. ``span.start
  == span.end`` denotes a zero-width span at a point — used, for
  example, for the ``Enter`` of an empty rule.

EOF and lex failures
--------------------

``EOF`` is reserved as a token-kind name — a grammar that declares a
token called ``EOF`` is rejected. ``ERROR`` is **not** reserved: you
can declare a token called ``ERROR`` if you like.

``EOF`` (kind id ``0``)
  Emitted once by the lexer when the input is exhausted. The parser
  consumes it internally; consumers typically do not see an ``EOF``
  token, but may see one inside ``Token`` events during error
  recovery in pathological cases.

Lex failures (no token pattern matches at the current position) are
not represented as a separate token kind in the grammar's enum. The
lexer emits a normal ``Token`` event covering one codepoint with the
``kind`` field set to the language's "no kind" value — ``None`` in
Rust, ``null`` in TypeScript, ``Optional[int]`` ``None`` in Python,
and the unsigned sentinel ``0xFFFF`` in Go, Java, C#, and C — so the
parser can surface an error and keep making progress. The offending
position will also produce a nearby ``Error`` event explaining what
was expected.

Error recovery, observably
--------------------------

When the parser commits to a rule that wants a particular kind and
the lookahead doesn't match, three things happen, observable as
events:

1. An ``Error`` event is emitted with a message like ``"expected X"``
   and a span over the current lookahead.
2. The parser switches into recovery mode — it consumes tokens until
   the lookahead matches a token in the enclosing rule's
   synchronization set (essentially that rule's ``FOLLOW`` plus
   ``EOF``). Each token consumed during recovery comes through as a
   ``Garbage`` event, one per call to the iterator, so consumers
   stay in lock-step with input even on long error runs.
3. Once the lookahead lands on a sync token, recovery finalises. If
   the synced token happens to be the kind the rule was expecting,
   it comes through as a normal ``Token`` event (because it *is*
   legitimate parse data). Otherwise recovery just clears the armed
   state and the rule's surrounding flow resumes — the sync token
   is not consumed; the next iteration sees it via the regular
   structural events.

This means a parse of a broken file produces a stream where every
input byte is accounted for: some as well-formed ``Token`` events,
some as ``Garbage`` followed by ``Token`` once recovery synced, and
each error position carries its own ``Error`` event. An editor or
linter consuming the stream can highlight error spans without losing
track of the surrounding structure.
