use parsuna_rt::RuleKindEnum;

use crate::grammar::ir::*;
use crate::Span;
use parsuna_rt::Error;

use super::generated::{self, Event, RuleKind, Token, TokenKind};

fn event_span(e: &Event<'_>) -> Span {
    match e {
        Event::Enter { pos, .. } | Event::Exit { pos, .. } => Span::point(*pos),
        Event::Token(t) | Event::Garbage(t) => t.span,
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
    while r.peek_enter() == Some(RuleKind::Item) {
        read_item(&mut r, &mut g);
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
                Some(Event::Garbage(_)) => {
                    // Recovery skipped this token — keep walking.
                }
                Some(Event::Enter { pos, .. }) | Some(Event::Exit { pos, .. }) => {
                    let span = Span::point(pos);
                    self.issues
                        .push(Error::new("expected a token, got a structural mark").at(span));
                    return Token {
                        kind: None,
                        span,
                        text: std::borrow::Cow::Borrowed(""),
                        label: None,
                    };
                }
                None => {
                    return Token {
                        kind: Some(TokenKind::Eof),
                        span: Span::default(),
                        text: std::borrow::Cow::Borrowed(""),
                        label: None,
                    }
                }
            }
        }
    }

    fn eat_token(&mut self, want_kind: TokenKind) -> Option<Token<'a>> {
        match self.peek() {
            Some(Event::Token(t)) if t.kind == Some(want_kind) => {
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
            Event::Token(t) if t.kind.map_or(false, is_skip) => continue,
            other => return Some(other),
        }
    }
}

fn is_skip(kind: TokenKind) -> bool {
    kind == TokenKind::Ws || kind == TokenKind::Comment
}

/// One item at the file's top level: an optional `@mode(...)`
/// pre-annotation followed by a single decl. The annotation can list
/// multiple mode names — `@mode(a, b, c) TOK = …` registers `TOK` in
/// every listed mode, sharing one kind id across the per-mode DFAs.
fn read_item<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>, g: &mut Grammar) {
    r.expect_enter(RuleKind::Item);

    let modes = if r.peek_enter() == Some(RuleKind::ModePre) {
        read_mode_pre(r)
    } else {
        Vec::new()
    };
    read_decl(r, g, &modes);

    r.expect_exit(RuleKind::Item);
}

/// `mode_pre = AT IDENT LPAREN IDENT (COMMA IDENT)* RPAREN` — the only
/// annotation kind today is `@mode(...)`. Returns the listed mode
/// names. Other annotation names are recorded as an error; the names
/// inside the parens are still returned so we keep going.
fn read_mode_pre<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Vec<String> {
    r.expect_enter(RuleKind::ModePre);
    let _at = r.next_token();
    let name_tok = r.next_token();
    let _lparen = r.next_token();
    let mut args: Vec<String> = Vec::new();
    let first = r.next_token();
    args.push(first.text.into_owned());
    while r.eat_token(TokenKind::Comma).is_some() {
        let arg = r.next_token();
        args.push(arg.text.into_owned());
    }
    let _rparen = r.next_token();
    r.expect_exit(RuleKind::ModePre);

    let kind = name_tok.text.as_ref();
    if kind != "mode" {
        r.issues.push(
            Error::new(format!(
                "unknown pre-annotation `@{}`; supported pre-annotations: mode",
                kind
            ))
            .at(name_tok.span),
        );
    }
    args
}

