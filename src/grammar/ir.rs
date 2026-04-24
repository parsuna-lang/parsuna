//! In-memory representation of a parsed grammar.
//!
//! A [`Grammar`] is a flat collection of [`TokenDef`]s and [`RuleDef`]s.
//! Tokens are matched by a regular pattern ([`TokenPattern`]) compiled to a
//! DFA at lowering time; rules are LL expressions ([`Expr`]) over tokens and
//! other rules. Names starting with `_` are fragments — reusable building
//! blocks that never appear in the emitted output.

/// A parsed grammar: its name (used to pick file names in code generation)
/// plus every declared token and rule in source order.
#[derive(Clone, Debug, Default)]
pub struct Grammar {
    /// Identifier used when the code generator needs to name the output
    /// (file stem, package/namespace, etc.). Usually the grammar file's
    /// stem unless the caller overrode it.
    pub name: String,
    /// Every `token` declaration, including fragments, in source order.
    pub tokens: Vec<TokenDef>,
    /// Every `rule` declaration, including fragments, in source order.
    pub rules: Vec<RuleDef>,
}

impl Grammar {
    /// Look up a token by name. Returns `None` if no such token was declared.
    pub fn token(&self, name: &str) -> Option<&TokenDef> {
        self.tokens.iter().find(|t| t.name == name)
    }

    /// Look up a rule by name.
    pub fn rule(&self, name: &str) -> Option<&RuleDef> {
        self.rules.iter().find(|r| r.name == name)
    }

    /// Index of a rule in declaration order, if any. Useful when you need
    /// to refer to a rule by a stable numeric id.
    pub fn rule_index(&self, name: &str) -> Option<usize> {
        self.rules.iter().position(|r| r.name == name)
    }

    /// Index of a token in declaration order, if any.
    pub fn token_index(&self, name: &str) -> Option<usize> {
        self.tokens.iter().position(|t| t.name == name)
    }

    /// The default start rule (the first rule declared). Returns `None` if
    /// the grammar has no rules.
    pub fn default_start(&self) -> Option<&RuleDef> {
        self.rules.first()
    }
}

/// A single token declaration.
///
/// `skip` comes from a `?`-prefix in the grammar and causes the runtime to
/// drop the token from the structural event stream (while still surfacing
/// it alongside neighbouring events). `is_fragment` comes from a `_`-prefix
/// and means "usable in other token patterns but not itself a token kind".
#[derive(Clone, Debug)]
pub struct TokenDef {
    /// Grammar-declared token name (e.g. `IDENT`).
    pub name: String,
    /// Regular-expression-style body that matches this token.
    pub pattern: TokenPattern,
    /// Marked `?TOKEN`: matched but dropped from the structural event
    /// stream (whitespace, comments, etc.).
    pub skip: bool,
    /// Marked `_TOKEN`: usable inside other token patterns but not itself
    /// a real token kind at run time.
    pub is_fragment: bool,
    /// Source span of the whole declaration, for diagnostics.
    pub span: crate::span::Span,
}

/// A regular expression over characters that defines how a token is lexed.
///
/// Compiled to an NFA and then DFA by `lowering::lexer_dfa`. `Ref` targets
/// another token by name — typically a fragment — and is resolved into its
/// body before the DFA is built.
#[derive(Clone, Debug)]
pub enum TokenPattern {
    /// Matches the empty string (ε) — consumes nothing.
    Empty,
    /// Matches the exact string byte-for-byte.
    Literal(String),
    /// Matches any codepoint in a character class.
    Class(CharClass),
    /// Reference to another token pattern by name. Resolved (inlined)
    /// during lowering, so this never reaches the DFA builder.
    Ref(String),
    /// Concatenation: match the children in order.
    Seq(Vec<TokenPattern>),
    /// Alternation: match any one child.
    Alt(Vec<TokenPattern>),
    /// `?` — match the child zero or one times.
    Opt(Box<TokenPattern>),
    /// `*` — match the child zero or more times.
    Star(Box<TokenPattern>),
    /// `+` — match the child one or more times.
    Plus(Box<TokenPattern>),
}

