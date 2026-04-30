use std::borrow::Cow;
use std::io::Read;

use crate::events::{Token, TokenKindEnum};
use crate::span::{Pos, Span};

/// Number of bytes in the UTF-8 sequence that starts with `b`.
///
/// For continuation bytes (`10xxxxxx`) returns `1` — the caller is expected
/// to be positioned at the start of a codepoint or a garbage byte, and
/// advancing one byte is the correct recovery step either way.
pub fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// Result of running the compiled DFA over a byte slice.
///
/// `best_len`/`best_kind` are the longest match found. `best_kind` is
/// `Some(TK)` whenever `best_len > 0`; on a dead scan that never reached
/// an accept state, `best_len` is `0` and `best_kind` is `None`.
/// `scanned` is how many bytes the scan actually walked past `start`
/// before it died — it may exceed `best_len` when the DFA accepted
/// something early and then kept advancing through non-accept states
/// before running dead. The streaming lexer uses `scanned == (buf.len()
/// - start)` to detect that the match ran out of buffer rather than
/// hitting a real dead transition.
pub struct DfaMatch<TK: TokenKindEnum> {
    /// Bytes consumed by the longest match. `0` means no accept.
    pub best_len: usize,
    /// Token kind of the longest match, or `None` when the scan never
    /// reached an accept state.
    pub best_kind: Option<TK>,
    /// Total bytes the scan walked. `>= best_len`; equals `buf.len() - start`
    /// if the scan stopped at the end of input rather than at a dead state.
    pub scanned: usize,
}

/// Grammar-specific lexer DFA. Generated code implements this with one
/// compiled state machine per declared lexer mode — a single
/// `longest_match` branches on the mode id and falls into the matcher
/// for the active mode. Grammars without `@mode(...)` pre-annotations
/// have one mode (id 0) and the dispatch collapses to a single arm.
pub trait DfaMatcher<TK: TokenKindEnum> {
    /// Scan `buf[start..]` for the longest matching token in `mode`.
    /// `mode` must be a mode id the grammar declared (0 = default).
    fn longest_match(buf: &[u8], start: usize, mode: u32) -> DfaMatch<TK>;
}

/// Abstraction over anything that can hand tokens to the parser one at a
/// time. Implemented by the two lexers in this crate; generated code is
/// generic over any type that implements it, so callers can plug in a
/// custom lexer if the DFA-based one is insufficient.
pub trait LexerBackend<'a, TK: TokenKindEnum> {
    /// Produce the next token. Must return [`Token`] with kind `TK::EOF`
    /// once the input is exhausted (and every subsequent call). The
    /// matcher used is determined by the mode currently on top of the
    /// lexer's mode stack.
    fn next_token(&mut self) -> Token<'a, TK>;
    /// Push `mode` onto the mode stack. Subsequent [`Self::next_token`]
    /// calls scan with that mode's DFA until a matching [`Self::pop_mode`].
    /// Default mode (id 0) is what the lexer starts in; calling
    /// [`Self::pop_mode`] when only the default is on the stack is a
    /// no-op so a stray `pop` action can't underflow.
    fn push_mode(&mut self, mode: u32);
    /// Pop the topmost mode off the stack, leaving at least the default
    /// mode in place. Underflow is silently ignored.
    fn pop_mode(&mut self);
}

/// In-memory, zero-copy lexer that runs the generated DFA over a `&str`.
///
/// Tokens borrow their `text` straight from the source string, so no
/// allocations happen per-token. Use this when you already have the whole
/// input in memory. `D` is the grammar-specific compiled matcher that
/// generated code supplies.
pub struct Scanner<'a, TK: TokenKindEnum, D: DfaMatcher<TK>> {
    src: &'a str,
    buf: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
    /// Mode stack — top of stack is the active mode. Always non-empty;
    /// initialised with the default mode (id 0). Push/pop happens via
    /// [`LexerBackend::push_mode`] / [`LexerBackend::pop_mode`], typically
    /// driven by `-> push(name)` / `-> pop` token actions.
    modes: Vec<u32>,
    _tk: std::marker::PhantomData<fn() -> TK>,
    _dfa: std::marker::PhantomData<fn() -> D>,
}

