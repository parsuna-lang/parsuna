use parsuna_rt::RuleKindEnum;

use crate::error::Error;
use crate::grammar::ir::*;
use crate::span::Span;

use super::generated::{self, Event, RuleKind, Token, TokenKind};

fn event_span(e: &Event<'_>) -> Span {
    match e {
        Event::Enter { pos, .. } | Event::Exit { pos, .. } => Span::point(*pos),
        Event::Token(t) => t.span,
        Event::Error(err) => err.span,
    }
}

/// Parse a `.parsuna` grammar into a [`Grammar`].
///
/// Returns a bag of errors rather than the first failure so callers can
/// show all syntactic issues at once. Semantic checks (undefined references,
/// left-recursion, etc.) happen later in [`crate::analysis::analyze`].
pub fn parse_grammar(source: &str) -> Result<Grammar, Vec<Error>> {
    let events = generated::parse_file_from_str(source);
    let mut r = Reader::new(events);
    let mut g = Grammar::default();

    r.expect_enter(RuleKind::File);
    while r.peek_enter() == Some(RuleKind::Decl) {
        read_decl(&mut r, &mut g);
    }
    r.expect_exit(RuleKind::File);

    if r.issues.is_empty() {
        Ok(g)
    } else {
        Err(r.issues)
    }
}

struct Reader<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    look: Option<Event<'a>>,
    issues: Vec<Error>,
}

impl<'a, I: Iterator<Item = Event<'a>>> Reader<'a, I> {
    fn new(mut inner: I) -> Self {
        let look = pull_significant(&mut inner);
        Self {
            inner,
            look,
            issues: Vec::new(),
        }
    }

    fn peek(&self) -> Option<&Event<'a>> {
        self.look.as_ref()
    }

    fn advance(&mut self) -> Option<Event<'a>> {
        let ret = self.look.take();
        self.look = pull_significant(&mut self.inner);
        ret
    }

    fn peek_enter(&self) -> Option<RuleKind> {
        match self.peek()? {
            Event::Enter { rule, .. } => Some(*rule),
            _ => None,
        }
    }

    fn expect_enter(&mut self, want: RuleKind) {
        match self.peek() {
            Some(Event::Enter { rule, .. }) if *rule == want => {
                self.advance();
            }
            other => {
                let span = other.map(event_span).unwrap_or_default();
                self.issues
                    .push(Error::new(format!("expected Enter({})", want.name())).at(span));
            }
        }
    }

    fn expect_exit(&mut self, want: RuleKind) {
        match self.peek() {
            Some(Event::Exit { rule, .. }) if *rule == want => {
                self.advance();
            }
            other => {
                let span = other.map(event_span).unwrap_or_default();
                self.issues
                    .push(Error::new(format!("expected Exit({})", want.name())).at(span));
            }
        }
    }

    fn next_token(&mut self) -> Token<'a> {
        loop {
            match self.advance() {
                Some(Event::Token(t)) => return t,
                Some(Event::Error(d)) => self.issues.push(d),
                Some(Event::Enter { pos, .. }) | Some(Event::Exit { pos, .. }) => {
                    let span = Span::point(pos);
                    self.issues
                        .push(Error::new("expected a token, got a structural mark").at(span));
                    return Token {
                        kind: TokenKind::Error,
                        span,
                        text: std::borrow::Cow::Borrowed(""),
                    };
                }
                None => {
                    return Token {
                        kind: TokenKind::Eof,
                        span: Span::default(),
                        text: std::borrow::Cow::Borrowed(""),
                    }
                }
            }
        }
    }

    fn eat_token(&mut self, want_kind: TokenKind) -> Option<Token<'a>> {
        match self.peek() {
            Some(Event::Token(t)) if t.kind == want_kind => {
                if let Some(Event::Token(t)) = self.advance() {
                    Some(t)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn pull_significant<'a, I: Iterator<Item = Event<'a>>>(inner: &mut I) -> Option<Event<'a>> {
    loop {
        match inner.next()? {
            Event::Token(t) if is_skip(t.kind) => continue,
            other => return Some(other),
        }
    }
}

fn is_skip(kind: TokenKind) -> bool {
    kind == TokenKind::Ws || kind == TokenKind::Comment
}

fn read_decl<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>, g: &mut Grammar) {
    r.expect_enter(RuleKind::Decl);

    // Case conventions disambiguate tokens from rules: the first letter
    // (skipping any leading `_` fragment marker) must be uppercase for a
    // token and lowercase for a rule. The `?` prefix, when present, marks
    // a skip token and must therefore only be applied to tokens.
    let skip = r.eat_token(TokenKind::Question).is_some();
    let name_tok = r.next_token();
    let name: String = name_tok.text.clone().into_owned();
    let is_fragment = name.starts_with('_');
    let initial = initial_letter(&name);
    let is_token = initial.map_or(false, |c| c.is_ascii_uppercase());

    let _eq = r.next_token();

    let (expr, pattern) = if is_token {
        (None, Some(read_pattern_alt(r)))
    } else {
        (Some(read_alt(r)), None)
    };
    let semi = r.next_token();
    r.expect_exit(RuleKind::Decl);

    let decl_span = Span::join(name_tok.span, semi.span);

    if is_token {
        if skip && is_fragment {
            r.issues.push(
                Error::new(format!(
                    "token `{}` is both `?`-prefixed (skip) and `_`-prefixed (fragment); pick one",
                    name
                ))
                .at(name_tok.span),
            );
        }
        g.tokens.push(TokenDef {
            name,
            pattern: pattern.unwrap_or(TokenPattern::Empty),
            skip,
            is_fragment,
            span: decl_span,
        });
    } else if initial.is_some() {
        if skip {
            r.issues.push(
                Error::new(format!(
                    "`?` marks a skip token and only applies to tokens, not rules; drop it on `{}`",
                    name
                ))
                .at(name_tok.span),
            );
        }
        g.rules.push(RuleDef {
            name,
            body: expr.unwrap_or(Expr::Empty),
            is_fragment,
            span: decl_span,
        });
    } else {
        r.issues.push(
            Error::new(format!(
                "declaration name `{}` has no letter to determine kind",
                name
            ))
            .at(name_tok.span),
        );
    }
}

fn initial_letter(name: &str) -> Option<char> {
    name.chars().find(|&c| c != '_')
}

fn read_alt<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
    r.expect_enter(RuleKind::AltExpr);
    let mut xs = vec![read_seq(r)];
    while r.eat_token(TokenKind::Pipe).is_some() {
        xs.push(read_seq(r));
    }
    r.expect_exit(RuleKind::AltExpr);
    Expr::alt(xs)
}

