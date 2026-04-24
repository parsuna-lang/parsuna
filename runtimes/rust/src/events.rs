use std::borrow::Cow;

use crate::span::{Pos, Span};

/// The token-kind enumeration produced by the code generator.
///
/// Generated parsers declare a `#[repr(i16)]` enum whose variants correspond
/// to the grammar's named tokens; it implements this trait so the runtime can
/// talk about kinds uniformly. `EOF` is the sentinel returned once input is
/// exhausted.
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

/// A single event in the pull-based parse stream.
///
/// A parse is a flat sequence of `Enter`/`Exit` markers (delimiting the
/// subtree of each rule) interleaved with the `Token`s that make up the
/// input and any `Error`s the parser surfaced. Walking the sequence in
/// order reconstructs the parse tree without the parser ever materialising
/// one.
///
/// The defaults `TK = i16, RK = u16` exist so the type can name itself
/// without depending on the generated enums; generated code substitutes its
/// own kind enums.
#[derive(Clone, Debug)]
pub enum Event<'a, TK = i16, RK = u16> {
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
    /// with structural events.
    Token(Token<'a, TK>),
    /// A recoverable parse error. The parser may continue emitting events
    /// after an error so downstream tools still see a usable event stream.
    Error(Error),
}

/// A lexed token: kind, source span, and the matched text.
///
/// `text` borrows from the input when possible ([`Scanner`](crate::lexer::Scanner)),
/// and owns an independent string when that is not possible
/// ([`StreamingLexer`](crate::lexer::StreamingLexer)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token<'a, TK = i16> {
    /// Classification given to this token by the lexer's DFA.
    pub kind: TK,
    /// Source span covered by the matched text.
    pub span: Span,
    /// The matched source text — borrowed from the input where possible
    /// and owned when the lexer cannot keep a stable reference.
    pub text: Cow<'a, str>,
}

impl<'a> Token<'a, i16> {
    /// Build a token with the raw numeric kind id. Mostly used by tests and
    /// by the default-kind specialisation; generated code constructs tokens
    /// with its own enum variants.
    pub fn new(kind: i16, span: Span, text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            kind,
            span,
            text: text.into(),
        }
    }
    /// Build an EOF token at `span`. Always has empty text.
    pub fn eof(span: Span) -> Self {
        Self {
            kind: TOKEN_EOF,
            span,
            text: Cow::Borrowed(""),
        }
    }
}

/// Reserved kind id for end-of-input. Never collides with a grammar token
/// because the code generator numbers real tokens from `1`.
pub const TOKEN_EOF: i16 = 0;

/// Reserved kind id for a lexer error (a byte at the current position that
/// no token pattern matches). The lexer still advances by one codepoint so
/// parsing can recover.
pub const TOKEN_ERROR: i16 = -1;

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
