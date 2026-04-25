//! Purely-structural grammar checks that do not need FIRST/FOLLOW.
//!
//! Catches reserved or duplicate names, undefined token/rule references,
//! token-reference cycles, and left recursion — all things that would
//! otherwise crash or loop later stages.

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;
use crate::Span;

/// Run every validator against `g`, accumulating issues into `issues`.
/// The analysis pipeline only proceeds if `issues` is empty after this
/// call.
pub fn run(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    for t in &g.tokens {
        if is_reserved_token_name(&t.name) {
            issues.push(
                Diagnostic::error(format!(
                    "token name `{}` is reserved (EOF and ERROR are emitted by the runtime)",
                    t.name
                ))
                .at(t.span),
            );
        }
    }

    let mut seen: BTreeMap<&str, Span> = BTreeMap::new();
    for t in &g.tokens {
        if seen.contains_key(t.name.as_str()) {
            issues.push(Diagnostic::error(format!("duplicate token: {}", t.name)).at(t.span));
        } else {
            seen.insert(&t.name, t.span);
        }
    }
    let mut seen_r: BTreeMap<&str, Span> = BTreeMap::new();
    for r in &g.rules {
        if seen_r.contains_key(r.name.as_str()) {
            issues.push(Diagnostic::error(format!("duplicate rule: {}", r.name)).at(r.span));
        } else {
            seen_r.insert(&r.name, r.span);
        }
    }

    let known_tokens: BTreeMap<&str, &TokenDef> =
        g.tokens.iter().map(|t| (t.name.as_str(), t)).collect();
    let known_rules: BTreeSet<&str> = g.rules.iter().map(|r| r.name.as_str()).collect();

    for r in &g.rules {
        check_expr_refs(
            &r.body,
            &known_tokens,
            &known_rules,
            issues,
            &r.name,
            r.span,
        );
    }

    for t in &g.tokens {
        check_pattern_refs(&t.pattern, &known_tokens, issues, &t.name, t.span);
    }

    detect_token_cycles(g, &known_tokens, issues);

    let mut leads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in &g.rules {
        let mut set = BTreeSet::new();
        collect_first_rule_names(&r.body, &mut set);
        leads.insert(r.name.clone(), set);
    }
    for r in &g.rules {
        if leads.get(&r.name).map_or(false, |s| s.contains(&r.name)) {
            issues.push(
                Diagnostic::error(format!("rule `{}` is left-recursive; parsuna is LL and does not accept left recursion, rewrite using `*` or `+` repetition", r.name))
                    .at(r.span)
            );
        }
    }

    if !g.rules.iter().any(|r| !r.is_fragment) {
        issues.push(Diagnostic::error(
            "grammar contains no non-fragment rules; nothing would be emitted",
        ));
    }
}

fn is_reserved_token_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "EOF" | "ERROR")
}

fn check_expr_refs(
    e: &Expr,
    tk: &BTreeMap<&str, &TokenDef>,
    rl: &BTreeSet<&str>,
    issues: &mut Vec<Diagnostic>,
    ctx: &str,
    ctx_span: Span,
) {
    match e {
        Expr::Empty => {}
        Expr::Token(n) => match tk.get(n.as_str()) {
            None => issues.push(Diagnostic::error(
                format!("undefined token `{}` in rule `{}`", n, ctx)
            ).at(ctx_span)),
            Some(t) if t.is_fragment => issues.push(
                Diagnostic::error(format!(
                    "rule `{}` references fragment token `{}`; fragments are only usable inside other tokens",
                    ctx, n
                )).at(ctx_span)
            ),
            _ => {}
        },
        Expr::Rule(n) => if !rl.contains(n.as_str()) {
            issues.push(Diagnostic::error(
                format!("undefined rule `{}` in rule `{}`", n, ctx)
            ).at(ctx_span));
        },
        Expr::Seq(xs) | Expr::Alt(xs) => xs.iter().for_each(|x| check_expr_refs(x, tk, rl, issues, ctx, ctx_span)),
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => check_expr_refs(x, tk, rl, issues, ctx, ctx_span),
    }
}