fn read_seq<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
    r.expect_enter(RuleKind::SeqExpr);
    let mut xs: Vec<Expr> = Vec::new();
    loop {
        match r.peek_enter() {
            Some(RuleKind::Atom) => xs.push(read_primary_expr(r)),
            Some(RuleKind::Group) => xs.push(read_group_expr(r)),
            _ => break,
        }
    }
    r.expect_exit(RuleKind::SeqExpr);
    Expr::seq(xs)
}

fn read_primary_expr<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
    r.expect_enter(RuleKind::Atom);

    let x = match r.peek_enter() {
        Some(RuleKind::CharPrimary) => {
            let at = r.peek().map(event_span).unwrap_or_default();
            skip_until_exit(r, RuleKind::CharPrimary);
            r.issues.push(
                Error::new(
                    "character atom (char, range, or `.`) is only valid inside a token declaration",
                )
                .at(at),
            );
            Expr::Empty
        }
        Some(RuleKind::NegClass) => {
            let at = r.peek().map(event_span).unwrap_or_default();
            skip_until_exit(r, RuleKind::NegClass);
            r.issues.push(
                Error::new("`!` character negation is only valid inside a token declaration")
                    .at(at),
            );
            Expr::Empty
        }
        _ => {
            let tok = r.next_token();
            match tok.kind {
                k if k == TokenKind::Ident => {
                    if initial_letter(&tok.text).map_or(false, |c| c.is_ascii_uppercase()) {
                        Expr::Token(tok.text.into_owned())
                    } else {
                        Expr::Rule(tok.text.into_owned())
                    }
                }
                k if k == TokenKind::String => {
                    r.issues.push(
                        Error::new("string literal atoms are only valid inside token declarations")
                            .at(tok.span),
                    );
                    Expr::Empty
                }
                _ => {
                    r.issues.push(
                        Error::new(format!("unexpected atom token `{}`", tok.text)).at(tok.span),
                    );
                    Expr::Empty
                }
            }
        }
    };
    r.expect_exit(RuleKind::Atom);
    apply_quantifiers(r, x)
}

