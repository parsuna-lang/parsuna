use std::borrow::Cow;

use crate::span::{Pos, Span};

/// The token-kind enumeration produced by the code generator.
///
/// Generated parsers declare a `#[repr(u16)]` enum whose variants correspond
/// to the grammar's named tokens; it implements this trait so the runtime can
/// talk about kinds uniformly. `EOF` is the sentinel returned once input is
/// exhausted. Lex failures are represented as `Option<TK>` (`None`) at the
/// lexer/parser boundary — there is no in-band "error" variant.
///
/// `Copy + Eq + 'static` keep token kinds cheap to move through the parser
/// and comparable in `FIRST`/`SYNC` sets.
pub trait TokenKindEnum: Copy + Eq + 'static {
    /// The grammar-defined name of this kind (e.g. `"IDENT"`), suitable for
    /// diagnostics.
    fn name(self) -> &'static str;
    /// The sentinel that marks end-of-input.
    const EOF: Self;
}

/// The rule-kind enumeration produced by the code generator.
///
/// One variant per non-fragment rule in the grammar. Attached to
/// [`Event::Enter`] and [`Event::Exit`] so consumers can see which rule a
/// subtree corresponds to.
pub trait RuleKindEnum: Copy + 'static {
    /// The grammar-defined name of this rule (e.g. `"expr"`).
    fn name(self) -> &'static str;
}

/// The label-kind enumeration produced by the code generator.
///
/// One variant per distinct grammar label (`name:NAME` form). Attached
/// to [`Token::label`] (as `Option<LK>`) so consumers can dispatch on
/// position name without doing string compares: `tok.label ==
/// Some(LabelKind::Name)`.
///
/// `from_id` lets callers convert a raw `u16` discriminant back to the
/// enum (e.g. when reading labels out of a serialised event). The
/// codegen also emits an empty enum variant marker for grammars that
/// declare no labels — see the per-backend `LabelKind` codegen.
pub trait LabelKindEnum: Copy + 'static {
    /// The grammar-declared name of this label (e.g. `"name"`).
    fn name(self) -> &'static str;
}

/// Trivial `LabelKindEnum` impl for `()`, used as the default
/// type-parameter value in [`Event`] and [`Token`] so the bare type
/// can be named without a codegen-emitted `LabelKind`. Grammars
/// always go through the codegen path; `()` is for the non-generated
/// naming case (tests, helper types, etc.).
impl LabelKindEnum for () {
    fn name(self) -> &'static str {
        ""
    }
}