fn check_pattern_refs(
    p: &TokenPattern,
    tk: &BTreeMap<&str, &TokenDef>,
    issues: &mut Vec<Diagnostic>,
    ctx: &str,
    ctx_span: Span,
) {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => {}
        TokenPattern::Ref(n) => {
            if !tk.contains_key(n.as_str()) {
                issues.push(
                    Diagnostic::error(format!("undefined token `{}` in token `{}`", n, ctx))
                        .at(ctx_span),
                );
            }
        }
        TokenPattern::Seq(xs) | TokenPattern::Alt(xs) => xs
            .iter()
            .for_each(|x| check_pattern_refs(x, tk, issues, ctx, ctx_span)),
        TokenPattern::Opt(x) | TokenPattern::Star(x) | TokenPattern::Plus(x) => {
            check_pattern_refs(x, tk, issues, ctx, ctx_span)
        }
    }
}

fn detect_token_cycles(g: &Grammar, tk: &BTreeMap<&str, &TokenDef>, issues: &mut Vec<Diagnostic>) {
    fn collect_refs(p: &TokenPattern, out: &mut Vec<String>) {
        match p {
            TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => {}
            TokenPattern::Ref(n) => out.push(n.clone()),
            TokenPattern::Seq(xs) | TokenPattern::Alt(xs) => {
                xs.iter().for_each(|x| collect_refs(x, out))
            }
            TokenPattern::Opt(x) | TokenPattern::Star(x) | TokenPattern::Plus(x) => {
                collect_refs(x, out)
            }
        }
    }

    fn visit(
        name: &str,
        g: &Grammar,
        tk: &BTreeMap<&str, &TokenDef>,
        stack: &mut Vec<String>,
        visited: &mut BTreeSet<String>,
        issues: &mut Vec<Diagnostic>,
    ) {
        if stack.iter().any(|s| s == name) {
            let path: Vec<String> = stack.iter().cloned().skip_while(|x| x != name).collect();
            let cycle = format!("{} -> {}", path.join(" -> "), name);
            if let Some(td) = tk.get(name) {
                issues.push(
                    Diagnostic::error(format!("token reference cycle: {}", cycle)).at(td.span),
                );
            }
            return;
        }
        if !visited.insert(name.to_string()) {
            return;
        }
        stack.push(name.to_string());
        if let Some(td) = g.token(name) {
            let mut refs = Vec::new();
            collect_refs(&td.pattern, &mut refs);
            for r in refs {
                visit(&r, g, tk, stack, visited, issues);
            }
        }
        stack.pop();
    }

    let mut visited: BTreeSet<String> = BTreeSet::new();
    for t in &g.tokens {
        let mut stack: Vec<String> = Vec::new();
        visit(&t.name, g, tk, &mut stack, &mut visited, issues);
    }
}

fn collect_first_rule_names(e: &Expr, out: &mut BTreeSet<String>) {
    match e {
        Expr::Empty | Expr::Token(_) => {}
        Expr::Rule(n) => {
            out.insert(n.clone());
        }
        Expr::Seq(xs) => {
            for x in xs {
                collect_first_rule_names(x, out);
                if !expr_definitely_nullable(x) {
                    break;
                }
            }
        }
        Expr::Alt(xs) => {
            for x in xs {
                collect_first_rule_names(x, out);
            }
        }
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => collect_first_rule_names(x, out),
    }
}

