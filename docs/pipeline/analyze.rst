Pass 2 — Analysis
=================

Entry point: ``analysis::analyze`` (``src/analysis/mod.rs``).
Output: ``AnalysisOutcome``, containing an optional ``AnalyzedGrammar``
and a flat bag of diagnostics produced along the way.

Analysis runs in three sub-passes: structural validation, post-validation
lints, and iterative FIRST/FOLLOW computation. The first two only emit
diagnostics; the third additionally produces the FIRST/FOLLOW tables that
later passes consume.

Diagnostics
-----------

Every check pushes into a single ``Vec<Diagnostic>``. A
``Diagnostic`` carries a severity (``Error`` or ``Warning``), a
message, and a source span. Errors stop the build; warnings are
surfaced but the build still succeeds unless the caller passes
``--warnings=errors``, which promotes every warning at print time.

Parser-level errors (``parsuna_rt::Error`` values produced by the
generated grammar parser) are converted into error-severity diagnostics
at the boundary; analysis itself only ever pushes ``Diagnostic`` values
directly.

Sub-pass 2a — Structural validation
-----------------------------------

File: ``src/analysis/validate.rs``.

Before any set computation, the grammar is checked for structural
problems. These checks do not need FIRST/FOLLOW; catching them first
means the later, more expensive fixed-point computations never run
on a malformed input.

Checks, in order:

* **Reserved token names.** ``EOF`` and ``ERROR`` (case-insensitive)
  are rejected. ``EOF`` is the end-of-input sentinel (kind id ``0``);
  ``ERROR`` is reserved so user grammars can't shadow the runtime's
  diagnostic vocabulary. Lex failures are surfaced via
  ``Token { kind: None }`` rather than a reserved kind id.
* **Duplicates.** Token names and rule names are checked for
  duplicates independently.
* **Undefined references.** Every ``Expr::Token(n)`` must name a
  declared token; every ``Expr::Rule(n)`` must name a declared rule;
  every ``TokenPattern::Ref(n)`` must name a declared token. Rules
  that reference fragment tokens by name are rejected — fragments are
  only usable inside other token patterns.
* **Token reference cycles.** ``A`` referencing ``B`` referencing ``A``
  would loop forever at lowering time. A DFS over the token-ref graph
  reports the cycle path and aborts.
* **Left recursion.** For each rule, compute the set of rules that
  can appear at its first-position (skipping rules whose body is
  guaranteed nullable — ``Empty``, ``Opt``, ``Star``). If the rule
  can reach itself, it is left-recursive. The check is purely
  structural; an accurate FIRST-based check comes essentially for
  free in sub-pass 2c but reporting here gives a better error.
* **Non-empty public surface.** A grammar with no non-fragment rules
  produces an empty public API; it is rejected.

If any error-severity diagnostics land in the bag here, analysis
returns immediately without proceeding to lints or FIRST/FOLLOW —
the later passes assume references resolve and there are no cycles,
so running them on a structurally broken grammar would be unsafe.

Sub-pass 2b — Post-validation lints
-----------------------------------

Files: ``src/analysis/lints.rs`` and ``src/analysis/shadow.rs``.

Once validation has cleared, four additional checks look for
grammars that *would* compile but shouldn't. Each picks its own
severity.

* **Empty-match tokens.** A token whose pattern can match the empty
  string (``T = 'a'?;``, ``T = 'x'*;``, ``T = "";``) would let the
  lexer accept at length zero — either an infinite loop or a stream
  of zero-length tokens. Detected by a recursive nullability walk
  over the resolved pattern; reported as **error**.
* **Unused fragments.** Fragment tokens (``_HEX``) and fragment
  rules (``_postfix``) that no other declaration references are dead
  code. Reachability is computed by BFS from the live seeds —
  non-fragment tokens, non-fragment rules, and skip tokens — over the
  reference graph. Reported as **warning**, since the grammar still
  produces a working parser.
* **Non-productive rules.** A rule with no derivation that
  terminates in tokens (``r = R r+;``, mutually-recursive cycles
  with no terminal escape) would never let the parser accept any
  input. Detected by a least-fixed-point over rules that reduce to
  terminals or to other productive rules; reported as **error**.
* **Literal-token shadowing.** A ``Literal``-pattern token (e.g.
  ``IF = "if";``) declared *after* a more general token whose
  pattern also accepts that literal (e.g. ``IDENT = ('a'..'z')+;``)
  is unreachable: the lexer breaks ties by smallest token id (=
  declaration order), so the earlier token always wins. Detected by
  running a small NFA-style pattern matcher over each candidate
  shadower; reported as **error**.

The lint pass and the shadow pass each push into the same diagnostic
bag. After both have run, if any error-severity diagnostics are
present analysis returns; warnings alone do not stop the build.

