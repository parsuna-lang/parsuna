//! Post-validation lints. Run after [`super::validate`] has confirmed that
//! refs resolve and there are no token cycles — the helpers here assume
//! both invariants.

use std::collections::BTreeSet;

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;

/// Run every lint. Each check chooses its own severity — empty-match tokens
/// and non-productive rules are hard errors; unused fragments are warnings.
pub fn run(g: &Grammar, diags: &mut Vec<Diagnostic>) {
    check_empty_match_tokens(g, diags);
    check_unused_fragments(g, diags);
    check_non_productive_rules(g, diags);
}

/// Flag non-fragment tokens whose pattern can match the empty string.
/// The lexer would happily accept at length 0 — depending on the runtime,
/// that's either an infinite loop or a stream of zero-length tokens.
fn check_empty_match_tokens(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    for t in &g.tokens {
        if t.is_fragment {
            continue;
        }
        if pattern_nullable(&t.pattern, g, &mut BTreeSet::new()) {
            issues.push(
                Diagnostic::error(format!(
                    "token `{}` can match the empty string; the lexer would emit \
                     zero-length tokens. Tighten the pattern (e.g. use `+` instead of `*`).",
                    t.name
                ))
                .at(t.span),
            );
        }
    }
}

fn pattern_nullable(p: &TokenPattern, g: &Grammar, visiting: &mut BTreeSet<String>) -> bool {
    match p {
        TokenPattern::Empty => true,
        TokenPattern::Literal(s) => s.is_empty(),
        TokenPattern::Class(_) => false,
        TokenPattern::Ref(n) => {
            if !visiting.insert(n.clone()) {
                return false;
            }
            let res = match g.token(n) {
                Some(td) => pattern_nullable(&td.pattern, g, visiting),
                None => false,
            };
            visiting.remove(n);
            res
        }
        TokenPattern::Seq(xs) => xs.iter().all(|x| pattern_nullable(x, g, visiting)),
        TokenPattern::Alt(xs) => xs.iter().any(|x| pattern_nullable(x, g, visiting)),
        TokenPattern::Opt(_) | TokenPattern::Star(_) => true,
        TokenPattern::Plus(x) => pattern_nullable(x, g, visiting),
    }
}

/// Flag fragment tokens (`_FOO`) and fragment rules (`_foo`) that no other
/// declaration references — pure dead code. Reachability seeds are the
/// non-fragment tokens, the non-fragment rules, and the skip tokens; any
/// fragment not reached from those is unused.
fn check_unused_fragments(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    let mut reachable_tokens: BTreeSet<String> = BTreeSet::new();
    let mut reachable_rules: BTreeSet<String> = BTreeSet::new();
    let mut token_queue: Vec<String> = Vec::new();
    let mut rule_queue: Vec<String> = Vec::new();

    for t in &g.tokens {
        if !t.is_fragment {
            reachable_tokens.insert(t.name.clone());
            token_queue.push(t.name.clone());
        }
    }
    for r in &g.rules {
        if !r.is_fragment {
            reachable_rules.insert(r.name.clone());
            rule_queue.push(r.name.clone());
        }
    }

    while !token_queue.is_empty() || !rule_queue.is_empty() {
        if let Some(name) = token_queue.pop() {
            if let Some(td) = g.token(&name) {
                let mut refs = Vec::new();
                collect_pattern_refs(&td.pattern, &mut refs);
                for r in refs {
                    if g.token(&r).is_some() && reachable_tokens.insert(r.clone()) {
                        token_queue.push(r);
                    }
                }
            }
            continue;
        }
        if let Some(name) = rule_queue.pop() {
            if let Some(rd) = g.rule(&name) {
                let mut tok_refs = Vec::new();
                let mut rule_refs = Vec::new();
                collect_expr_refs(&rd.body, &mut tok_refs, &mut rule_refs);
                for r in tok_refs {
                    if g.token(&r).is_some() && reachable_tokens.insert(r.clone()) {
                        token_queue.push(r);
                    }
                }
                for r in rule_refs {
                    if g.rule(&r).is_some() && reachable_rules.insert(r.clone()) {
                        rule_queue.push(r);
                    }
                }
            }
        }
    }

    for t in &g.tokens {
        if t.is_fragment && !reachable_tokens.contains(&t.name) {
            issues.push(
                Diagnostic::warning(format!("fragment token `{}` is never referenced", t.name))
                    .at(t.span),
            );
        }
    }
    for r in &g.rules {
        if r.is_fragment && !reachable_rules.contains(&r.name) {
            issues.push(
                Diagnostic::warning(format!("fragment rule `{}` is never referenced", r.name))
                    .at(r.span),
            );
        }
    }
}