fn expr_definitely_nullable(e: &Expr) -> bool {
    match e {
        Expr::Empty | Expr::Opt(_) | Expr::Star(_) => true,
        _ => false,
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

    fn run_collect(g: &Grammar) -> Vec<Diagnostic> {
        let mut issues = Vec::new();
        run(g, &mut issues);
        issues
    }

    #[test]
    fn reserved_token_names_eof_and_error_are_rejected() {
        let mut g = Grammar::default();
        g.tokens.push(tok("EOF", lit("e")));
        g.tokens.push(tok("ERROR", lit("r")));
        g.rules.push(rule("main", Expr::Empty));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message.contains("`EOF`")));
        assert!(issues.iter().any(|d| d.message.contains("`ERROR`")));
    }

    #[test]
    fn reserved_check_is_case_insensitive() {
        let mut g = Grammar::default();
        g.tokens.push(tok("eof", lit("e")));
        g.rules.push(rule("main", Expr::Empty));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message.contains("reserved")));
    }

    #[test]
    fn duplicate_token_and_rule_names_flagged() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.tokens.push(tok("T", lit("u")));
        g.rules.push(rule("r", Expr::Token("T".into())));
        g.rules.push(rule("r", Expr::Token("T".into())));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message == "duplicate token: T"));
        assert!(issues.iter().any(|d| d.message == "duplicate rule: r"));
    }

    #[test]
    fn undefined_token_in_rule_body_flagged() {
        let mut g = Grammar::default();
        g.rules
            .push(rule("main", Expr::Token("MISSING".into())));
        let issues = run_collect(&g);
        assert!(issues
            .iter()
            .any(|d| d.message.contains("undefined token `MISSING`")));
    }

    #[test]
    fn undefined_rule_in_body_flagged() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules
            .push(rule("main", Expr::Rule("ghost".into())));
        let issues = run_collect(&g);
        assert!(issues
            .iter()
            .any(|d| d.message.contains("undefined rule `ghost`")));
    }

    #[test]
    fn rule_referencing_fragment_token_flagged() {
        let mut g = Grammar::default();
        let mut frag = tok("_F", lit("f"));
        frag.is_fragment = true;
        g.tokens.push(frag);
        g.rules.push(rule("main", Expr::Token("_F".into())));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message.contains("fragment token")));
    }

    #[test]
    fn undefined_token_ref_inside_pattern_flagged() {
        let mut g = Grammar::default();
        g.tokens
            .push(tok("OUTER", TokenPattern::Ref("MISSING".into())));
        g.rules.push(rule("main", Expr::Token("OUTER".into())));
        let issues = run_collect(&g);
        assert!(issues
            .iter()
            .any(|d| d.message.contains("undefined token `MISSING`")
                && d.message.contains("in token `OUTER`")));
    }

    #[test]
    fn token_reference_cycle_flagged() {
        let mut g = Grammar::default();
        let mut a = tok("A", TokenPattern::Ref("B".into()));
        a.is_fragment = true;
        let mut b = tok("B", TokenPattern::Ref("A".into()));
        b.is_fragment = true;
        g.tokens.push(a);
        g.tokens.push(b);
        g.tokens.push(tok("USE", TokenPattern::Ref("A".into())));
        g.rules.push(rule("main", Expr::Token("USE".into())));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message.contains("cycle")));
    }

    #[test]
    fn left_recursion_direct_flagged() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule(
            "expr",
            Expr::Seq(vec![Expr::Rule("expr".into()), Expr::Token("T".into())]),
        ));
        let issues = run_collect(&g);
        assert!(issues
            .iter()
            .any(|d| d.message.contains("left-recursive")));
    }

    #[test]
    fn left_recursion_through_nullable_prefix_flagged() {
        // `a = b? a T;` — `b?` is nullable, so `a` can begin with itself.
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule(
            "b",
            Expr::Token("T".into()),
        ));
        g.rules.push(rule(
            "a",
            Expr::Seq(vec![
                Expr::Opt(Box::new(Expr::Rule("b".into()))),
                Expr::Rule("a".into()),
                Expr::Token("T".into()),
            ]),
        ));
        let issues = run_collect(&g);
        assert!(issues.iter().any(|d| d.message.contains("`a`")
            && d.message.contains("left-recursive")));
    }

    #[test]
    fn grammar_with_only_fragments_has_no_emittable_rules() {
        let mut g = Grammar::default();
        let mut frag = rule("_helper", Expr::Empty);
        frag.is_fragment = true;
        g.rules.push(frag);
        let issues = run_collect(&g);
        assert!(issues
            .iter()
            .any(|d| d.message.contains("no non-fragment rules")));
    }

    #[test]
    fn clean_grammar_yields_no_issues() {
        let mut g = Grammar::default();
        g.tokens.push(tok("T", lit("t")));
        g.rules.push(rule("main", Expr::Token("T".into())));
        let issues = run_collect(&g);
        assert!(issues.is_empty(), "{:?}", issues);
    }
}
