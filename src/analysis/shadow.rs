//! Detects literal tokens that are unreachable because an earlier-declared
//! token's pattern also accepts them.
//!
//! The lexer breaks accept-length ties by smallest token id (= declaration
//! order, see `lowering::lexer_dfa::build_dfa_state`). So a token like
//! `IF = "if";` declared *after* an `IDENT = [a-z]+;` would silently never
//! fire — the DFA would always pick `IDENT`. This pass walks every
//! non-fragment, non-skip literal token and reports the case.

use std::collections::BTreeSet;

use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;

/// Run the shadow check. Assumes structural validation already passed —
/// in particular, that token references resolve and are acyclic.
pub fn run(g: &Grammar, issues: &mut Vec<Diagnostic>) {
    let live: Vec<(usize, &TokenDef)> = g
        .tokens
        .values()
        .enumerate()
        .filter(|(_, t)| !t.is_fragment && !t.skip)
        .collect();

    for &(i, t) in &live {
        let TokenPattern::Literal(s) = &t.pattern else {
            continue;
        };
        let bytes = s.as_bytes();
        for &(j, u) in &live {
            if j >= i {
                break;
            }
            let resolved = resolve_pattern(&u.pattern, g);
            if pattern_matches_exactly(&resolved, bytes) {
                issues.push(
                    Diagnostic::error(format!(
                        "token `{}` (literal {:?}) is shadowed by earlier-declared token `{}`; \
                         the lexer would always pick `{}`. Move `{}` above `{}`.",
                        t.name, s, u.name, u.name, t.name, u.name
                    ))
                    .at(t.span),
                );
                break;
            }
        }
    }
}

/// True iff `pat` (with refs already resolved) has an accepting path that
/// consumes exactly the bytes of `s` — no more, no less.
pub fn pattern_matches_exactly(pat: &TokenPattern, s: &[u8]) -> bool {
    let mut out = BTreeSet::new();
    ends(pat, s, 0, &mut out);
    out.contains(&s.len())
}

fn ends(pat: &TokenPattern, s: &[u8], start: usize, out: &mut BTreeSet<usize>) {
    match pat {
        TokenPattern::Empty => {
            out.insert(start);
        }
        TokenPattern::Literal(lit) => {
            let bs = lit.as_bytes();
            if start + bs.len() <= s.len() && &s[start..start + bs.len()] == bs {
                out.insert(start + bs.len());
            }
        }
        TokenPattern::Class(cc) => {
            if start < s.len() && cc.contains(s[start] as u32) {
                out.insert(start + 1);
            }
        }
        TokenPattern::Ref(_) => {
            // resolve_pattern should have inlined these.
        }
        TokenPattern::Seq(xs) => {
            let mut cur: BTreeSet<usize> = BTreeSet::new();
            cur.insert(start);
            for x in xs {
                let mut next = BTreeSet::new();
                for &p in &cur {
                    ends(x, s, p, &mut next);
                }
                cur = next;
                if cur.is_empty() {
                    return;
                }
            }
            out.extend(cur);
        }
        TokenPattern::Alt(xs) => {
            for x in xs {
                ends(x, s, start, out);
            }
        }
        TokenPattern::Opt(x) => {
            out.insert(start);
            ends(x, s, start, out);
        }
        TokenPattern::Star(x) => {
            out.insert(start);
            iterate_body(x, s, [start].into(), out);
        }
        TokenPattern::Plus(x) => {
            let mut after_one = BTreeSet::new();
            ends(x, s, start, &mut after_one);
            for &p in &after_one {
                out.insert(p);
            }
            iterate_body(x, s, after_one, out);
        }
    }
}

/// Repeatedly apply `body` to a worklist of positions, accumulating reachable
/// end-positions in `out`. Termination is guaranteed because every iteration
/// only enqueues positions newly inserted into `out`, and `out` is bounded
/// by `[0, s.len()]`.
fn iterate_body(
    body: &TokenPattern,
    s: &[u8],
    mut frontier: BTreeSet<usize>,
    out: &mut BTreeSet<usize>,
) {
    while !frontier.is_empty() {
        let mut new = BTreeSet::new();
        for &p in &frontier {
            let mut step = BTreeSet::new();
            ends(body, s, p, &mut step);
            for q in step {
                if out.insert(q) {
                    new.insert(q);
                }
            }
        }
        frontier = new;
    }
}