impl TokenPattern {
    /// True if this pattern is a plain string literal.
    pub fn is_literal(&self) -> bool {
        matches!(self, TokenPattern::Literal(_))
    }
    /// Build a sequence, short-circuiting the trivial cases so we never
    /// produce a 0- or 1-element `Seq` (makes later walks simpler).
    pub fn seq(xs: Vec<TokenPattern>) -> TokenPattern {
        match xs.len() {
            0 => TokenPattern::Empty,
            1 => xs.into_iter().next().unwrap(),
            _ => TokenPattern::Seq(xs),
        }
    }
    /// Build an alternation, short-circuiting the trivial cases.
    pub fn alt(xs: Vec<TokenPattern>) -> TokenPattern {
        match xs.len() {
            0 => TokenPattern::Empty,
            1 => xs.into_iter().next().unwrap(),
            _ => TokenPattern::Alt(xs),
        }
    }
}

/// A character class: either the listed characters, or (when `negated`) the
/// complement of that set over the byte domain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CharClass {
    /// When true, the class matches any codepoint *not* listed in `items`
    /// (the complement of the set).
    pub negated: bool,
    /// Codepoints and ranges that make up the class.
    pub items: Vec<ClassItem>,
}

/// One element of a character class: a single codepoint or an inclusive range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassItem {
    /// A single codepoint (stored as its numeric value).
    Char(u32),
    /// An inclusive range `[lo, hi]` of codepoints.
    Range(u32, u32),
}

impl CharClass {
    /// Does codepoint `cp` belong to this class? Honours `negated`.
    pub fn contains(&self, cp: u32) -> bool {
        let hit = self.items.iter().any(|it| match it {
            ClassItem::Char(c) => *c == cp,
            ClassItem::Range(lo, hi) => *lo <= cp && cp <= *hi,
        });
        if self.negated {
            !hit
        } else {
            hit
        }
    }
}

/// A single rule declaration.
///
/// Fragment rules (`_`-prefix) are inlined by callers and do not produce
/// their own `Enter`/`Exit` events; non-fragment rules become public API
/// surface in the generated parser.
#[derive(Clone, Debug)]
pub struct RuleDef {
    /// Grammar-declared rule name (e.g. `expr`).
    pub name: String,
    /// The expression that makes up this rule.
    pub body: Expr,
    /// Marked `_rule`: inlined wherever referenced, produces no
    /// `Enter`/`Exit` events, and is not part of the public API.
    pub is_fragment: bool,
    /// Source span of the whole declaration, for diagnostics.
    pub span: crate::span::Span,
}

/// An LL expression — the body of a rule.
///
/// Left-recursion is forbidden (validated during analysis); unbounded
/// repetition uses `Star`/`Plus`/`Opt` instead.
#[derive(Clone, Debug)]
pub enum Expr {
    /// Matches nothing (ε).
    Empty,
    /// Matches a single token of the named kind.
    Token(String),
    /// Recursively parses the named rule.
    Rule(String),
    /// Concatenation: every child in order.
    Seq(Vec<Expr>),
    /// Alternation: exactly one child. Must be LL(k)-disambiguable.
    Alt(Vec<Expr>),
    /// `?` — the child appears zero or one times.
    Opt(Box<Expr>),
    /// `*` — the child appears zero or more times.
    Star(Box<Expr>),
    /// `+` — the child appears one or more times.
    Plus(Box<Expr>),
}

impl Expr {
    /// Build a sequence, collapsing trivial 0/1-element cases.
    pub fn seq(items: Vec<Expr>) -> Expr {
        match items.len() {
            0 => Expr::Empty,
            1 => items.into_iter().next().unwrap(),
            _ => Expr::Seq(items),
        }
    }
    /// Build an alternation, collapsing trivial 0/1-element cases.
    pub fn alt(items: Vec<Expr>) -> Expr {
        match items.len() {
            0 => Expr::Empty,
            1 => items.into_iter().next().unwrap(),
            _ => Expr::Alt(items),
        }
    }
}