impl<'a, TK: TokenKindEnum, D: DfaMatcher<TK>> Scanner<'a, TK, D> {
    /// Build a scanner over `src`. The scanner starts at the beginning of
    /// the string at line 1, column 1, with the default lexer mode (id 0)
    /// on the mode stack.
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            buf: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            modes: vec![0],
            _tk: std::marker::PhantomData,
            _dfa: std::marker::PhantomData,
        }
    }

    #[inline]
    fn current_mode(&self) -> u32 {
        // `modes` is initialised non-empty and `pop_mode` refuses to
        // underflow, so this is always safe.
        *self.modes.last().expect("mode stack underflow")
    }

    #[inline]
    fn cur_pos(&self) -> Pos {
        Pos::new(self.pos as u32, self.line, self.col)
    }

    /// Advance `n` bytes, updating line/column counters.
    ///
    /// Fast path: if the slice contains only plain ASCII non-newline bytes,
    /// `col` can be bumped by `n` directly. Otherwise we walk byte-by-byte
    /// so that multi-byte UTF-8 sequences count as one column and newlines
    /// reset the column.
    #[inline]
    fn advance(&mut self, n: usize) {
        let slice = &self.buf[self.pos..self.pos + n];
        let needs_walk = slice.iter().any(|&b| b == b'\n' || b >= 0x80);
        if !needs_walk {
            self.col += n as u32;
            self.pos += n;
            return;
        }

        let end = self.pos + n;
        while self.pos < end {
            let b = self.buf[self.pos];
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                self.col = 1;
            } else if (b & 0xC0) != 0x80 {
                self.col += 1;
            }
        }
    }
}

impl<'a, TK: TokenKindEnum, D: DfaMatcher<TK>> LexerBackend<'a, TK> for Scanner<'a, TK, D> {
    /// Produce the next token, advancing the scanner.
    ///
    /// Behaviour at the boundary: if no token pattern matches at the current
    /// position, the scanner emits a single-codepoint token with `kind: None`
    /// so the parser can surface an error and recover. On exhaustion it
    /// emits repeated `Some(TK::EOF)` tokens.
    #[inline]
    fn next_token(&mut self) -> Token<'a, TK> {
        if self.pos >= self.buf.len() {
            let pos = self.cur_pos();
            return Token {
                kind: Some(TK::EOF),
                span: Span::point(pos),
                text: Cow::Borrowed(""),
            };
        }
        let m = D::longest_match(self.buf, self.pos, self.current_mode());
        let start = self.cur_pos();
        if m.best_len > 0 {
            let text: Cow<'a, str> = Cow::Borrowed(&self.src[self.pos..self.pos + m.best_len]);
            self.advance(m.best_len);
            Token {
                kind: m.best_kind,
                span: Span::new(start, self.cur_pos()),
                text,
            }
        } else {
            let ch_len = utf8_char_len(self.buf[self.pos]).min(self.buf.len() - self.pos);
            let text: Cow<'a, str> = Cow::Borrowed(&self.src[self.pos..self.pos + ch_len]);
            self.advance(ch_len);
            Token {
                kind: None,
                span: Span::new(start, self.cur_pos()),
                text,
            }
        }
    }

    #[inline]
    fn push_mode(&mut self, mode: u32) {
        self.modes.push(mode);
    }

    #[inline]
    fn pop_mode(&mut self) {
        if self.modes.len() > 1 {
            self.modes.pop();
        }
    }
}

const STREAM_CHUNK: usize = 16 * 1024;

/// Streaming lexer that reads from any `Read` source in 16 KiB chunks.
///
/// Unlike [`Scanner`], tokens own their text (`Cow::Owned`) because the
/// underlying buffer gets drained as the lexer makes progress. Use this
/// when you want to parse without loading the entire input into memory.
///
/// Internally the buffer is compacted once consumed bytes exceed 64 KiB,
/// which bounds memory use to roughly that plus one chunk.
pub struct StreamingLexer<R: Read, TK: TokenKindEnum, D: DfaMatcher<TK>> {
    reader: R,
    buf: Vec<u8>,
    buf_pos: usize,
    eof: bool,
    offset: u32,
    line: u32,
    col: u32,
    /// Mode stack — see [`Scanner::modes`]. Always non-empty.
    modes: Vec<u32>,
    _tk: std::marker::PhantomData<fn() -> TK>,
    _dfa: std::marker::PhantomData<fn() -> D>,
}

