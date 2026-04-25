//! Parsuna: parser generator for recoverable, pull-based parsers.
//!
//! A grammar is parsed by [`grammar::parse_grammar`] into a [`Grammar`] IR,
//! [`analysis::analyze`] checks it and computes FIRST/FOLLOW up to the
//! smallest `k` that removes LL conflicts, [`lowering::lower`] turns the
//! result into a flat [`lowering::StateTable`], and a [`codegen::Backend`]
//! emits target-language source from that table.
//!
//! Generated parsers at run time use the [`parsuna_rt`] crate (re-exported
//! here for [`Error`], [`Span`]) for the generic pull loop; each target
//! language has its own embedded runtime when there is no host crate.

pub mod analysis;
pub mod codegen;
pub mod diagnostic;
pub mod grammar;
pub mod lowering;
pub mod tree_sitter;

pub use analysis::{AnalysisOutcome, AnalyzedGrammar};
pub use codegen::EmittedFile;
pub use diagnostic::{Diagnostic, Severity};
pub use grammar::ir::{Expr, Grammar, RuleDef, TokenDef, TokenPattern};
pub use parsuna_rt::{Error, Span};
