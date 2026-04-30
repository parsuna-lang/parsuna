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

use std::collections::BTreeMap;

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;

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
/// The structural validators and lints run first; if they pass,
/// [`first_follow::solve_lookahead`] iteratively deepens `k` until no
/// alternative remains ambiguous, and the FIRST/FOLLOW tables for the
/// chosen `k` are returned as part of [`AnalyzedGrammar`]. Failures at any
/// stage are pushed into the outcome's `diagnostics` bag.
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

    let (k, nullable, first) = first_follow::solve_lookahead(&g, diags)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;
    use crate::Span;

    fn tok(name: &str, pat: TokenPattern) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: pat,
            skip: false,
            is_fragment: false,
            mode: None,
            mode_actions: Vec::new(),
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
        g.add_token(tok("T", lit("t")));
        g.add_rule(rule("main", Expr::Token("T".into())));
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
        g.add_token(frag);
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
        g.add_rule(rule("main", Expr::Token("UNDECLARED".into())));
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
        g.add_token(tok("BAD", TokenPattern::Star(Box::new(lit("a")))));
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
        g.add_token(tok("T", lit("t")));
        // `loops = T loops+;` — Plus needs body productive; recursion never bottoms out.
        g.add_rule(rule(
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
        g.add_token(tok("IDENT", alpha_plus()));
        g.add_token(tok("IF", lit("if")));
        g.add_rule(rule(
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
            g.add_token(frag);
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
        g.add_token(tok("A", lit("a")));
        g.add_rule(rule(
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