impl<R: Read, TK: TokenKindEnum, D: DfaMatcher<TK>> StreamingLexer<R, TK, D> {
    /// Build a streaming lexer that pulls from `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(STREAM_CHUNK),
            buf_pos: 0,
            eof: false,
            offset: 0,
            line: 1,
            col: 1,
            modes: vec![0],
            _tk: std::marker::PhantomData,
            _dfa: std::marker::PhantomData,
        }
    }

    fn current_mode(&self) -> u32 {
        *self.modes.last().expect("mode stack underflow")
    }

    fn longest_match(&mut self) -> (usize, Option<TK>) {
        loop {
            let view_slice = &self.buf[self.buf_pos..];
            let view_len = view_slice.len();
            let mode = self.current_mode();
            let m = D::longest_match(view_slice, 0, mode);
            if m.scanned == view_len && !self.eof && self.read_more() {
                continue;
            }
            return (m.best_len, m.best_kind);
        }
    }

    fn pos(&self) -> Pos {
        Pos::new(self.offset, self.line, self.col)
    }

    fn view(&self) -> &[u8] {
        &self.buf[self.buf_pos..]
    }

    fn consume(&mut self, len: usize) {
        let end = self.buf_pos + len;
        while self.buf_pos < end {
            let b = self.buf[self.buf_pos];
            self.buf_pos += 1;
            self.offset += 1;
            if b == b'\n' {
                self.line += 1;
                self.col = 1;
            } else if (b & 0xC0) != 0x80 {
                self.col += 1;
            }
        }

        if self.buf_pos > 64 * 1024 {
            self.buf.drain(..self.buf_pos);
            self.buf_pos = 0;
        }
    }

    fn ensure_bytes(&mut self, want: usize) {
        while !self.eof && self.view().len() < want {
            if !self.read_more() {
                break;
            }
        }
    }

    fn read_more(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let start = self.buf.len();
        self.buf.resize(start + STREAM_CHUNK, 0);
        match self.reader.read(&mut self.buf[start..]) {
            Ok(0) => {
                self.buf.truncate(start);
                self.eof = true;
                false
            }
            Ok(n) => {
                self.buf.truncate(start + n);
                true
            }
            Err(_) => {
                self.buf.truncate(start);
                self.eof = true;
                false
            }
        }
    }
}

impl<R: Read, TK: TokenKindEnum, D: DfaMatcher<TK>> LexerBackend<'static, TK>
    for StreamingLexer<R, TK, D>
{
    /// Produce the next token from the stream. See [`Scanner::next_token`]
    /// for the overall contract; the only difference is that `text` is
    /// owned because the buffer is not stable across calls.
    ///
    /// Token-spanning-buffer-boundary handling: we pre-read a chunk before
    /// matching, then re-drive the compiled matcher with more buffer if the
    /// match saturated the view without hitting EOF. This can re-scan the
    /// same token prefix but keeps the compiled DFA a pure slice function
    /// rather than one entangled with the reader.
    fn next_token(&mut self) -> Token<'static, TK> {
        self.ensure_bytes(STREAM_CHUNK);
        if self.view().is_empty() {
            let p = self.pos();
            return Token {
                kind: Some(TK::EOF),
                span: Span::point(p),
                text: Cow::Borrowed(""),
            };
        }
        let (len, kind) = self.longest_match();
        let start = self.pos();
        if len > 0 {
            let text = Cow::Owned(String::from_utf8_lossy(&self.view()[..len]).into_owned());
            self.consume(len);
            Token {
                kind,
                span: Span::new(start, self.pos()),
                text,
            }
        } else {
            let ch_len = utf8_char_len(self.view()[0]).min(self.view().len());
            let text = Cow::Owned(String::from_utf8_lossy(&self.view()[..ch_len]).into_owned());
            self.consume(ch_len);
            Token {
                kind: None,
                span: Span::new(start, self.pos()),
                text,
            }
        }
    }

    #[inline]
    fn push_mode(&mut self, mode: u32) {
        self.modes.push(mode);
    }

    #[inline]
    fn pop_mode(&mut self) {
        if self.modes.len() > 1 {
            self.modes.pop();
        }
    }
}