fn collect_pattern_refs(p: &TokenPattern, out: &mut Vec<String>) {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => {}
        TokenPattern::Ref(n) => out.push(n.clone()),
        TokenPattern::Seq(xs) | TokenPattern::Alt(xs) => {
            xs.iter().for_each(|x| collect_pattern_refs(x, out))
        }
        TokenPattern::Opt(x) | TokenPattern::Star(x) | TokenPattern::Plus(x) => {
            collect_pattern_refs(x, out)
        }
    }
}

fn collect_expr_refs(e: &Expr, tok: &mut Vec<String>, rule: &mut Vec<String>) {
    match e {
        Expr::Empty => {}
        Expr::Token(n) => tok.push(n.clone()),
        Expr::Rule(n) => rule.push(n.clone()),
        Expr::Seq(xs) | Expr::Alt(xs) => xs.iter().for_each(|x| collect_expr_refs(x, tok, rule)),
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => collect_expr_refs(x, tok, rule),
    }
}

/// Flag rules with no derivation that terminates in tokens. A rule is
/// productive if some alternative reduces to terminals or to other
/// productive rules; we compute the productive set as a least fixed point.
fn check_non_productive_rules(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    let mut productive: BTreeSet<String> = BTreeSet::new();
    loop {
        let mut changed = false;
        for r in &g.rules {
            if productive.contains(&r.name) {
                continue;
            }
            if expr_productive(&r.body, &productive) {
                productive.insert(r.name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for r in &g.rules {
        if !productive.contains(&r.name) {
            issues.push(
                Diagnostic::error(format!(
                    "rule `{}` is non-productive: no derivation terminates in tokens. \
                     Make sure at least one alternative is purely terminal or reaches \
                     productive rules without re-entering `{}` unconditionally.",
                    r.name, r.name
                ))
                .at(r.span),
            );
        }
    }
}

fn expr_productive(e: &Expr, productive: &BTreeSet<String>) -> bool {
    match e {
        Expr::Empty | Expr::Token(_) => true,
        Expr::Rule(n) => productive.contains(n),
        Expr::Seq(xs) => xs.iter().all(|x| expr_productive(x, productive)),
        Expr::Alt(xs) => xs.iter().any(|x| expr_productive(x, productive)),
        Expr::Opt(_) | Expr::Star(_) => true,
        Expr::Plus(x) => expr_productive(x, productive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(name: &str, pat: TokenPattern) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: pat,
            skip: false,
            is_fragment: false,
            span: Default::default(),
        }
    }

    fn rule(name: &str, body: Expr) -> RuleDef {
        RuleDef {
            name: name.into(),
            body,
            is_fragment: false,
            span: Default::default(),
        }
    }

    fn lit(s: &str) -> TokenPattern {
        TokenPattern::Literal(s.into())
    }

    fn class_range(lo: char, hi: char) -> TokenPattern {
        TokenPattern::Class(CharClass {
            negated: false,
            items: vec![ClassItem::Range(lo as u32, hi as u32)],
        })
    }

    // ---- empty-match ----

    #[test]
    fn empty_match_flags_optional_token() {
        let mut g = Grammar::default();
        g.tokens
            .push(tok("BAD", TokenPattern::Opt(Box::new(lit("a")))));
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("`BAD`"));
    }

    #[test]
    fn empty_match_flags_star_token() {
        let mut g = Grammar::default();
        g.tokens
            .push(tok("BAD", TokenPattern::Star(Box::new(lit("a")))));
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn empty_match_flags_empty_literal() {
        let mut g = Grammar::default();
        g.tokens.push(tok("BAD", lit("")));
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn empty_match_passes_plus_and_class() {
        let mut g = Grammar::default();
        g.tokens
            .push(tok("OK1", TokenPattern::Plus(Box::new(lit("a")))));
        g.tokens.push(tok("OK2", class_range('a', 'z')));
        g.tokens.push(tok("OK3", lit("if")));
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn empty_match_through_fragment_ref() {
        let mut g = Grammar::default();
        let mut frag = tok("_NULLABLE", TokenPattern::Star(Box::new(lit("a"))));
        frag.is_fragment = true;
        g.tokens.push(frag);
        g.tokens
            .push(tok("BAD", TokenPattern::Ref("_NULLABLE".into())));
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("`BAD`"));
    }

    #[test]
    fn empty_match_skips_fragment_themselves() {
        // A nullable fragment isn't directly a problem; only the public
        // tokens that consume it would be (covered above).
        let mut g = Grammar::default();
        let mut frag = tok("_NULLABLE", TokenPattern::Star(Box::new(lit("a"))));
        frag.is_fragment = true;
        g.tokens.push(frag);
        let mut issues = Vec::new();
        check_empty_match_tokens(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    // ---- unused fragments ----

    #[test]
    fn unused_fragment_token() {
        let mut g = Grammar::default();
        let mut frag = tok("_HEX_DIGIT", class_range('0', '9'));
        frag.is_fragment = true;
        g.tokens.push(frag);
        g.tokens.push(tok("IDENT", class_range('a', 'z')));
        g.rules
            .push(rule("r", Expr::Token("IDENT".into())));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("_HEX_DIGIT"));
    }

    #[test]
    fn unused_fragment_rule() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        let mut frag = rule("_unused", Expr::Token("T".into()));
        frag.is_fragment = true;
        g.rules.push(frag);
        g.rules.push(rule("main", Expr::Token("T".into())));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("_unused"));
    }

    #[test]
    fn used_fragment_token_via_pattern_ref() {
        let mut g = Grammar::default();
        let mut frag = tok("_HEX_DIGIT", class_range('0', '9'));
        frag.is_fragment = true;
        g.tokens.push(frag);
        g.tokens.push(tok(
            "HEX",
            TokenPattern::Plus(Box::new(TokenPattern::Ref("_HEX_DIGIT".into()))),
        ));
        g.rules.push(rule("r", Expr::Token("HEX".into())));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn used_fragment_rule_via_expr_ref() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        let mut frag = rule("_helper", Expr::Token("T".into()));
        frag.is_fragment = true;
        g.rules.push(frag);
        g.rules.push(rule(
            "main",
            Expr::Seq(vec![Expr::Token("T".into()), Expr::Rule("_helper".into())]),
        ));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn skip_token_counts_as_live_seed() {
        // A skip token isn't referenced from any rule, but it still needs
        // its fragments — those should be considered used.
        let mut g = Grammar::default();
        let mut digit = tok("_DIGIT", class_range('0', '9'));
        digit.is_fragment = true;
        g.tokens.push(digit);
        let mut ws = tok(
            "WS",
            TokenPattern::Plus(Box::new(TokenPattern::Ref("_DIGIT".into()))),
        );
        ws.skip = true;
        g.tokens.push(ws);
        g.rules.push(rule("r", Expr::Empty));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn fragment_chain_only_dead_at_root() {
        // _LEAF only used by _MID, which is only used by _ROOT — and _ROOT
        // is not used. We expect all three flagged in one pass.
        let mut g = Grammar::default();
        let mut leaf = tok("_LEAF", class_range('a', 'z'));
        leaf.is_fragment = true;
        g.tokens.push(leaf);
        let mut mid = tok("_MID", TokenPattern::Ref("_LEAF".into()));
        mid.is_fragment = true;
        g.tokens.push(mid);
        let mut root = tok("_ROOT", TokenPattern::Ref("_MID".into()));
        root.is_fragment = true;
        g.tokens.push(root);
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule("r", Expr::Token("T".into())));
        let mut issues = Vec::new();
        check_unused_fragments(&g, &mut issues);
        assert_eq!(issues.len(), 3, "{:?}", issues);
    }

    // ---- productivity ----

    #[test]
    fn productive_via_token_alternative() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule(
            "a",
            Expr::Alt(vec![
                Expr::Token("T".into()),
                Expr::Seq(vec![Expr::Token("T".into()), Expr::Rule("a".into())]),
            ]),
        ));
        let mut issues = Vec::new();
        check_non_productive_rules(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn non_productive_self_only() {
        let mut g = Grammar::default();
        g.rules.push(rule("a", Expr::Rule("a".into())));
        let mut issues = Vec::new();
        check_non_productive_rules(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("`a`"));
    }

    #[test]
    fn non_productive_mutual_cycle() {
        let mut g = Grammar::default();
        g.rules.push(rule("a", Expr::Rule("b".into())));
        g.rules.push(rule("b", Expr::Rule("a".into())));
        let mut issues = Vec::new();
        check_non_productive_rules(&g, &mut issues);
        assert_eq!(issues.len(), 2);
    }

    #[test]
    fn productive_via_star_self_reference() {
        // a = T a* — `Star(Rule(a))` is always productive (matches empty),
        // so the Seq is productive on its first iteration via just T.
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule(
            "a",
            Expr::Seq(vec![
                Expr::Token("T".into()),
                Expr::Star(Box::new(Expr::Rule("a".into()))),
            ]),
        ));
        let mut issues = Vec::new();
        check_non_productive_rules(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn non_productive_plus_self_reference() {
        // a = T a+ — Plus requires its body productive; a is unproductive
        // until proven otherwise, so this never bottoms out.
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule(
            "a",
            Expr::Seq(vec![
                Expr::Token("T".into()),
                Expr::Plus(Box::new(Expr::Rule("a".into()))),
            ]),
        ));
        let mut issues = Vec::new();
        check_non_productive_rules(&g, &mut issues);
        assert_eq!(issues.len(), 1);
    }
}
