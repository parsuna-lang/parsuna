//! Compile-time diagnostics for `.parsuna` grammars.
//!
//! [`Diagnostic`] is the single shape emitted by every compiler phase —
//! parse, validate, lints, semantic analysis. It carries a [`Severity`] so
//! the CLI can surface warnings without failing the build, and so callers
//! can promote warnings to errors with `--warnings=errors`.
//!
//! Runtime parse errors (the `parsuna_rt::Error` value emitted by generated
//! parsers) are converted into [`Diagnostic::Error`] at the boundary.

use std::borrow::Cow;

use crate::Span;

/// How serious a [`Diagnostic`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Stops compilation.
    Error,
    /// Surfaces an issue but the grammar still compiles. Promoted to
    /// [`Severity::Error`] under `--warnings=errors`.
    Warning,
}

/// A single compile-time diagnostic: severity, message, source span.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Severity of this diagnostic.
    pub severity: Severity,
    /// Human-readable description.
    pub message: Cow<'static, str>,
    /// Source span the diagnostic attaches to.
    pub span: Span,
}

impl Diagnostic {
    /// Build an error-severity diagnostic with no span. Call [`Diagnostic::at`]
    /// to attach a location.
    pub fn error(msg: impl Into<Cow<'static, str>>) -> Self {
        Self {
            severity: Severity::Error,
            message: msg.into(),
            span: Span::default(),
        }
    }

    /// Build a warning-severity diagnostic with no span.
    pub fn warning(msg: impl Into<Cow<'static, str>>) -> Self {
        Self {
            severity: Severity::Warning,
            message: msg.into(),
            span: Span::default(),
        }
    }

    /// Builder: attach a span.
    pub fn at(mut self, span: Span) -> Self {
        self.span = span;
        self
    }

    /// True iff this diagnostic should stop compilation.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl From<parsuna_rt::Error> for Diagnostic {
    fn from(e: parsuna_rt::Error) -> Self {
        Self {
            severity: Severity::Error,
            message: e.message,
            span: e.span,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let (s, e) = (self.span.start, self.span.end);
        if s == e {
            write!(f, "{}[{}:{}]: {}", label, s.line, s.column, self.message)
        } else {
            write!(
                f,
                "{}[{}:{}-{}:{}]: {}",
                label, s.line, s.column, e.line, e.column, self.message
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsuna_rt::Pos;

    fn point(line: u32, col: u32) -> Span {
        Span::point(Pos::new(0, line, col))
    }

    fn range(l1: u32, c1: u32, l2: u32, c2: u32) -> Span {
        Span::new(Pos::new(0, l1, c1), Pos::new(0, l2, c2))
    }

    #[test]
    fn error_constructor_sets_severity_and_default_span() {
        let d = Diagnostic::error("oops");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "oops");
        assert_eq!(d.span, Span::default());
        assert!(d.is_error());
    }

    #[test]
    fn warning_constructor_sets_severity() {
        let d = Diagnostic::warning("careful");
        assert_eq!(d.severity, Severity::Warning);
        assert!(!d.is_error());
    }

    #[test]
    fn at_attaches_span() {
        let s = point(3, 7);
        let d = Diagnostic::error("x").at(s);
        assert_eq!(d.span, s);
    }

    #[test]
    fn from_runtime_error_is_error_severity() {
        let e = parsuna_rt::Error::new("boom").at(point(2, 4));
        let d: Diagnostic = e.into();
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "boom");
        assert_eq!(d.span, point(2, 4));
    }

    #[test]
    fn display_error_point_span() {
        let d = Diagnostic::error("nope").at(point(5, 9));
        assert_eq!(format!("{}", d), "error[5:9]: nope");
    }

    #[test]
    fn display_warning_point_span() {
        let d = Diagnostic::warning("hmm").at(point(1, 1));
        assert_eq!(format!("{}", d), "warning[1:1]: hmm");
    }

    #[test]
    fn display_range_span() {
        let d = Diagnostic::error("range").at(range(1, 1, 1, 12));
        assert_eq!(format!("{}", d), "error[1:1-1:12]: range");
    }

    #[test]
    fn display_warning_range_span() {
        let d = Diagnostic::warning("multi").at(range(2, 3, 4, 5));
        assert_eq!(format!("{}", d), "warning[2:3-4:5]: multi");
    }

    #[test]
    fn message_accepts_owned_string() {
        let d = Diagnostic::error(format!("rule `{}` is broken", "main"));
        assert_eq!(d.message, "rule `main` is broken");
    }
}
