//! Static check that every token reference in a rule body is reachable
//! given the lexer modes that can be active at that position.
//!
//! A token declared `@mode(tag) NAME = …` only matches while the lexer
//! has `tag` on top of its mode stack. If a rule references `NAME` from
//! a position where the lexer can never be in `tag` mode, the token can
//! never fire — the parse will lex-fail or get an "unexpected token"
//! every time it tries. The lint catches that at grammar-compile time.
//!
//! The analysis is conservative — it over-approximates the set of modes
//! reachable at any position so we never wrongly flag a valid grammar.
//! Two simplifications:
//!
//!   * `-> pop` is treated as a no-op for mode tracking. We don't model
//!     the lex stack, just the *set* of modes that could be on top, so
//!     a pop can't remove modes from that set. This may let some
//!     genuinely-unreachable token references slip through, but won't
//!     produce false positives.
//!
//!   * Public (non-fragment) rules are seeded with `default` because
//!     they're callable through generated `parse_<rule>` entry points.
//!     The lint treats every public rule as a potential entry, so a
//!     rule whose body needs a non-default mode (e.g. `attr_dq`) will
//!     be flagged unless it's marked as a fragment (which inlines it
//!     into its callers and skips the public-API treatment).

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;

/// Run the lint. Reports an error per token reference whose token
/// modes don't intersect the modes reachable at that position.
pub fn run(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    let active = compute_active_modes(g);
    for r in g.rules.values() {
        let entry = active.get(&r.name).cloned().unwrap_or_default();
        if entry.is_empty() {
            // Rule never reached from any entry — `check_unused_fragments`
            // already covers fragments; for non-fragments this can't
            // happen because we seed with "default". Skip just in case.
            continue;
        }
        check_walk(&r.body, &entry, r, g, issues);
    }
}

/// Per-rule "modes the lexer could be on top of when this rule's body
/// starts executing", computed as a least fixed point over the rule
/// call graph.
fn compute_active_modes(g: &Grammar) -> BTreeMap<String, BTreeSet<String>> {
    let mut active: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // Public (non-fragment) rules are exposed via generated
    // `parse_<rule>` entry points and can therefore be invoked while
    // the lexer is in the default mode.
    for r in g.rules.values() {
        if !r.is_fragment {
            active
                .entry(r.name.clone())
                .or_default()
                .insert("default".to_string());
        }
    }

    loop {
        let mut changed = false;
        // Snapshot the rule list so the walk can borrow `active` mutably
        // without iterator-invalidation drama.
        let rules: Vec<(String, Expr)> = g
            .rules
            .values()
            .map(|r| (r.name.clone(), r.body.clone()))
            .collect();
        for (name, body) in &rules {
            let entry = active.get(name).cloned().unwrap_or_default();
            propagate_walk(body, &entry, g, &mut active, &mut changed);
        }
        if !changed {
            break;
        }
    }

    active
}

/// First pass: walk the body in source order, threading the current
/// "modes possibly on top of stack" through the structure, and union
/// that into each callee rule's active set. Returns the modes possibly
/// on top *after* the body executes.
fn propagate_walk(
    e: &Expr,
    in_modes: &BTreeSet<String>,
    g: &Grammar,
    active: &mut BTreeMap<String, BTreeSet<String>>,
    changed: &mut bool,
) -> BTreeSet<String> {
    match e {
        Expr::Empty => in_modes.clone(),
        Expr::Token(name) => post_token_modes(g, name, in_modes),
        Expr::Rule(name) => {
            let entry = active.entry(name.clone()).or_default();
            let before = entry.len();
            for m in in_modes {
                entry.insert(m.clone());
            }
            if entry.len() > before {
                *changed = true;
            }
            in_modes.clone()
        }
        Expr::Seq(xs) => {
            let mut current = in_modes.clone();
            for x in xs {
                current = propagate_walk(x, &current, g, active, changed);
            }
            current
        }
        Expr::Alt(xs) => {
            let mut union = BTreeSet::new();
            for x in xs {
                let post = propagate_walk(x, in_modes, g, active, changed);
                union.extend(post);
            }
            union
        }
        Expr::Opt(x) => {
            // Either skipped (pre-modes) or executed (post-modes).
            let post = propagate_walk(x, in_modes, g, active, changed);
            in_modes.iter().cloned().chain(post).collect()
        }
        Expr::Star(x) | Expr::Plus(x) => {
            // One iteration captures the reachable modes; further
            // iterations don't add to the set under our
            // over-approximation (push monotonically grows the set).
            let post = propagate_walk(x, in_modes, g, active, changed);
            in_modes.iter().cloned().chain(post).collect()
        }
        Expr::Label(_, x) => propagate_walk(x, in_modes, g, active, changed),
    }
}