fn read_decl<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    g: &mut Grammar,
    modes: &[String],
) {
    r.expect_enter(RuleKind::Decl);

    // Case conventions disambiguate tokens from rules: the first letter
    // (skipping any leading `_` fragment marker) must be uppercase for a
    // token and lowercase for a rule.
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

    let actions_raw = read_actions(r);
    let semi = r.next_token();
    r.expect_exit(RuleKind::Decl);

    let decl_span = Span::join(name_tok.span, semi.span);

    // Resolve `-> ...` actions into `(skip, mode_actions)`. Mode actions
    // are kept as a sequence (in source order) so combinations like
    // `-> pop, push(b)` (swap top) and `-> push(a), push(b)` (push two)
    // round-trip cleanly. The only forbidden combo is `skip` together
    // with any mode action.
    let mut skip = false;
    let mut skip_span: Option<Span> = None;
    let mut mode_actions: Vec<ModeAction> = Vec::new();
    let mut first_mode_action_span: Option<Span> = None;

    for raw in &actions_raw {
        match raw.name.as_str() {
            "skip" => {
                if raw.arg.is_some() {
                    r.issues.push(
                        Error::new("`skip` action takes no argument").at(raw.span),
                    );
                }
                if skip {
                    r.issues
                        .push(Error::new("duplicate `skip` action").at(raw.span));
                }
                skip = true;
                skip_span.get_or_insert(raw.span);
            }
            "push" => {
                let Some(arg) = raw.arg.clone() else {
                    r.issues.push(
                        Error::new("`push` action requires a mode name argument: `push(mode)`")
                            .at(raw.span),
                    );
                    continue;
                };
                mode_actions.push(ModeAction::Push(arg));
                first_mode_action_span.get_or_insert(raw.span);
            }
            "pop" => {
                if raw.arg.is_some() {
                    r.issues.push(
                        Error::new("`pop` action takes no argument").at(raw.span),
                    );
                }
                mode_actions.push(ModeAction::Pop);
                first_mode_action_span.get_or_insert(raw.span);
            }
            other => {
                r.issues.push(
                    Error::new(format!(
                        "unknown action `{}`; supported actions: skip, push(mode), pop",
                        other
                    ))
                    .at(raw.span),
                );
            }
        }
    }

    if skip && !mode_actions.is_empty() {
        let span = first_mode_action_span.or(skip_span).unwrap_or(decl_span);
        r.issues.push(
            Error::new("`skip` cannot be combined with `push` or `pop` on the same token")
                .at(span),
        );
    }

    if is_token {
        if skip && is_fragment {
            r.issues.push(
                Error::new(format!(
                    "token `{}` is both marked `skip` and `_`-prefixed (fragment); pick one",
                    name
                ))
                .at(name_tok.span),
            );
        }
        if g.tokens.contains_key(&name) {
            r.issues
                .push(Error::new(format!("duplicate token: {}", name)).at(decl_span));
        }
        g.add_token(TokenDef {
            name,
            pattern: pattern.unwrap_or(TokenPattern::Empty),
            skip,
            is_fragment,
            modes: if modes.is_empty() {
                vec!["default".to_string()]
            } else {
                modes.to_vec()
            },
            mode_actions,
            span: decl_span,
        });
    } else if initial.is_some() {
        if let Some(span) = skip_span {
            r.issues.push(
                Error::new(format!(
                    "action `skip` only applies to tokens, not rules; drop it on `{}`",
                    name
                ))
                .at(span),
            );
        }
        if let Some(span) = first_mode_action_span {
            r.issues.push(
                Error::new(format!(
                    "actions `push`/`pop` only apply to tokens, not rules; drop them on `{}`",
                    name
                ))
                .at(span),
            );
        }
        if !modes.is_empty() {
            r.issues.push(
                Error::new(format!(
                    "`@mode(...)` only applies to tokens, not rules; drop it on `{}`",
                    name
                ))
                .at(name_tok.span),
            );
        }
        if g.rules.contains_key(&name) {
            r.issues
                .push(Error::new(format!("duplicate rule: {}", name)).at(decl_span));
        }
        g.add_rule(RuleDef {
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

/// One raw `-> action` entry, before semantic resolution. The `arg`
/// captures the optional `(name)` payload (e.g. `push(mode_name)`).
struct RawAction {
    name: String,
    arg: Option<String>,
    span: Span,
}

/// `actions = ARROW action (COMMA action)*` — returns the parsed list, or
/// an empty Vec if no `->` appeared.
fn read_actions<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Vec<RawAction> {
    if r.peek_enter() != Some(RuleKind::Actions) {
        return Vec::new();
    }
    r.expect_enter(RuleKind::Actions);
    let _arrow = r.next_token();
    let mut out = Vec::new();
    while r.peek_enter() == Some(RuleKind::Action) {
        out.push(read_action(r));
        if r.eat_token(TokenKind::Comma).is_none() {
            break;
        }
    }
    r.expect_exit(RuleKind::Actions);
    out
}

/// `action = IDENT action_arg?`, `action_arg = LPAREN IDENT RPAREN`.
fn read_action<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> RawAction {
    r.expect_enter(RuleKind::Action);
    let name_tok = r.next_token();
    let arg = if r.peek_enter() == Some(RuleKind::ActionArg) {
        r.expect_enter(RuleKind::ActionArg);
        let _lparen = r.next_token();
        let arg_tok = r.next_token();
        let _rparen = r.next_token();
        r.expect_exit(RuleKind::ActionArg);
        Some(arg_tok.text.into_owned())
    } else {
        None
    };
    r.expect_exit(RuleKind::Action);
    RawAction {
        name: name_tok.text.into_owned(),
        arg,
        span: name_tok.span,
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
        // `_postfix_expr = LABEL? _primary_expr _quant_op*` — `_postfix_expr`
        // is a fragment, so we see its body inlined: an optional `LABEL`
        // token (an IDENT immediately followed by `:`) before the atom or
        // group. Label binds tighter than the quantifier — `name:A*`
        // parses as `(name:A)*` so each iteration of the Star produces a
        // labeled Token event.
        let label = peek_label(r);
        match r.peek_enter() {
            Some(RuleKind::Atom) => {
                let atom = read_primary_atom(r);
                let labeled = wrap_label(label, atom);
                xs.push(apply_quantifiers(r, labeled));
            }
            Some(RuleKind::Group) => {
                // For groups the existing helper applies quantifiers
                // internally; reuse it but wrap before quantifying by
                // splitting the group read.
                let group = read_group_inner(r);
                let labeled = wrap_label(label, group);
                xs.push(apply_quantifiers(r, labeled));
            }
            _ => {
                if let Some((name, span)) = label {
                    r.issues.push(
                        Error::new(format!(
                            "label `{}:` must be followed by an atom or group",
                            name
                        ))
                        .at(span),
                    );
                }
                break;
            }
        }
    }
    r.expect_exit(RuleKind::SeqExpr);
    Expr::seq(xs)
}

/// Peek for an optional `LABEL` token at the start of the next postfix.
/// Consumes it (and strips the trailing `:`) on a hit; returns `None`
/// otherwise.
fn peek_label<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
) -> Option<(String, Span)> {
    let is_label = matches!(
        r.peek(),
        Some(Event::Token(t)) if t.kind == Some(TokenKind::Label)
    );
    if !is_label {
        return None;
    }
    let tok = r.next_token();
    let span = tok.span;
    let mut text = tok.text.into_owned();
    // LABEL is `IDENT ":"` — drop the trailing colon.
    debug_assert!(text.ends_with(':'));
    text.pop();
    Some((text, span))
}

fn wrap_label(label: Option<(String, Span)>, body: Expr) -> Expr {
    match label {
        Some((name, _)) => Expr::Label(name, Box::new(body)),
        None => body,
    }
}

/// Read a single atom (Token / Rule / etc.) without consuming any
/// trailing `*`/`+`/`?` — those are applied by the caller after any
/// label wrapping, so labels bind tighter than quantifiers.
fn read_primary_atom<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
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
                k if k == Some(TokenKind::Ident) => {
                    if initial_letter(&tok.text).map_or(false, |c| c.is_ascii_uppercase()) {
                        Expr::Token(tok.text.into_owned())
                    } else {
                        Expr::Rule(tok.text.into_owned())
                    }
                }
                k if k == Some(TokenKind::String) => {
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
    x
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


/// Read the contents of a `(... )` group, leaving any trailing
/// quantifiers for the caller to apply. Splitting it lets the caller
/// wrap the bare group with a label before quantifying.
fn read_group_inner<'a, I: Iterator<Item = Event<'a>>>(r: &mut Reader<'a, I>) -> Expr {
    r.expect_enter(RuleKind::Group);
    let _lparen = r.next_token();
    let inner = read_alt(r);
    let _rparen = r.next_token();
    r.expect_exit(RuleKind::Group);
    inner
}

fn apply_quantifiers<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    mut x: Expr,
) -> Expr {
    loop {
        match r.peek() {
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Question) => {
                r.advance();
                x = Expr::Opt(Box::new(x));
            }
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Star) => {
                r.advance();
                x = Expr::Star(Box::new(x));
            }
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Plus) => {
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
                k if k == Some(TokenKind::Ident) => TokenPattern::Ref(tok.text.into_owned()),
                k if k == Some(TokenKind::String) => {
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
    let p = if first.kind == Some(TokenKind::Dot) {
        TokenPattern::Class(CharClass {
            negated: true,
            items: Vec::new(),
        })
    } else if first.kind == Some(TokenKind::Char) {
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
    let mut strings: Vec<String> = Vec::new();

    if r.eat_token(TokenKind::Lparen).is_some() {
        loop {
            collect_neg_atom(r, &mut items, &mut strings, bang.span);
            if r.eat_token(TokenKind::Pipe).is_none() {
                break;
            }
        }
        r.eat_token(TokenKind::Rparen);
    } else {
        collect_neg_atom(r, &mut items, &mut strings, bang.span);
    }
    r.expect_exit(RuleKind::NegClass);
    if strings.is_empty() {
        TokenPattern::Class(CharClass {
            negated: true,
            items,
        })
    } else {
        // Reject ranges in NegLook chars — the AC trie compiler can't
        // handle ranges-on-each-position; ranges'd have to expand to
        // per-codepoint patterns, which can blow up. Single chars and
        // string literals are fine.
        if items
            .iter()
            .any(|it| matches!(it, ClassItem::Range(_, _)))
        {
            r.issues.push(
                Error::new(
                    "character ranges are not supported alongside string atoms inside `!(...)`; \
                     split the negation into a separate `!('a'..'z')` group",
                )
                .at(bang.span),
            );
        }
        TokenPattern::NegLook {
            chars: CharClass {
                negated: true,
                items,
            },
            strings,
        }
    }
}

/// Read one `_neg_atom` from the event stream, folding it into either
/// `items` (single-codepoint atoms — chars, ranges, dot, single-codepoint
/// strings) or `strings` (multi-codepoint string literals).
fn collect_neg_atom<'a, I: Iterator<Item = Event<'a>>>(
    r: &mut Reader<'a, I>,
    items: &mut Vec<ClassItem>,
    strings: &mut Vec<String>,
    fallback_span: Span,
) {
    // `_neg_atom` is a fragment, so its events are inlined: we see
    // either `Enter(CharPrimary)` ... or a bare `Token(STRING)` here.
    if let Some(Event::Token(t)) = r.peek() {
        if t.kind == Some(TokenKind::String) {
            let tok = r.next_token();
            let text_owned = tok.text.clone().into_owned();
            let s = unquote_string(&text_owned, tok.span, &mut r.issues);
            // Single-codepoint strings collapse into the chars set —
            // `!"x"` is identical to `!'x'`. Empty strings can't be
            // negated meaningfully.
            let mut iter = s.chars();
            let first = iter.next();
            let second = iter.next();
            match (first, second) {
                (None, _) => {
                    r.issues.push(
                        Error::new(
                            "cannot negate empty string; an empty literal can never start at a position",
                        )
                        .at(tok.span),
                    );
                }
                (Some(ch), None) => {
                    items.push(ClassItem::Char(ch as u32));
                }
                (Some(_), Some(_)) => {
                    strings.push(s);
                }
            }
            return;
        }
    }
    let p = read_char_primary(r);
    match p {
        TokenPattern::Class(c) if c.negated && c.items.is_empty() => {
            r.issues.push(
                Error::new("cannot negate `.` (any char); the resulting set is empty")
                    .at(fallback_span),
            );
        }
        TokenPattern::Class(c) => items.extend(c.items),
        TokenPattern::Literal(s) => {
            let mut it = s.chars();
            if let Some(ch) = it.next() {
                items.push(ClassItem::Char(ch as u32));
            }
        }
        _ => {
            r.issues
                .push(Error::new("expected a character primary or string in negation").at(fallback_span));
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
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Question) => {
                r.advance();
                p = TokenPattern::Opt(Box::new(p));
            }
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Star) => {
                r.advance();
                p = TokenPattern::Star(Box::new(p));
            }
            Some(Event::Token(t)) if t.kind == Some(TokenKind::Plus) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_token_and_rule() {
        let g = parse_grammar("A = \"a\"; main = A;").expect("ok");
        assert_eq!(g.tokens.len(), 1);
        let a = g.tokens.get("A").expect("A");
        assert!(matches!(a.pattern, TokenPattern::Literal(ref s) if s == "a"));
        assert_eq!(g.rules.len(), 1);
        let main = g.rules.get("main").expect("main");
        assert!(matches!(main.body, Expr::Token(ref n) if n == "A"));
    }

    #[test]
    fn skip_action_marks_token_as_skip() {
        let g = parse_grammar("WS = \" \"+ -> skip; T = \"t\"; main = T;").expect("ok");
        let ws = g.tokens.get("WS").expect("WS");
        assert!(ws.skip);
        assert!(!ws.is_fragment);
    }

    #[test]
    fn unknown_action_is_rejected() {
        let errs = parse_grammar("WS = \" \"+ -> bogus; T = \"t\"; main = T;")
            .err()
            .expect("err");
        assert!(errs.iter().any(|e| e.message.contains("unknown action")));
    }

    #[test]
    fn underscore_prefix_marks_fragment() {
        let g = parse_grammar("_DIGIT = '0'..'9'; NUM = _DIGIT+; main = NUM;").expect("ok");
        let frag = g.tokens.get("_DIGIT").expect("_DIGIT");
        assert!(frag.is_fragment);
        assert!(!frag.skip);
    }

    #[test]
    fn skip_and_fragment_combo_rejected() {
        let errs = parse_grammar("_X = \"x\" -> skip; T = \"t\"; main = T;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("skip") && e.message.contains("fragment")));
    }

    #[test]
    fn skip_on_rule_is_rejected() {
        let errs = parse_grammar("T = \"t\"; main = T -> skip;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("only applies to tokens")));
    }

    #[test]
    fn quantifiers_become_opt_star_plus_in_rule_body() {
        let g = parse_grammar("A = \"a\"; main = A? A* A+;").expect("ok");
        let body = &g.rules.get("main").expect("main").body;
        let xs = match body {
            Expr::Seq(xs) => xs,
            other => panic!("expected Seq, got {:?}", other),
        };
        assert!(matches!(xs[0], Expr::Opt(_)));
        assert!(matches!(xs[1], Expr::Star(_)));
        assert!(matches!(xs[2], Expr::Plus(_)));
    }

    #[test]
    fn alt_with_pipe_in_rule_body() {
        let g = parse_grammar("A = \"a\"; B = \"b\"; main = A | B;").expect("ok");
        let main = g.rules.get("main").expect("main");
        assert!(matches!(main.body, Expr::Alt(ref xs) if xs.len() == 2));
    }

    #[test]
    fn group_parentheses_in_token_pattern() {
        let g = parse_grammar("T = (\"a\" | \"b\")+; main = T;").expect("ok");
        let pat = &g.tokens.get("T").unwrap().pattern;
        assert!(matches!(pat, TokenPattern::Plus(_)));
    }

    #[test]
    fn char_range_pattern() {
        let g = parse_grammar("D = '0'..'9'; main = D;").expect("ok");
        let pat = &g.tokens.get("D").unwrap().pattern;
        match pat {
            TokenPattern::Class(cc) => {
                assert!(!cc.negated);
                assert_eq!(cc.items.len(), 1);
                assert!(matches!(cc.items[0], ClassItem::Range(0x30, 0x39)));
            }
            _ => panic!("expected Class, got {:?}", pat),
        }
    }

    #[test]
    fn dot_atom_means_negated_empty_class_any_char() {
        let g = parse_grammar("ANY = .; main = ANY;").expect("ok");
        let pat = &g.tokens.get("ANY").unwrap().pattern;
        match pat {
            TokenPattern::Class(cc) => {
                assert!(cc.negated);
                assert!(cc.items.is_empty());
            }
            _ => panic!("expected negated class, got {:?}", pat),
        }
    }

    #[test]
    fn negated_class_with_bang() {
        let g = parse_grammar("X = !'a'; main = X;").expect("ok");
        let pat = &g.tokens.get("X").unwrap().pattern;
        match pat {
            TokenPattern::Class(cc) => {
                assert!(cc.negated);
                assert_eq!(cc.items.len(), 1);
                assert!(matches!(cc.items[0], ClassItem::Char(0x61)));
            }
            _ => panic!("expected negated class, got {:?}", pat),
        }
    }

    #[test]
    fn unicode_escape_in_char_literal() {
        let g = parse_grammar("X = '\\u{0041}'; main = X;").expect("ok");
        let pat = &g.tokens.get("X").unwrap().pattern;
        assert!(matches!(pat, TokenPattern::Literal(ref s) if s == "A"));
    }

    #[test]
    fn string_atom_in_rule_body_is_rejected() {
        let errs = parse_grammar("T = \"t\"; main = \"t\";")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("string literal atoms")));
    }

    #[test]
    fn char_atom_in_rule_body_is_rejected() {
        let errs = parse_grammar("T = \"t\"; main = 'a';").err().expect("err");
        assert!(errs.iter().any(|e| e.message.contains("character atom")));
    }

    #[test]
    fn duplicate_token_and_rule_names_flagged() {
        let errs = parse_grammar("T = \"t\"; T = \"u\"; r = T; r = T;")
            .err()
            .expect("err");
        assert!(errs.iter().any(|e| e.message == "duplicate token: T"));
        assert!(errs.iter().any(|e| e.message == "duplicate rule: r"));
    }

    #[test]
    fn empty_source_yields_empty_grammar() {
        let g = parse_grammar("").expect("ok");
        assert!(g.tokens.is_empty());
        assert!(g.rules.is_empty());
    }

    #[test]
    fn comments_and_whitespace_are_skipped() {
        let src = "
            // a leading comment
            A = \"a\"; // trailing
            // between
            main = A;
        ";
        let g = parse_grammar(src).expect("ok");
        assert_eq!(g.tokens.len(), 1);
        assert!(g.tokens.get("A").is_some());
        assert_eq!(g.rules.len(), 1);
        assert!(g.rules.get("main").is_some());
    }

    #[test]
    fn multiple_decls_preserve_source_order() {
        let g = parse_grammar("Z = \"z\"; A = \"a\"; second = A; first = Z;").expect("ok");
        let token_names: Vec<&str> = g.tokens.values().map(|t| t.name.as_str()).collect();
        let rule_names: Vec<&str> = g.rules.values().map(|r| r.name.as_str()).collect();
        assert_eq!(token_names, vec!["Z", "A"]);
        assert_eq!(rule_names, vec!["second", "first"]);
    }

    #[test]
    fn negated_class_with_grouped_alternatives() {
        let g = parse_grammar("X = !('a' | 'b'); main = X;").expect("ok");
        let pat = &g.tokens.get("X").unwrap().pattern;
        match pat {
            TokenPattern::Class(cc) => {
                assert!(cc.negated);
                assert_eq!(cc.items.len(), 2);
            }
            _ => panic!("expected negated class, got {:?}", pat),
        }
    }

    // ----- NegLook (string negation) -----------------------------------

    #[test]
    fn neg_string_under_star_yields_neg_look() {
        let g = parse_grammar(r#"BLOCK = "/*" !"*/"* "*/"; main = BLOCK;"#).expect("ok");
        let pat = &g.tokens.get("BLOCK").unwrap().pattern;
        // Walk to the Star inside the Seq.
        let TokenPattern::Seq(xs) = pat else {
            panic!("expected Seq, got {:?}", pat)
        };
        let TokenPattern::Star(inner) = &xs[1] else {
            panic!("expected Star at position 1, got {:?}", xs[1])
        };
        match inner.as_ref() {
            TokenPattern::NegLook { chars, strings } => {
                assert!(chars.negated);
                assert!(chars.items.is_empty());
                assert_eq!(strings, &vec!["*/".to_string()]);
            }
            other => panic!("expected NegLook, got {:?}", other),
        }
    }

    #[test]
    fn neg_string_grouped_with_chars_yields_neg_look() {
        let g = parse_grammar(r#"T = !("*/" | '\n')*; B = "x"; main = T B;"#).expect("ok");
        let pat = &g.tokens.get("T").unwrap().pattern;
        let TokenPattern::Star(inner) = pat else {
            panic!("expected Star, got {:?}", pat)
        };
        match inner.as_ref() {
            TokenPattern::NegLook { chars, strings } => {
                assert!(chars.negated);
                assert_eq!(chars.items.len(), 1); // '\n'
                assert_eq!(strings, &vec!["*/".to_string()]);
            }
            other => panic!("expected NegLook, got {:?}", other),
        }
    }

    #[test]
    fn single_codepoint_string_in_neg_collapses_to_chars() {
        // !"x"  is identical to !'x'  — both produce a Class, not a NegLook.
        let g = parse_grammar(r#"T = !"x"*; B = "y"; main = T B;"#).expect("ok");
        let pat = &g.tokens.get("T").unwrap().pattern;
        let TokenPattern::Star(inner) = pat else {
            panic!("expected Star, got {:?}", pat)
        };
        match inner.as_ref() {
            TokenPattern::Class(cc) => {
                assert!(cc.negated);
                assert_eq!(cc.items.len(), 1);
                assert!(matches!(cc.items[0], ClassItem::Char(0x78)));
            }
            other => panic!("expected Class, got {:?}", other),
        }
    }

    #[test]
    fn empty_neg_string_is_rejected() {
        let errs = parse_grammar(r#"T = !""*; B = "x"; main = T B;"#)
            .err()
            .expect("err");
        assert!(
            errs.iter().any(|e| e.message.contains("empty string")),
            "diagnostics: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn neg_string_with_range_atom_is_rejected() {
        // Mixing a string with a char range inside the same `!(...)` is
        // refused — the AC trie can't handle ranges-on-each-position.
        let errs = parse_grammar(r#"T = !('a'..'z' | "abc")*; B = "x"; main = T B;"#)
            .err()
            .expect("err");
        assert!(
            errs.iter().any(|e| e.message.contains("ranges are not supported")),
            "diagnostics: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn string_with_escape_unquotes_correctly() {
        let g = parse_grammar(r#"NL = "\n"; main = NL;"#).expect("ok");
        let pat = &g.tokens.get("NL").unwrap().pattern;
        assert!(matches!(pat, TokenPattern::Literal(ref s) if s == "\n"));
    }

    // ----- Modes & action syntax ---------------------------------------

    #[test]
    fn inline_mode_pre_tags_single_token() {
        let g = parse_grammar(
            "@mode(tag) NAME = \"x\"; OUTSIDE = \"y\"; main = NAME OUTSIDE;",
        )
        .expect("ok");
        assert_eq!(g.tokens.get("NAME").unwrap().modes, vec!["tag".to_string()]);
        assert_eq!(g.tokens.get("OUTSIDE").unwrap().modes, vec!["default".to_string()]);
    }

    // ----- Labels ------------------------------------------------------

    #[test]
    fn labeled_token_yields_label_expr() {
        let g = parse_grammar("A = \"a\"; B = \"b\"; r = name:A B;").expect("ok");
        let body = &g.rules.get("r").unwrap().body;
        let xs = match body {
            Expr::Seq(xs) => xs,
            other => panic!("expected Seq, got {:?}", other),
        };
        match &xs[0] {
            Expr::Label(name, inner) => {
                assert_eq!(name, "name");
                assert!(matches!(inner.as_ref(), Expr::Token(n) if n == "A"));
            }
            other => panic!("expected Label, got {:?}", other),
        }
        assert!(matches!(&xs[1], Expr::Token(n) if n == "B"));
    }

    #[test]
    fn label_with_quantifier_wraps_inner() {
        // `name:A*` parses as `(name:A)*` — label binds tighter than `*`.
        let g = parse_grammar("A = \"a\"; r = name:A*;").expect("ok");
        let body = &g.rules.get("r").unwrap().body;
        match body {
            Expr::Star(inner) => match inner.as_ref() {
                Expr::Label(name, leaf) => {
                    assert_eq!(name, "name");
                    assert!(matches!(leaf.as_ref(), Expr::Token(n) if n == "A"));
                }
                other => panic!("expected Star(Label), got Star({:?})", other),
            },
            other => panic!("expected Star, got {:?}", other),
        }
    }

    #[test]
    fn mode_pre_with_multiple_modes_records_each() {
        let g = parse_grammar(
            "@mode(a, b, c) X = \"x\"; @mode(a) Y = \"y\"; main = X Y;",
        )
        .expect("ok");
        assert_eq!(
            g.tokens.get("X").unwrap().modes,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(g.tokens.get("Y").unwrap().modes, vec!["a".to_string()]);
    }

    #[test]
    fn mode_pre_repeats_per_decl() {
        // Mode is a per-token attribute, not a scope. Applying it to
        // multiple decls is plain repetition.
        let src = r#"
            @mode(tag) NAME = "x";
            @mode(tag) EQ_T = "=";
            OUTSIDE = "o";
            main = NAME EQ_T OUTSIDE;
        "#;
        let g = parse_grammar(src).expect("ok");
        assert_eq!(g.tokens.get("NAME").unwrap().modes, vec!["tag".to_string()]);
        assert_eq!(g.tokens.get("EQ_T").unwrap().modes, vec!["tag".to_string()]);
        assert_eq!(g.tokens.get("OUTSIDE").unwrap().modes, vec!["default".to_string()]);
    }

    #[test]
    fn push_action_records_mode_action() {
        let g = parse_grammar(
            "ENTER = \"enter\" -> push(tag); T = \"t\"; main = ENTER T;",
        )
        .expect("ok");
        let enter = g.tokens.get("ENTER").unwrap();
        assert_eq!(enter.mode_actions, vec![ModeAction::Push("tag".into())]);
        assert!(!enter.skip);
    }

    #[test]
    fn pop_action_records_mode_action() {
        let g = parse_grammar(
            "@mode(tag) EXIT = \"exit\" -> pop; T = \"t\"; main = T;",
        )
        .expect("ok");
        let exit = g.tokens.get("EXIT").unwrap();
        assert_eq!(exit.mode_actions, vec![ModeAction::Pop]);
    }

    #[test]
    fn pop_then_push_is_a_swap_top() {
        // `-> pop, push(b)` replaces the top of the stack — both actions
        // are kept in source order so codegen can emit them as written.
        let g = parse_grammar(
            "@mode(a) SWAP = \"s\" -> pop, push(b); T = \"t\"; main = T;",
        )
        .expect("ok");
        let swap = g.tokens.get("SWAP").unwrap();
        assert_eq!(
            swap.mode_actions,
            vec![ModeAction::Pop, ModeAction::Push("b".into())]
        );
    }

    #[test]
    fn multiple_pushes_are_kept_in_order() {
        let g = parse_grammar(
            "DEEP = \"d\" -> push(a), push(b); T = \"t\"; main = T;",
        )
        .expect("ok");
        let deep = g.tokens.get("DEEP").unwrap();
        assert_eq!(
            deep.mode_actions,
            vec![ModeAction::Push("a".into()), ModeAction::Push("b".into())]
        );
    }

    #[test]
    fn skip_with_push_is_rejected() {
        let errs = parse_grammar(
            "BUTTS = \"butts\" -> skip, push(foo); T = \"t\"; main = T;",
        )
        .err()
        .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("`skip` cannot be combined")));
    }

    #[test]
    fn push_without_arg_is_rejected() {
        let errs = parse_grammar("ENTER = \"e\" -> push; T = \"t\"; main = T;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("requires a mode name argument")));
    }

    #[test]
    fn pop_with_arg_is_rejected() {
        let errs = parse_grammar("EXIT = \"e\" -> pop(foo); T = \"t\"; main = T;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("`pop` action takes no argument")));
    }

    #[test]
    fn unknown_pre_annotation_is_rejected() {
        let errs = parse_grammar("@bogus(x) T = \"t\"; main = T;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("unknown pre-annotation `@bogus`")));
    }

    #[test]
    fn mode_pre_on_rule_is_rejected() {
        let errs = parse_grammar("T = \"t\"; @mode(tag) main = T;")
            .err()
            .expect("err");
        assert!(errs
            .iter()
            .any(|e| e.message.contains("`@mode(...)` only applies to tokens")));
    }

    // ----- ParserConfig (rust runtime) --------------------------------

    #[test]
    fn parser_config_emit_skips_yields_ws_tokens() {
        use crate::grammar::generated;
        use parsuna_rt::EmitSkips;

        // The bootstrap grammar marks WS / COMMENT as skip tokens; the
        // default config surfaces them as `Event::Token` events.
        let p = generated::parse_file_from_str_with::<EmitSkips>("  // hi\n");
        let saw_ws = p.into_iter().any(|e| match e {
            generated::Event::Token(t) => matches!(
                t.kind,
                Some(generated::TokenKind::Ws) | Some(generated::TokenKind::Comment),
            ),
            _ => false,
        });
        assert!(saw_ws, "EmitSkips should yield skip tokens in the stream");
    }

    #[test]
    fn parser_config_drop_skips_silences_ws_tokens() {
        use crate::grammar::generated;
        use parsuna_rt::DropSkips;

        let p = generated::parse_file_from_str_with::<DropSkips>("  // hi\n");
        let saw_ws = p.into_iter().any(|e| match e {
            generated::Event::Token(t) => matches!(
                t.kind,
                Some(generated::TokenKind::Ws) | Some(generated::TokenKind::Comment),
            ),
            _ => false,
        });
        assert!(!saw_ws, "DropSkips should silently consume skip tokens");
    }

    #[test]
    fn mode_pre_on_fragment_is_metadata_only() {
        // A fragment with `@mode(foo)` carries the tag for completeness,
        // but since fragments are inlined at lex time the field is
        // effectively metadata.
        let g = parse_grammar(
            "@mode(tag) _NSTART = 'A'..'Z'; NAME = _NSTART; main = NAME;",
        )
        .expect("ok");
        let frag = g.tokens.get("_NSTART").expect("_NSTART");
        assert!(frag.is_fragment);
        assert_eq!(frag.modes, vec!["tag".to_string()]);
    }
}