fn skip_until_exit<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>, kind: RuleKind) {
    r.expect_enter(kind);
    let mut depth: i32 = 1;
    while depth > 0 {
        match r.advance() {
            Some(Event::Enter { rule, .. }) if rule == kind => depth += 1,
            Some(Event::Exit { rule, .. }) if rule == kind => depth -= 1,
            Some(_) => {}
            None => break,
        }
    }
}

fn read_group_expr<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
    r.expect_enter(RuleKind::Group);
    let _lparen = r.next_token();
    let inner = read_alt(r);
    let _rparen = r.next_token();
    r.expect_exit(RuleKind::Group);
    apply_quantifiers(r, inner)
}

fn apply_quantifiers<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    mut x: Expr,
) -> Expr {
    loop {
        match r.peek() {
            Some(Event::Token(t)) if t.kind == TokenKind::Question => {
                r.advance();
                x = Expr::Opt(Box::new(x));
            }
            Some(Event::Token(t)) if t.kind == TokenKind::Star => {
                r.advance();
                x = Expr::Star(Box::new(x));
            }
            Some(Event::Token(t)) if t.kind == TokenKind::Plus => {
                r.advance();
                x = Expr::Plus(Box::new(x));
            }
            _ => break,
        }
    }
    x
}

fn read_pattern_alt<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::AltExpr);
    let mut xs = vec![read_pattern_seq(r)];
    while r.eat_token(TokenKind::Pipe).is_some() {
        xs.push(read_pattern_seq(r));
    }
    r.expect_exit(RuleKind::AltExpr);
    TokenPattern::alt(xs)
}

fn read_pattern_seq<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::SeqExpr);
    let mut xs: Vec<TokenPattern> = Vec::new();
    loop {
        match r.peek_enter() {
            Some(RuleKind::Atom) => xs.push(read_pattern_primary(r)),
            Some(RuleKind::Group) => xs.push(read_pattern_group(r)),
            _ => break,
        }
    }
    r.expect_exit(RuleKind::SeqExpr);
    TokenPattern::seq(xs)
}

fn read_pattern_primary<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::Atom);
    let p = match r.peek_enter() {
        Some(RuleKind::CharPrimary) => read_char_primary(r),

        Some(RuleKind::NegClass) => read_neg_class(r),

        _ => {
            let tok = r.next_token();
            match tok.kind {
                k if k == TokenKind::Ident => TokenPattern::Ref(tok.text.into_owned()),
                k if k == TokenKind::String => {
                    TokenPattern::Literal(unquote_string(&tok.text, tok.span, &mut r.issues))
                }
                _ => {
                    r.issues.push(
                        Error::new(format!("unexpected atom token `{}`", tok.text)).at(tok.span),
                    );
                    TokenPattern::Empty
                }
            }
        }
    };
    r.expect_exit(RuleKind::Atom);
    apply_pattern_quantifiers(r, p)
}

fn read_char_primary<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::CharPrimary);
    let first = r.next_token();
    let p = if first.kind == TokenKind::Dot {
        TokenPattern::Class(CharClass {
            negated: true,
            items: Vec::new(),
        })
    } else if first.kind == TokenKind::Char {
        let lo = unquote_char(&first.text, first.span, &mut r.issues);
        if r.eat_token(TokenKind::Dotdot).is_some() {
            let hi_tok = r.next_token();
            let hi = unquote_char(&hi_tok.text, hi_tok.span, &mut r.issues);
            TokenPattern::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Range(lo, hi)],
            })
        } else {
            let ch = char::from_u32(lo).unwrap_or('\0');
            let mut buf = String::new();
            buf.push(ch);
            TokenPattern::Literal(buf)
        }
    } else {
        r.issues
            .push(Error::new(format!("unexpected atom token `{}`", first.text)).at(first.span));
        TokenPattern::Empty
    };
    r.expect_exit(RuleKind::CharPrimary);
    p
}

fn read_neg_class<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::NegClass);
    let bang = r.next_token();
    let mut items: Vec<ClassItem> = Vec::new();

    if r.eat_token(TokenKind::Lparen).is_some() {
        loop {
            collect_class_items(r, &mut items, bang.span);
            if r.eat_token(TokenKind::Pipe).is_none() {
                break;
            }
        }
        r.eat_token(TokenKind::Rparen);
    } else {
        collect_class_items(r, &mut items, bang.span);
    }
    r.expect_exit(RuleKind::NegClass);
    TokenPattern::Class(CharClass {
        negated: true,
        items,
    })
}

