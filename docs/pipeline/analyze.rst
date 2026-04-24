Pass 2 — Analysis
=================

Entry point: ``analysis::analyze`` (``src/analysis/mod.rs``).
Output: ``AnalyzedGrammar``, containing the original ``Grammar``
plus the sets and metadata the lowering and code-generation passes
need.

Analysis is split into two sub-passes: structural validation, and
iterative FIRST/FOLLOW computation.

Sub-pass 2a — Structural validation
-----------------------------------

File: ``src/analysis/validate.rs``.

Before any set computation, the grammar is checked for structural
problems. These checks do not need FIRST/FOLLOW; catching them first
means the later, more expensive fixed-point computations never run
on a malformed input.

Checks, in order:

* **Reserved token names.** ``EOF`` and ``ERROR`` (case-insensitive)
  are rejected — they collide with runtime sentinels.
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
  free in sub-pass 2b but reporting here gives a better error.
* **Non-empty public surface.** A grammar with no non-fragment rules
  produces an empty public API; it is rejected.

Failures are collected into a ``Vec<Error>``; if any are present,
analysis returns them without proceeding.

Sub-pass 2b — Iterative FIRST/FOLLOW at the smallest viable k
-------------------------------------------------------------

File: ``src/analysis/first_follow.rs`` plus the outer loop in
``src/analysis/mod.rs``.

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

``Result<AnalyzedGrammar, Vec<Error>>``. On success, the analyzed
grammar is handed to :doc:`lower`.
