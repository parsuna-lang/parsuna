//! Re-exports the runtime [`Error`] type; all compiler-phase diagnostics
//! (parse, analysis, lowering) use the same shape as the runtime emits at
//! parse time.
pub use parsuna_rt::Error;
