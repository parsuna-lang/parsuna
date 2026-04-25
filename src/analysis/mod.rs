//! Semantic analysis of a parsed grammar.
//!
//! The main entry point is [`analyze`]: it validates the grammar, then
//! iteratively computes FIRST/FOLLOW for increasing `k` until every
//! alternative can be disambiguated by `k` tokens of lookahead (LL(k)).
//! If no finite `k` works within [`STUCK_LIMIT`] attempts without progress,
//! it reports the remaining conflicts as errors.

pub mod first_follow;
mod lints;
mod shadow;
mod validate;

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;
use crate::Span;

pub use first_follow::{FirstSet, FollowSet, Seq};

/// Result of [`analyze`]: the analyzed grammar (when analysis succeeded) and
/// every diagnostic produced along the way. `grammar` is `Some` iff no
/// diagnostic in `diagnostics` has [`crate::diagnostic::Severity::Error`].
#[derive(Debug)]
pub struct AnalysisOutcome {
    /// The analyzed grammar; present iff [`AnalysisOutcome::has_errors`]
    /// returns `false`.
    pub grammar: Option<AnalyzedGrammar>,
    /// Every diagnostic produced, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

impl AnalysisOutcome {
    /// True iff any diagnostic has error severity.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Placeholder name used inside FOLLOW sets to mean "end of input". Picked
/// so no real grammar token can collide with it.
pub const EOF_MARKER: &str = "$EOF";

/// How many rounds of raising `k` we tolerate with no reduction in the
/// number of conflicting sequences before giving up on the grammar. Keeps
/// analysis from looping forever on genuinely ambiguous grammars.
const STUCK_LIMIT: usize = 3;

/// A validated grammar together with the FIRST/FOLLOW tables needed to
/// generate its parser.
///
/// `first` is FIRST(k) for the chosen `k` (sequences up to `k` tokens long).
/// `follow` is the classic FOLLOW set (single-token, used for recovery sync
/// points). `follow_k` is FIRST(k)-style follow used by the dispatch logic.
/// `nullable[name]` tracks whether a rule derives the empty string.
#[derive(Clone, Debug)]
pub struct AnalyzedGrammar {
    /// The grammar this analysis ran over. Preserved verbatim so
    /// downstream phases can reach the original token/rule definitions.
    pub grammar: Grammar,
    /// `FIRST(k)` per rule name: every length-≤-`k` token-name prefix that
    /// can start an occurrence of the rule. The empty sequence marks a
    /// nullable rule.
    pub first: BTreeMap<String, FirstSet>,
    /// Classic single-token FOLLOW per rule name, used as recovery sync
    /// points. `EOF_MARKER` stands in for end-of-input.
    pub follow: BTreeMap<String, FollowSet>,
    /// `FIRST(k)`-style FOLLOW per rule name: every length-≤-`k` sequence
    /// that can legally follow an occurrence of the rule. Used by the
    /// dispatch logic to extend each arm's prediction with context.
    pub follow_k: BTreeMap<String, FirstSet>,
    /// Per-rule nullability: `true` iff the rule can derive ε.
    pub nullable: BTreeMap<String, bool>,
    /// The smallest `k` for which the grammar is LL(k) — i.e. for which
    /// every alternative can be disambiguated by `k` tokens of lookahead.
    pub k: usize,
}

/// Validate `g` and compute the analysis tables at the smallest `k` that
/// resolves all LL conflicts.
///
/// Strategy: raise `k` one at a time. At each `k`, compute FIRST and
/// FIRST(k)-based follow, then scan every `Alt` and `Seq` node for arms
/// whose prediction sets intersect. If no conflicts remain we take that
/// `k`; otherwise we keep going, tracking whether the overall conflict
/// count is dropping. After [`STUCK_LIMIT`] rounds with no improvement we
/// give up — the grammar is ambiguous in a way that more lookahead cannot
/// fix, and we report the last round's conflicts as errors.
pub fn analyze(g: Grammar) -> AnalysisOutcome {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let grammar = analyze_inner(g, &mut diags).ok();
    AnalysisOutcome {
        grammar,
        diagnostics: diags,
    }
}

/// Inner implementation: pushes diagnostics into `diags` and returns the
/// analyzed grammar on success, or `Err(())` if any error-severity
/// diagnostic was emitted (the actual messages live in `diags`).
fn analyze_inner(g: Grammar, diags: &mut Vec<Diagnostic>) -> Result<AnalyzedGrammar, ()> {
    validate::run(&g, diags);
    bail_on_errors(diags)?;

    lints::run(&g, diags);
    shadow::run(&g, diags);
    bail_on_errors(diags)?;

    let mut chosen: Option<(usize, BTreeMap<String, bool>, BTreeMap<String, FirstSet>)> = None;
    let mut last_conflicts: Vec<RawConflict> = Vec::new();
    let mut last_k;
    let mut min_size: Option<usize> = None;
    let mut stuck = 0usize;

    let mut k = 0usize;
    loop {
        k += 1;
        last_k = k;
        let (nullable, first) = first_follow::compute_first(&g, k);
        let follow_k = first_follow::compute_follow_k(&g, &first, &nullable, k);
        let conflicts = detect_conflicts(&g, &nullable, &first, &follow_k, k);
        if conflicts.is_empty() {
            chosen = Some((k, nullable, first));
            break;
        }
        let size: usize = conflicts.iter().map(|c| c.ambiguous.len()).sum();
        match min_size {
            Some(m) if size < m => {
                min_size = Some(size);
                stuck = 0;
            }
            Some(_) => stuck += 1,
            None => min_size = Some(size),
        }
        last_conflicts = conflicts;
        if stuck >= STUCK_LIMIT {
            break;
        }
    }

    let (k, nullable, first) = chosen.ok_or_else(|| {
        diags.push(Diagnostic::error(format!(
            "grammar is not LL(k) for any finite k: conflicts are stable \
             at k = {} (stopped iterating after no progress over {} rounds)",
            last_k, STUCK_LIMIT
        )));
        diags.extend(render_conflicts(&last_conflicts));
    })?;

    let follow = first_follow::compute_follow(&g, &first, &nullable);
    let follow_k = first_follow::compute_follow_k(&g, &first, &nullable, k);
    Ok(AnalyzedGrammar {
        grammar: g,
        first,
        follow,
        follow_k,
        nullable,
        k,
    })
}

/// `?` helper: short-circuit if any error-severity diagnostic is in the bag.
fn bail_on_errors(diags: &[Diagnostic]) -> Result<(), ()> {
    if diags.iter().any(Diagnostic::is_error) {
        Err(())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct RawConflict {
    rule_name: String,
    rule_span: Span,
    arm_i: usize,
    arm_j: usize,
    ambiguous: BTreeSet<Seq>,
}

fn render_conflicts(raws: &[RawConflict]) -> Vec<Diagnostic> {
    raws.iter()
        .map(|c| {
            let sample: Vec<String> = c.ambiguous.iter().take(3).map(|s| format_seq(s)).collect();
            Diagnostic::error(format!(
                "rule `{}`: alternatives {} and {} both predict on {}{}",
                c.rule_name,
                c.arm_i + 1,
                c.arm_j + 1,
                sample.join(", "),
                if c.ambiguous.len() > 3 { ", …" } else { "" }
            ))
            .at(c.rule_span)
        })
        .collect()
}

fn detect_conflicts(
    g: &Grammar,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    follow_k: &BTreeMap<String, FirstSet>,
    k: usize,
) -> Vec<RawConflict> {
    let mut out = Vec::new();
    for r in &g.rules {
        let tail = follow_k.get(&r.name).cloned().unwrap_or_default();
        walk_check_conflicts(
            &r.body, &tail, &r.name, r.span, nullable, first, k, &mut out,
        );
    }
    out
}

fn walk_check_conflicts(
    e: &Expr,
    tail: &FirstSet,
    rule_name: &str,
    rule_span: Span,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    k: usize,
    out: &mut Vec<RawConflict>,
) {
    match e {
        Expr::Empty | Expr::Token(_) | Expr::Rule(_) => {}
        Expr::Seq(xs) => {
            // Compute the "tail after position i" (succ[i+1] = tail after
            // xs[i]) by folding right-to-left: each element's lookahead is
            // its own FIRST concatenated with the tail that follows it.
            let n = xs.len();
            let mut succ: Vec<FirstSet> = vec![tail.clone(); n + 1];
            for i in (0..n).rev() {
                let fx = first_follow::first_of(&xs[i], nullable, first, k);
                succ[i] = first_follow::concat_k(&fx, &succ[i + 1], k);
            }
            for i in 0..n {
                walk_check_conflicts(
                    &xs[i],
                    &succ[i + 1],
                    rule_name,
                    rule_span,
                    nullable,
                    first,
                    k,
                    out,
                );
            }
        }
        Expr::Alt(xs) => {
            // Predict set for each arm: FIRST(arm) extended by the enclosing
            // tail. Nullable arms contribute their ε via the tail, so we
            // strip ε here and compare on the non-empty prefixes.
            let arms: Vec<(usize, FirstSet)> = xs
                .iter()
                .enumerate()
                .map(|(i, x)| {
                    let f = first_follow::first_of(x, nullable, first, k);
                    let predict = first_follow::concat_k(&f, tail, k);
                    let (non_eps, _) = first_follow::split_nullable(&predict);
                    (i, non_eps)
                })
                .collect();
            // Conflict detection: any prefix relation between two arms'
            // prediction sequences is ambiguous — if arm i predicts [a b]
            // and arm j predicts [a], seeing [a c] is fine for j but a
            // pure [a b …] input could match either.
            for i in 0..arms.len() {
                for j in (i + 1)..arms.len() {
                    let mut ambiguous: BTreeSet<Seq> = BTreeSet::new();
                    for a in &arms[i].1 {
                        for b in &arms[j].1 {
                            if a.starts_with(b.as_slice()) {
                                ambiguous.insert(b.clone());
                            } else if b.starts_with(a.as_slice()) {
                                ambiguous.insert(a.clone());
                            }
                        }
                    }
                    if !ambiguous.is_empty() {
                        out.push(RawConflict {
                            rule_name: rule_name.to_string(),
                            rule_span,
                            arm_i: arms[i].0,
                            arm_j: arms[j].0,
                            ambiguous,
                        });
                    }
                }
            }
            for x in xs {
                walk_check_conflicts(x, tail, rule_name, rule_span, nullable, first, k, out);
            }
        }
        Expr::Opt(x) => {
            walk_check_conflicts(x, tail, rule_name, rule_span, nullable, first, k, out);
        }
        Expr::Star(x) | Expr::Plus(x) => {
            // Inside a repetition, each iteration can be followed by
            // another iteration or by the outer tail — hence star_k
            // concatenated with tail rather than just tail.
            let fx = first_follow::first_of(x, nullable, first, k);
            let star = first_follow::star_k(&fx, k);
            let body_tail = first_follow::concat_k(&star, tail, k);
            walk_check_conflicts(x, &body_tail, rule_name, rule_span, nullable, first, k, out);
        }
    }
}

fn format_seq(seq: &Seq) -> String {
    if seq.is_empty() {
        "ε".to_string()
    } else {
        format!("[{}]", seq.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;

    fn tok(name: &str, pat: TokenPattern) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: pat,
            skip: false,
            is_fragment: false,
            span: Span::default(),
        }
    }

    fn rule(name: &str, body: Expr) -> RuleDef {
        RuleDef {
            name: name.into(),
            body,
            is_fragment: false,
            span: Span::default(),
        }
    }

    fn lit(s: &str) -> TokenPattern {
        TokenPattern::Literal(s.into())
    }

    fn alpha_plus() -> TokenPattern {
        TokenPattern::Plus(Box::new(TokenPattern::Class(CharClass {
            negated: false,
            items: vec![ClassItem::Range(b'a' as u32, b'z' as u32)],
        })))
    }

    fn minimal() -> Grammar {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule("main", Expr::Token("T".into())));
        g
    }

    #[test]
    fn clean_grammar_yields_grammar_no_diagnostics() {
        let outcome = analyze(minimal());
        assert!(!outcome.has_errors());
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        assert!(outcome.grammar.is_some());
        let ag = outcome.grammar.unwrap();
        assert_eq!(ag.k, 1);
        assert_eq!(ag.grammar.tokens.len(), 1);
    }

    #[test]
    fn warning_only_grammar_keeps_grammar() {
        // Unused fragment — warning only; grammar still compiles.
        let mut g = minimal();
        let mut frag = tok("_DEAD", lit("d"));
        frag.is_fragment = true;
        g.tokens.insert(0, frag);
        let outcome = analyze(g);
        assert!(!outcome.has_errors());
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(outcome.diagnostics[0].severity, Severity::Warning);
        assert!(outcome.grammar.is_some());
    }

    #[test]
    fn validate_error_short_circuits_before_lints() {
        // Undefined token reference — validate fails. Lints/shadow should
        // not run, so we expect exactly the one validate error and no
        // grammar.
        let mut g = Grammar::default();
        g.rules
            .push(rule("main", Expr::Token("UNDECLARED".into())));
        let outcome = analyze(g);
        assert!(outcome.has_errors());
        assert!(outcome.grammar.is_none());
        assert_eq!(outcome.diagnostics.len(), 1);
        assert!(outcome.diagnostics[0]
            .message
            .contains("undefined token `UNDECLARED`"));
    }

    #[test]
    fn empty_match_lint_blocks_compilation() {
        let mut g = minimal();
        g.tokens
            .push(tok("BAD", TokenPattern::Star(Box::new(lit("a")))));
        let outcome = analyze(g);
        assert!(outcome.has_errors());
        assert!(outcome.grammar.is_none());
        assert!(outcome
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("`BAD`")));
    }

    #[test]
    fn non_productive_lint_blocks_compilation() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        // `loops = T loops+;` — Plus needs body productive; recursion never bottoms out.
        g.rules.push(rule(
            "loops",
            Expr::Seq(vec![
                Expr::Token("T".into()),
                Expr::Plus(Box::new(Expr::Rule("loops".into()))),
            ]),
        ));
        let outcome = analyze(g);
        assert!(outcome.has_errors());
        assert!(outcome.grammar.is_none());
        assert!(outcome
            .diagnostics
            .iter()
            .any(|d| d.message.contains("non-productive")));
    }

