The grammar language
====================

A parsuna grammar is a sequence of **declarations** in a ``.parsuna``
file. Each declaration defines either a **token** (matched by a regular
pattern over characters) or a **rule** (an LL expression over tokens
and other rules). Whitespace and ``//`` line comments between
declarations are ignored.

Declarations
------------

Every declaration has the form::

    name = body [annot, ...] ;

The annotation block is optional.

Whether the declaration is a token or a rule is determined by the case
of the first letter of ``name`` (skipping a leading ``_``):

* An **uppercase** initial makes it a token: ``IDENT``, ``STRING``,
  ``_HEX_DIGIT``.
* A **lowercase** initial makes it a rule: ``expr``, ``statement``,
  ``_parenthesized``.

A trailing ``[name, ...]`` block attaches **annotations** to the
declaration. The only annotation recognized today is ``skip``, which
marks a token as a **skip token**: the lexer still matches it, but the
parser drops it from the structural event stream. Skip tokens are
still delivered to consumers as events — they just appear outside any
``Enter``/``Exit`` scope, interleaved with structure in source order.
``skip`` only applies to tokens; ``[skip]`` on a rule is an error, and
unknown annotation names are rejected.

A leading ``_`` marks a **fragment**. Fragment tokens can be referenced
from other token bodies but are not themselves produced at runtime;
they are inlined into their callers before the lexer DFA is built.
Fragment rules are inlined the same way — a fragment rule emits no
``Enter``/``Exit`` event and is not part of the public parser API.
Combining ``_`` with ``[skip]`` on the same token is rejected.

Token patterns
--------------

A token body is a regular expression over characters. The atoms are:

* ``"abc"`` — a string literal, matches exactly those bytes.
* ``'a'`` — a character literal, matches one codepoint.
* ``'a'..'z'`` — a character range, matches any codepoint in the
  inclusive range.
* ``.`` — matches any codepoint.
* ``!x`` — the negation of character atom ``x`` (or a list of atoms in
  parentheses separated by ``|``). For example, ``!('"' | '\n')``
  matches any codepoint that is neither ``"`` nor a newline.
* ``NAME`` — reference to another token (usually a fragment).

Operators, from tightest to loosest binding:

* ``x?`` — zero or one ``x``.
* ``x*`` — zero or more ``x``.
* ``x+`` — one or more ``x``.
* Juxtaposition ``x y`` — concatenation.
* ``x | y`` — alternation.

Parentheses group.

Escapes inside string and char literals follow a small fixed set:
``\n``, ``\r``, ``\t``, ``\0``, ``\\``, ``\'``, ``\"``, and
``\u{HHHH}`` for arbitrary Unicode codepoints (1–6 hex digits).

Example: declaring identifier, integer, and whitespace tokens::

    IDENT  = ('A'..'Z' | 'a'..'z' | '_') ('A'..'Z' | 'a'..'z' | '_' | '0'..'9')*;
    INT    = ('0'..'9')+;
    WS     = (' ' | '\t' | '\r' | '\n')+ [skip];

The name ``EOF`` is reserved as a token-kind name; the runtime emits it
as the end-of-input sentinel (kind id ``0``).

Rule expressions
----------------

A rule body is an LL expression over tokens and rules. The atoms are:

* ``NAME`` where ``NAME`` starts with an uppercase letter —
  consume one token of that kind.
* ``name`` where ``name`` starts with a lowercase letter — recursively
  parse that rule.

Operators mirror the token language:

* ``x?``, ``x*``, ``x+`` — repetition.
* Juxtaposition ``x y`` — concatenation.
* ``x | y`` — alternation; the analyzer picks the arm using up to
  ``k`` tokens of lookahead.

Parentheses group.

String literals (``"..."``) and character atoms (``'a'``, ``.``, ``!``)
are **not** valid inside a rule — rules refer to tokens by name only.
This keeps tokenization decisions in one place so the lexer stays a
single DFA.

Example: a fragment of a JSON grammar, where ``value`` is the start
rule and ``member`` is factored out::

    value  = object | array | string | number | bool | null;

    object = LBRACE (member (COMMA member)*)? RBRACE;
    array  = LBRACK (value  (COMMA value )*)? RBRACK;
    member = key COLON value;
    key    = STRING;

    string = STRING;
    number = NUMBER;
    bool   = TRUE | FALSE;
    null   = NULL;

What the grammar cannot express
-------------------------------

Some constructs are deliberately rejected:

* **Left recursion.** ``expr = expr PLUS term | term`` is not a valid
  parsuna grammar. Rewrite with repetition::

      expr = term (PLUS term)*;

  Left recursion is detected as a structural check and reported before
  any analysis runs.
* **Ambiguity.** If two alternatives share a common prefix that no
  finite ``k`` can distinguish, the grammar is rejected with a
  conflict report naming the ambiguous prefix. The analyzer tries
  increasing values of ``k`` until either all conflicts vanish or the
  conflict count stops dropping for several rounds in a row.
* **Context-dependent lexing.** Every token matches the same way
  wherever it appears; there is no way to enable or disable a token
  based on parser state. Factor your grammar so that a single DFA can
  disambiguate tokens by their longest match.

Fragment rules and tokens
-------------------------

Fragments let you name a sub-pattern without it showing up in the
output. They are useful in two ways:

* **Readability.** Break a long token pattern into named pieces::

      _DIGIT  = '0'..'9';
      _FRAC   = "." _DIGIT+;
      _EXP    = ('e' | 'E') ('+' | '-')? _DIGIT+;
      NUMBER  = '-'? _DIGIT+ _FRAC? _EXP?;

  The fragments are inlined into ``NUMBER`` before the lexer DFA is
  built; they are not themselves token kinds at runtime.

* **Structure without noise.** Fragment rules factor common rule
  bodies without adding a nesting level to the event stream — a
  consumer that walks ``Enter``/``Exit`` events sees exactly the
  hierarchy your non-fragment rules describe.

Naming conventions and reserved names
-------------------------------------

* The first non-fragment rule declared in the file is the **default
  start rule**. Every non-fragment rule also becomes a public entry
  point in the generated parser, so the start choice is a default —
  consumers can begin parsing from any public rule.
* A grammar must declare at least one non-fragment rule, otherwise
  nothing would be emitted.
* Token and rule name spaces are separate, but parsuna uses the case
  of the first letter of the name to decide which one you meant, so
  you cannot declare a token and a rule with names that differ only
  in case.