/// A single event in the pull-based parse stream.
///
/// A parse is a flat sequence of `Enter`/`Exit` markers (delimiting the
/// subtree of each rule) interleaved with the `Token`s that make up the
/// input and any `Error`s the parser surfaced. Walking the sequence in
/// order reconstructs the parse tree without the parser ever materialising
/// one.
///
/// `TK` / `RK` / `LK` are the codegen-emitted kind/label enums;
/// generated code names this with concrete types via a `pub type Event`
/// alias. The `u16` defaults on `TK` / `RK` keep the type nameable
/// without the generated enums for the rare unparameterised use; `LK`
/// has no default because it's bounded by [`LabelKindEnum`] (and the
/// non-generated `u16` doesn't satisfy it).
#[derive(Clone, Debug)]
pub enum Event<'a, TK = (), RK = (), LK = ()> {
    /// Opens the subtree for a rule. `pos` is the start of the first child.
    Enter {
        /// Which rule's subtree is opening.
        rule: RK,
        /// Position of the first token (or matching `Exit`) inside the rule.
        pos: Pos,
    },
    /// Closes the subtree for a rule. `pos` is the end of the last child
    /// (equal to the enter position for empty rules).
    Exit {
        /// Which rule's subtree is closing. Matches the most recent
        /// unclosed `Enter`.
        rule: RK,
        /// Position just past the last consumed byte of the rule's content.
        pos: Pos,
    },
    /// A token consumed from the input, including skip tokens interleaved
    /// with structural events. After a recoverable error, the
    /// "synced-to-expected" token (e.g. when `expect` mismatches and
    /// the sync set lands on the kind it was expecting) also comes
    /// through as `Token` because it's legitimate parse data.
    Token(Token<'a, TK, LK>),
    /// A token consumed by error-recovery. Emitted between a preceding
    /// [`Event::Error`] and the recovery's sync point: each unexpected
    /// token the runtime had to skip past appears here. Distinct from
    /// [`Event::Token`] so consumers (AST builders, syntax
    /// highlighters) can either drop it from the tree or render it
    /// as an error span without tracking recovery state externally.
    Garbage(Token<'a, TK, LK>),
    /// A recoverable parse error. The parser may continue emitting events
    /// after an error so downstream tools still see a usable event stream.
    /// Followed by zero or more `Garbage` events (the unexpected
    /// tokens that recovery skipped) and then either a normal `Token`
    /// (the synced-to-expected kind) or the next structural event.
    Error(Error),
}

/// A lexed token: kind, source span, and the matched text.
///
/// `kind` is `None` only on a lex pattern miss. The scanner advances by
/// one codepoint and emits the byte(s) as a `kind: None` token; the
/// parser runtime turns that into a paired
/// [`Event::Error`]+[`Event::Garbage`] sequence at pump time and never
/// lets it reach dispatch. EOF is its own variant (`Some(TK::EOF)`).
///
/// `text` borrows from the input when possible ([`Scanner`](crate::lexer::Scanner)),
/// and owns an independent string when that is not possible
/// ([`StreamingLexer`](crate::lexer::StreamingLexer)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token<'a, TK = (), LK = ()> {
    /// Classification given to this token by the lexer's DFA, or `None` if
    /// no token pattern matched.
    pub kind: Option<TK>,
    /// Source span covered by the matched text.
    pub span: Span,
    /// The matched source text — borrowed from the input where possible
    /// and owned when the lexer cannot keep a stable reference.
    pub text: Cow<'a, str>,
    /// Grammar-position label from a `name:NAME` form, or `None` for
    /// unlabeled positions. Set by the dispatch's labeled `expect`
    /// path; left as `None` for skip tokens, garbage, and the
    /// synced-to-expected token after a recovery.
    ///
    /// Consumers compare against the codegen's `LabelKind` enum
    /// directly: `tok.label == Some(LabelKind::Name)`. No string
    /// handling, no integer conversion.
    pub label: Option<LK>,
}

/// Reserved kind id for end-of-input. Never collides with a grammar token
/// because the code generator numbers real tokens from `1`.
pub const TOKEN_EOF: u16 = 0;

/// A recoverable parse or lex error.
///
/// The parser emits these as [`Event::Error`] rather than returning them —
/// recovery happens via `recover_to` so a stream with errors still carries
/// enough structure to be useful to editors, linters, and formatters.
#[derive(Clone, Debug)]
pub struct Error {
    /// Human-readable description, suitable to display to an end-user.
    pub message: Cow<'static, str>,
    /// Span the error attaches to (typically the offending lookahead).
    pub span: Span,
}

impl Error {
    /// Build an error with the given message and a default (zero) span.
    /// Call [`Error::at`] to attach a real location.
    pub fn new(message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            message: message.into(),
            span: Span::default(),
        }
    }
    /// Builder: attach a span to this error.
    pub fn at(mut self, span: Span) -> Self {
        self.span = span;
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (s, e) = (self.span.start, self.span.end);
        if s == e {
            write!(f, "error[{}:{}]: {}", s.line, s.column, self.message)
        } else {
            write!(
                f,
                "error[{}:{}-{}:{}]: {}",
                s.line, s.column, e.line, e.column, self.message
            )
        }
    }
}

impl std::error::Error for Error {}