Sub-pass 2c — Iterative FIRST/FOLLOW at the smallest viable k
-------------------------------------------------------------

File: ``src/analysis/first_follow.rs``. The entry point used by
``analyze`` is ``solve_lookahead``; FIRST/FOLLOW computation,
conflict detection, and the iterative deepening loop all live
together in this module.

The driver algorithm is **iterative deepening on k**:

.. parsed-literal::

    k = 0
    loop:
        k += 1
        (nullable, first)   = compute_first(g, k)
        follow_k            = compute_follow_k(g, first, nullable, k)
        conflicts           = detect_conflicts(g, nullable, first, follow_k, k)
        if conflicts empty:
            commit (nullable, first, k); break
        if conflict count has not decreased for STUCK_LIMIT rounds:
            give up — grammar is not LL(k) for any finite k

``STUCK_LIMIT`` is a small constant (three at the time of writing)
that protects against genuinely ambiguous grammars without giving up
too early on ones that slowly converge.

``compute_first`` is a standard least-fixed-point computation lifted
to FIRST(k): instead of a set of single tokens, each rule's FIRST is
a set of length-≤-k **lookahead sequences**. ``nullable`` is the
classic Boolean fixed point ("does the rule derive ε?"). The
``concat_k`` helper, used pervasively, concatenates two FIRST-sets
with truncation to length ``k``.

``compute_follow_k`` is the FIRST(k)-style follow: for each rule
``A``, the set of length-≤-k sequences that can legally appear after
an occurrence of ``A``. End-of-input is represented by the placeholder
token ``$EOF``.

Conflict detection
------------------

For every ``Alt`` node, ``detect_conflicts`` computes each arm's
**prediction set** — its FIRST extended by the tail that follows the
alternation — and then checks pairs of arms for prefix overlap. If
arm *i* predicts ``[a b]`` and arm *j* predicts ``[a]``, an input
starting with ``a c`` is fine for *j* but ambiguous for any
``a b …``, so the pair is flagged.

The walker is structural: for a ``Seq(xs)`` node it folds the tail
right-to-left so each child is checked with the correct forward-
looking context; for ``Star(x)`` / ``Plus(x)`` the tail concatenates
``FIRST(x)*`` so an iteration's lookahead includes the possibility
of another iteration; for ``Opt(x)`` the tail flows through
unchanged.

When conflicts remain at the final ``k`` and the number of conflict
sequences has not dropped for several rounds, the grammar is
reported as "not LL(k) for any finite k", each remaining conflict
listing the rule, the arm indices, and up to three sample ambiguous
prefixes.

The AnalyzedGrammar
-------------------

When analysis succeeds it produces:

* ``grammar`` — the original ``Grammar``, verbatim. Downstream passes
  reach through it for the raw token and rule definitions.
* ``first: BTreeMap<String, FirstSet>`` — FIRST(k) per rule,
  deduplicated. Sequences are stored as ``Vec<String>`` of token
  names (not ids — those come later).
* ``follow: BTreeMap<String, FollowSet>`` — classic single-token
  FOLLOW per rule. Used later as a recovery synchronization base.
* ``follow_k: BTreeMap<String, FirstSet>`` — FIRST(k)-style follow.
  Used to compute accurate prediction sets for nullable arms at
  lowering time.
* ``nullable: BTreeMap<String, bool>`` — per-rule nullability.
* ``k: usize`` — the smallest ``k`` for which no alternative is
  ambiguous. This is the LL(k) of the whole grammar; the value flows
  through to the runtime as a compile-time constant that sizes the
  parser's lookahead ring.

Why both ``follow`` and ``follow_k``
------------------------------------

They serve different purposes:

* ``follow`` is used by lowering to compute **SYNC sets**, the
  single-token sets the parser recovers to on an ``Expect``
  mismatch. Recovery needs to stop at the earliest familiar token,
  which means a one-token horizon.
* ``follow_k`` is used to extend per-arm prediction sets at
  dispatch sites, so nullable arms can still be chosen when the next
  ``k`` tokens match the surrounding context.

Output
------

``AnalysisOutcome { grammar: Option<AnalyzedGrammar>, diagnostics: Vec<Diagnostic> }``.
The grammar is ``Some`` iff no diagnostic in ``diagnostics`` has
``Severity::Error``; warnings can accompany a successful outcome.
``AnalysisOutcome::has_errors`` is the standard discriminator.

On success the grammar is handed to :doc:`lower`. Either way, the
caller iterates ``diagnostics`` for printing — under
``--warnings=errors`` warnings are re-rendered with the ``error``
label and the build fails even if ``grammar`` was produced.
