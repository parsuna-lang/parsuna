/// A source position: a byte offset plus the 1-based line/column it falls on.
///
/// `offset` is measured in bytes into the input (not chars or grapheme
/// clusters). `line` and `column` are 1-based; a fresh position before any
/// input has been consumed is `line = 1, column = 1`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Pos {
    /// 0-based byte offset into the source.
    pub offset: u32,
    /// 1-based line number (each `\n` increments this).
    pub line: u32,
    /// 1-based column, counted in Unicode codepoints within the current line.
    pub column: u32,
}

impl Pos {
    /// Construct a [`Pos`] at `offset` bytes on `line`/`column`.
    pub fn new(offset: u32, line: u32, column: u32) -> Self {
        Self {
            offset,
            line,
            column,
        }
    }
}

/// A half-open span `[start, end)` over the source text.
///
/// `start` is inclusive, `end` is exclusive. A zero-width span (`start ==
/// end`) marks a single point in the source — used, for example, for errors
/// raised at the current lookahead before any token has been consumed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Span {
    /// Inclusive start position.
    pub start: Pos,
    /// Exclusive end position; equal to `start` for a zero-width point.
    pub end: Pos,
}

impl Span {
    /// Build a span from explicit `start` and `end` positions.
    pub fn new(start: Pos, end: Pos) -> Self {
        Self { start, end }
    }
    /// Build a zero-width span at a single point.
    pub fn point(p: Pos) -> Self {
        Self { start: p, end: p }
    }
    /// Join two spans into the span that covers both, taking the start of `a`
    /// and the end of `b`. Only meaningful when `a` precedes `b`.
    pub fn join(a: Span, b: Span) -> Self {
        Self {
            start: a.start,
            end: b.end,
        }
    }
}