    #[test]
    fn shadow_lint_blocks_compilation() {
        let mut g = Grammar::default();
        g.tokens.push(tok("IDENT", alpha_plus()));
        g.tokens.push(tok("IF", lit("if")));
        g.rules.push(rule(
            "main",
            Expr::Alt(vec![Expr::Token("IF".into()), Expr::Token("IDENT".into())]),
        ));
        let outcome = analyze(g);
        assert!(outcome.has_errors());
        assert!(outcome.grammar.is_none());
        assert!(outcome
            .diagnostics
            .iter()
            .any(|d| d.message.contains("shadowed by earlier-declared")));
    }

    #[test]
    fn warnings_accompany_grammar_on_full_success() {
        // Two unused fragments — both warnings, grammar still produced.
        let mut g = minimal();
        for name in &["_F1", "_F2"] {
            let mut frag = tok(name, lit("x"));
            frag.is_fragment = true;
            g.tokens.insert(0, frag);
        }
        let outcome = analyze(g);
        assert!(!outcome.has_errors());
        assert_eq!(outcome.diagnostics.len(), 2);
        assert!(outcome
            .diagnostics
            .iter()
            .all(|d| d.severity == Severity::Warning));
        assert!(outcome.grammar.is_some());
    }

    #[test]
    fn ll_conflict_reports_with_grammar_dropped() {
        // Two arms with the same FIRST set: classic LL(1) conflict.
        let mut g = Grammar::default();
        g.tokens.push(tok("A", lit("a")));
        g.rules.push(rule(
            "main",
            Expr::Alt(vec![
                Expr::Token("A".into()),
                Expr::Token("A".into()),
            ]),
        ));
        let outcome = analyze(g);
        assert!(outcome.has_errors());
        assert!(outcome.grammar.is_none());
    }

    #[test]
    fn has_errors_true_only_for_errors() {
        let outcome = AnalysisOutcome {
            grammar: None,
            diagnostics: vec![Diagnostic::warning("just a tip")],
        };
        assert!(!outcome.has_errors());

        let outcome = AnalysisOutcome {
            grammar: None,
            diagnostics: vec![
                Diagnostic::warning("tip"),
                Diagnostic::error("bad"),
            ],
        };
        assert!(outcome.has_errors());
    }
}
