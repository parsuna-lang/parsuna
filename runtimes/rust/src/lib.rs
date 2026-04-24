//! Runtime library shared by every parser produced by `parsuna`.
//!
//! The crate provides the generic pieces that a generated parser plugs into:
//! a DFA-driven [`Scanner`] (and its streaming sibling [`StreamingLexer`]),
//! an event-based pull parser [`Parser`] that drives a grammar through a
//! state machine, and the glue types ([`Event`], [`Token`], [`Error`],
//! [`Span`], [`Pos`]) that every backend produces or consumes.
//!
//! Generated code supplies the grammar-specific parts (token kinds, rule
//! kinds, the compiled DFA, and the state-dispatch function) by
//! implementing [`TokenKindEnum`], [`RuleKindEnum`], [`lexer::DfaMatcher`],
//! and [`Drive`]. The runtime itself is agnostic of any particular grammar.

pub mod events;
pub mod lexer;
pub mod parser;
pub mod span;

pub use events::{Error, Event, RuleKindEnum, Token, TokenKindEnum, TOKEN_EOF, TOKEN_ERROR};
pub use lexer::{
    slurp_reader, utf8_char_len, DfaMatch, DfaMatcher, LexerBackend, Scanner, StreamingLexer,
};
pub use parser::{Drive, Parser, TERMINATED};
pub use span::{Pos, Span};