fn collect_class_items<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    out: &mut Vec<ClassItem>,
    fallback_span: Span,
) {
    let p = read_char_primary(r);
    match p {
        TokenPattern::Class(c) if c.negated && c.items.is_empty() => {
            r.issues.push(
                Error::new("cannot negate `.` (any char); the resulting set is empty")
                    .at(fallback_span),
            );
        }
        TokenPattern::Class(c) => out.extend(c.items),
        TokenPattern::Literal(s) => {
            let mut it = s.chars();
            if let Some(ch) = it.next() {
                out.push(ClassItem::Char(ch as u32));
            }
        }
        _ => {
            r.issues
                .push(Error::new("expected a character primary in negation").at(fallback_span));
        }
    }
}

fn unquote_char(lit: &str, span: Span, issues: &mut Vec<Error>) -> u32 {
    let inner = &lit[1..lit.len() - 1];
    let mut chars = inner.chars();
    let first = chars.next();
    if first != Some('\\') {
        return match first {
            Some(c) => c as u32,
            None => {
                issues.push(Error::new("empty char literal").at(span));
                0
            }
        };
    }
    match chars.next() {
        Some('n') => '\n' as u32,
        Some('r') => '\r' as u32,
        Some('t') => '\t' as u32,
        Some('\\') => '\\' as u32,
        Some('\'') => '\'' as u32,
        Some('"') => '"' as u32,
        Some('0') => 0,
        Some('u') => parse_unicode_escape(&mut chars, span, issues),
        Some(c) => {
            issues.push(Error::new(format!("unknown escape `\\{}`", c)).at(span));
            0
        }
        None => {
            issues.push(Error::new("dangling backslash in char literal").at(span));
            0
        }
    }
}

fn parse_unicode_escape(
    chars: &mut std::str::Chars<'_>,
    span: Span,
    issues: &mut Vec<Error>,
) -> u32 {
    if chars.next() != Some('{') {
        issues.push(Error::new(r"expected `{` after `\u` in escape").at(span));
        return 0;
    }
    let mut cp: u32 = 0;
    let mut digits = 0usize;
    loop {
        match chars.next() {
            Some('}') => break,
            Some(c) => {
                let Some(d) = c.to_digit(16) else {
                    issues.push(
                        Error::new(format!("invalid hex digit `{}` in \\u{{...}} escape", c))
                            .at(span),
                    );
                    return 0;
                };

                cp = cp.saturating_mul(16).saturating_add(d);
                digits += 1;
                if digits > 6 {
                    issues.push(
                        Error::new(r"\u{...} escape has too many hex digits (max 6)").at(span),
                    );
                    return 0;
                }
            }
            None => {
                issues.push(Error::new(r"unterminated \u{...} escape").at(span));
                return 0;
            }
        }
    }
    if digits == 0 {
        issues.push(Error::new(r"\u{} escape must contain at least one hex digit").at(span));
        return 0;
    }
    if char::from_u32(cp).is_none() {
        issues
            .push(Error::new(format!("\\u{{{:X}}} is not a valid Unicode codepoint", cp)).at(span));
        return 0;
    }
    cp
}

fn read_pattern_group<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> TokenPattern {
    r.expect_enter(RuleKind::Group);
    let _lparen = r.next_token();
    let inner = read_pattern_alt(r);
    let _rparen = r.next_token();
    r.expect_exit(RuleKind::Group);
    apply_pattern_quantifiers(r, inner)
}

fn apply_pattern_quantifiers<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    mut p: TokenPattern,
) -> TokenPattern {
    loop {
        match r.peek() {
            Some(Event::Token(t)) if t.kind == TokenKind::Question => {
                r.advance();
                p = TokenPattern::Opt(Box::new(p));
            }
            Some(Event::Token(t)) if t.kind == TokenKind::Star => {
                r.advance();
                p = TokenPattern::Star(Box::new(p));
            }
            Some(Event::Token(t)) if t.kind == TokenKind::Plus => {
                r.advance();
                p = TokenPattern::Plus(Box::new(p));
            }
            _ => break,
        }
    }
    p
}

fn unquote_string(lit: &str, span: Span, issues: &mut Vec<Error>) -> String {
    let inner = &lit[1..lit.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some('u') => {
                let cp = parse_unicode_escape(&mut chars, span, issues);
                if let Some(c) = char::from_u32(cp) {
                    out.push(c);
                }
            }
            Some(c) => {
                issues.push(Error::new(format!("unknown escape `\\{}`", c)).at(span));
            }
            None => break,
        }
    }
    out
}