fn resolve_pattern(p: &TokenPattern, g: &Grammar) -> TokenPattern {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => p.clone(),
        TokenPattern::Ref(n) => match g.tokens.get(n) {
            Some(td) => resolve_pattern(&td.pattern, g),
            None => TokenPattern::Empty,
        },
        TokenPattern::Seq(xs) => {
            TokenPattern::Seq(xs.iter().map(|x| resolve_pattern(x, g)).collect())
        }
        TokenPattern::Alt(xs) => {
            TokenPattern::Alt(xs.iter().map(|x| resolve_pattern(x, g)).collect())
        }
        TokenPattern::Opt(x) => TokenPattern::Opt(Box::new(resolve_pattern(x, g))),
        TokenPattern::Star(x) => TokenPattern::Star(Box::new(resolve_pattern(x, g))),
        TokenPattern::Plus(x) => TokenPattern::Plus(Box::new(resolve_pattern(x, g))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> TokenPattern {
        TokenPattern::Literal(s.into())
    }

    fn class_range(lo: char, hi: char) -> TokenPattern {
        TokenPattern::Class(CharClass {
            negated: false,
            items: vec![ClassItem::Range(lo as u32, hi as u32)],
        })
    }

    fn alpha_plus() -> TokenPattern {
        TokenPattern::Plus(Box::new(class_range('a', 'z')))
    }

    #[test]
    fn literal_matches_itself() {
        assert!(pattern_matches_exactly(&lit("if"), b"if"));
        assert!(!pattern_matches_exactly(&lit("if"), b"i"));
        assert!(!pattern_matches_exactly(&lit("if"), b"iff"));
    }

    #[test]
    fn class_plus_accepts_keyword_string() {
        assert!(pattern_matches_exactly(&alpha_plus(), b"if"));
        assert!(pattern_matches_exactly(&alpha_plus(), b"true"));
        assert!(!pattern_matches_exactly(&alpha_plus(), b"if!"));
        assert!(!pattern_matches_exactly(&alpha_plus(), b""));
    }

    #[test]
    fn star_accepts_empty_and_repeats() {
        let p = TokenPattern::Star(Box::new(lit("ab")));
        assert!(pattern_matches_exactly(&p, b""));
        assert!(pattern_matches_exactly(&p, b"ab"));
        assert!(pattern_matches_exactly(&p, b"abab"));
        assert!(!pattern_matches_exactly(&p, b"aba"));
    }

    #[test]
    fn nested_star_terminates_on_empty_body() {
        // Star(Star(_)) — inner can match empty; outer must not loop.
        let p = TokenPattern::Star(Box::new(TokenPattern::Star(Box::new(lit("x")))));
        assert!(pattern_matches_exactly(&p, b""));
        assert!(pattern_matches_exactly(&p, b"xxx"));
        assert!(!pattern_matches_exactly(&p, b"y"));
    }

    #[test]
    fn alt_picks_any_branch() {
        let p = TokenPattern::Alt(vec![lit(".."), lit("...")]);
        assert!(pattern_matches_exactly(&p, b".."));
        assert!(pattern_matches_exactly(&p, b"..."));
        assert!(!pattern_matches_exactly(&p, b"."));
    }

    fn tok(name: &str, pat: TokenPattern) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: pat,
            skip: false,
            is_fragment: false,
            span: Default::default(),
        }
    }

    #[test]
    fn flags_keyword_after_ident() {
        let mut g = Grammar::default();
        g.add_token(tok("IDENT", alpha_plus()));
        g.add_token(tok("IF", lit("if")));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert_eq!(issues.len(), 1, "{:?}", issues);
        assert!(issues[0].message.contains("`IF`"));
        assert!(issues[0].message.contains("`IDENT`"));
    }

    #[test]
    fn accepts_keyword_before_ident() {
        let mut g = Grammar::default();
        g.add_token(tok("IF", lit("if")));
        g.add_token(tok("IDENT", alpha_plus()));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn flags_duplicate_literal_tokens() {
        let mut g = Grammar::default();
        g.add_token(tok("A", lit("x")));
        g.add_token(tok("B", lit("x")));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("`B`"));
    }

    #[test]
    fn dot_not_shadowed_by_dotdot() {
        // PUNC=".." declared first; DOT="." stays reachable.
        let mut g = Grammar::default();
        g.add_token(tok("PUNC", lit("..")));
        g.add_token(tok("DOT", lit(".")));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn ignores_skip_and_fragment_shadowers() {
        let mut g = Grammar::default();
        let mut frag = tok("_LETTERS", alpha_plus());
        frag.is_fragment = true;
        g.add_token(frag);
        let mut skip = tok("_SKIPPY", alpha_plus());
        skip.skip = true;
        skip.is_fragment = false;
        skip.name = "SKIPPY".into();
        g.add_token(skip);
        g.add_token(tok("IF", lit("if")));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        // Neither a fragment nor a skip token should count as a shadower.
        assert!(issues.is_empty(), "{:?}", issues);
    }

    #[test]
    fn resolves_fragment_in_shadower_pattern() {
        let mut g = Grammar::default();
        let mut frag = tok("_LETTER", class_range('a', 'z'));
        frag.is_fragment = true;
        g.add_token(frag);
        g.add_token(tok(
            "IDENT",
            TokenPattern::Plus(Box::new(TokenPattern::Ref("_LETTER".into()))),
        ));
        g.add_token(tok("IF", lit("if")));
        let mut issues = Vec::new();
        run(&g, &mut issues);
        assert_eq!(issues.len(), 1, "{:?}", issues);
    }
}