/// Final pass: once `active` has converged, walk each rule's body
/// again and emit a diagnostic at every token reference whose modes
/// don't intersect the local current set.
fn check_walk(
    e: &Expr,
    in_modes: &BTreeSet<String>,
    rule: &RuleDef,
    g: &Grammar,
    issues: &mut Vec<Diagnostic>,
) -> BTreeSet<String> {
    match e {
        Expr::Empty => in_modes.clone(),
        Expr::Token(name) => {
            if let Some(t) = g.tokens.get(name) {
                let token_modes: BTreeSet<&str> = t.modes.iter().map(String::as_str).collect();
                let in_set: BTreeSet<&str> = in_modes.iter().map(String::as_str).collect();
                if token_modes.is_disjoint(&in_set) {
                    let mut needed: Vec<&str> = t.modes.iter().map(String::as_str).collect();
                    needed.sort();
                    let mut have: Vec<&str> = in_modes.iter().map(String::as_str).collect();
                    have.sort();
                    issues.push(
                        Diagnostic::error(format!(
                            "rule `{}` references token `{}` which lives in mode(s) `{}`, \
                             but the lexer is on mode(s) `{}` at this position — the token \
                             can never be matched here",
                            rule.name,
                            name,
                            needed.join(", "),
                            have.join(", "),
                        ))
                        .at(rule.span),
                    );
                }
            }
            post_token_modes(g, name, in_modes)
        }
        Expr::Rule(_) => in_modes.clone(),
        Expr::Seq(xs) => {
            let mut current = in_modes.clone();
            for x in xs {
                current = check_walk(x, &current, rule, g, issues);
            }
            current
        }
        Expr::Alt(xs) => {
            let mut union = BTreeSet::new();
            for x in xs {
                let post = check_walk(x, in_modes, rule, g, issues);
                union.extend(post);
            }
            union
        }
        Expr::Opt(x) => {
            let post = check_walk(x, in_modes, rule, g, issues);
            in_modes.iter().cloned().chain(post).collect()
        }
        Expr::Star(x) | Expr::Plus(x) => {
            let post = check_walk(x, in_modes, rule, g, issues);
            in_modes.iter().cloned().chain(post).collect()
        }
        Expr::Label(_, x) => check_walk(x, in_modes, rule, g, issues),
    }
}

/// Apply a token's `-> push(x)` actions to the mode set. `-> pop` is
/// treated as a no-op (we don't track the stack precisely, so a pop
/// can't remove a mode from the set under our over-approximation).
fn post_token_modes(g: &Grammar, name: &str, in_modes: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = in_modes.clone();
    if let Some(t) = g.tokens.get(name) {
        for action in &t.mode_actions {
            if let ModeAction::Push(x) = action {
                out.insert(x.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;

    fn tok(name: &str, pat: TokenPattern, modes: Vec<&str>) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: pat,
            skip: false,
            is_fragment: false,
            modes: modes.into_iter().map(String::from).collect(),
            mode_actions: Vec::new(),
            span: Span::default(),
        }
    }

    fn tok_with_action(name: &str, pat: TokenPattern, modes: Vec<&str>, actions: Vec<ModeAction>) -> TokenDef {
        let mut t = tok(name, pat, modes);
        t.mode_actions = actions;
        t
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

    #[test]
    fn token_in_default_mode_referenced_from_default_rule_is_ok() {
        let mut g = Grammar::default();
        g.add_token(tok("T", lit("t"), vec!["default"]));
        g.add_rule(rule("main", Expr::Token("T".into())));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn token_in_other_mode_referenced_from_default_rule_is_flagged() {
        let mut g = Grammar::default();
        g.add_token(tok("T", lit("t"), vec!["tag"]));
        g.add_rule(rule("main", Expr::Token("T".into())));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("can never be matched here"),
            "{}",
            issues[0].message
        );
    }

    #[test]
    fn token_after_push_is_reachable_in_pushed_mode() {
        let mut g = Grammar::default();
        g.add_token(tok_with_action(
            "OPEN",
            lit("<"),
            vec!["default"],
            vec![ModeAction::Push("tag".into())],
        ));
        g.add_token(tok("NAME", lit("x"), vec!["tag"]));
        g.add_rule(rule(
            "main",
            Expr::Seq(vec![Expr::Token("OPEN".into()), Expr::Token("NAME".into())]),
        ));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn multi_mode_token_overlaps_pushed_mode() {
        let mut g = Grammar::default();
        g.add_token(tok_with_action(
            "OPEN",
            lit("<"),
            vec!["default"],
            vec![ModeAction::Push("inner".into())],
        ));
        // `AMP` lives in both default and inner — a rule call inside
        // either is fine.
        g.add_token(tok("AMP", lit("&"), vec!["default", "inner"]));
        g.add_rule(rule(
            "main",
            Expr::Seq(vec![Expr::Token("OPEN".into()), Expr::Token("AMP".into())]),
        ));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn rule_call_propagates_modes_to_callee() {
        // After OPEN, the lexer is in `tag`. Calling `inner` from there
        // should mark `inner`'s active modes as containing `tag`, so
        // `inner` can use a tag-mode token without complaint.
        let mut g = Grammar::default();
        g.add_token(tok_with_action(
            "OPEN",
            lit("<"),
            vec!["default"],
            vec![ModeAction::Push("tag".into())],
        ));
        g.add_token(tok("NAME", lit("x"), vec!["tag"]));
        let mut inner = rule("inner", Expr::Token("NAME".into()));
        inner.is_fragment = true;
        g.add_rule(inner);
        g.add_rule(rule(
            "main",
            Expr::Seq(vec![Expr::Token("OPEN".into()), Expr::Rule("inner".into())]),
        ));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn rule_only_reachable_in_non_default_mode_flagged_as_public_entry() {
        // Public (non-fragment) rule whose body needs a non-default
        // mode. Because every public rule is a potential parse_<rule>
        // entry, the lint flags this — the user should mark the rule
        // as a fragment if it's only meant to be called internally.
        let mut g = Grammar::default();
        g.add_token(tok("NAME", lit("x"), vec!["tag"]));
        g.add_rule(rule("inner", Expr::Token("NAME".into())));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(!issues.is_empty(), "should have flagged the rule");
    }
}
