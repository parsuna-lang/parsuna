//! Purely-structural grammar checks that do not need FIRST/FOLLOW.
//!
//! Catches reserved or duplicate names, undefined token/rule references,
//! token-reference cycles, and left recursion — all things that would
//! otherwise crash or loop later stages.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Error;
use crate::grammar::ir::*;
use crate::span::Span;

/// Run every validator against `g`, accumulating issues into `issues`.
/// The analysis pipeline only proceeds if `issues` is empty after this
/// call.
pub fn run(g: &Grammar, issues: &mut Vec<Error>) {
    for t in &g.tokens {
        if is_reserved_token_name(&t.name) {
            issues.push(
                Error::new(format!(
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
            issues.push(Error::new(format!("duplicate token: {}", t.name)).at(t.span));
        } else {
            seen.insert(&t.name, t.span);
        }
    }
    let mut seen_r: BTreeMap<&str, Span> = BTreeMap::new();
    for r in &g.rules {
        if seen_r.contains_key(r.name.as_str()) {
            issues.push(Error::new(format!("duplicate rule: {}", r.name)).at(r.span));
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
                Error::new(format!("rule `{}` is left-recursive; parsuna is LL and does not accept left recursion, rewrite using `*` or `+` repetition", r.name))
                    .at(r.span)
            );
        }
    }

    if !g.rules.iter().any(|r| !r.is_fragment) {
        issues.push(Error::new(
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
    issues: &mut Vec<Error>,
    ctx: &str,
    ctx_span: Span,
) {
    match e {
        Expr::Empty => {}
        Expr::Token(n) => match tk.get(n.as_str()) {
            None => issues.push(Error::new(
                format!("undefined token `{}` in rule `{}`", n, ctx)
            ).at(ctx_span)),
            Some(t) if t.is_fragment => issues.push(
                Error::new(format!(
                    "rule `{}` references fragment token `{}`; fragments are only usable inside other tokens",
                    ctx, n
                )).at(ctx_span)
            ),
            _ => {}
        },
        Expr::Rule(n) => if !rl.contains(n.as_str()) {
            issues.push(Error::new(
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
    issues: &mut Vec<Error>,
    ctx: &str,
    ctx_span: Span,
) {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => {}
        TokenPattern::Ref(n) => {
            if !tk.contains_key(n.as_str()) {
                issues.push(
                    Error::new(format!("undefined token `{}` in token `{}`", n, ctx)).at(ctx_span),
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

fn detect_token_cycles(g: &Grammar, tk: &BTreeMap<&str, &TokenDef>, issues: &mut Vec<Error>) {
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
        issues: &mut Vec<Error>,
    ) {
        if stack.iter().any(|s| s == name) {
            let path: Vec<String> = stack.iter().cloned().skip_while(|x| x != name).collect();
            let cycle = format!("{} -> {}", path.join(" -> "), name);
            if let Some(td) = tk.get(name) {
                issues.push(Error::new(format!("token reference cycle: {}", cycle)).at(td.span));
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
