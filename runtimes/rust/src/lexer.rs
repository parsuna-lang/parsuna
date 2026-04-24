use std::borrow::Cow;
use std::io::Read;

use crate::events::{Token, TokenKindEnum, TOKEN_EOF, TOKEN_ERROR};
use crate::span::{Pos, Span};

/// A dense DFA table, flat-packed so state transitions are one array index.
///
/// Layout:
/// * `trans` is `states * 256` entries; `trans[state * 256 + byte]` is the
///   next state, or `0` (the dead state) if the input byte does not extend
///   any live match.
/// * `accept[state]` is the token-kind id accepted in `state`, or `0` when
///   no token ends at that state.
///
/// Built at generator time by `lexer_dfa` and embedded into generated code
/// as a `'static` table.
pub struct DfaConfig {
    /// Total number of DFA states, including the dead state at id 0.
    pub states: u32,
    /// Id of the initial state the lexer enters at each `next_token`.
    pub start: u32,
    /// Flat transition table: `trans[state * 256 + byte]` is the next
    /// state id (or `0` for "no transition" — the dead state).
    pub trans: &'static [u32],
    /// Acceptance table: `accept[state]` is the token-kind id the state
    /// accepts, or `0` when the state is non-accepting.
    pub accept: &'static [i16],
}

/// Abstraction over anything that can hand tokens to the parser one at a
/// time. Implemented by the two lexers in this crate; generated code is
/// generic over any type that implements it, so callers can plug in a
/// custom lexer if the DFA-based one is insufficient.
pub trait LexerBackend<'a, TK: TokenKindEnum> {
    /// Produce the next token. Must return [`Token`] with kind `TK::EOF`
    /// once the input is exhausted (and every subsequent call).
    fn next_token(&mut self) -> Token<'a, TK>;
}

impl<'a, TK: TokenKindEnum> LexerBackend<'a, TK> for Scanner<'a, TK> {
    #[inline(always)]
    fn next_token(&mut self) -> Token<'a, TK> {
        Scanner::next_token(self)
    }
}

impl<R: Read, TK: TokenKindEnum> LexerBackend<'static, TK> for StreamingLexer<R, TK> {
    #[inline]
    fn next_token(&mut self) -> Token<'static, TK> {
        StreamingLexer::next_token(self)
    }
}

/// In-memory, zero-copy lexer that runs the DFA over a `&str`.
///
/// Tokens borrow their `text` straight from the source string, so no
/// allocations happen per-token. Use this when you already have the whole
/// input in memory.
pub struct Scanner<'a, TK: TokenKindEnum> {
    src: &'a str,
    buf: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
    dfa: &'static DfaConfig,
    _tk: std::marker::PhantomData<fn() -> TK>,
}

impl<'a, TK: TokenKindEnum> Scanner<'a, TK> {
    /// Build a scanner that runs `dfa` over `src`. The scanner starts at the
    /// beginning of the string at line 1, column 1.
    pub fn new(src: &'a str, dfa: &'static DfaConfig) -> Self {
        Self {
            src,
            buf: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            dfa,
            _tk: std::marker::PhantomData,
        }
    }

    /// Produce the next token, advancing the scanner.
    ///
    /// Behaviour at the boundary: if no token pattern matches at the current
    /// position, the scanner emits a single-codepoint [`TOKEN_ERROR`] token
    /// so the parser can surface an error and recover. On exhaustion it
    /// emits repeated [`TOKEN_EOF`] tokens.
    #[inline]
    pub fn next_token(&mut self) -> Token<'a, TK> {
        if self.pos >= self.buf.len() {
            let pos = self.cur_pos();
            return Token {
                kind: unsafe { dfa_id_to_kind::<TK>(TOKEN_EOF) },
                span: Span::point(pos),
                text: Cow::Borrowed(""),
            };
        }
        let (len, kind_id) = self.longest_match();
        let start = self.cur_pos();
        if len == 0 {
            let ch_len = utf8_char_len(self.buf[self.pos]).min(self.buf.len() - self.pos);
            let text: Cow<'a, str> = Cow::Borrowed(&self.src[self.pos..self.pos + ch_len]);
            self.advance(ch_len);
            return Token {
                kind: unsafe { dfa_id_to_kind::<TK>(TOKEN_ERROR) },
                span: Span::new(start, self.cur_pos()),
                text,
            };
        }
        let text: Cow<'a, str> = Cow::Borrowed(&self.src[self.pos..self.pos + len]);
        self.advance(len);
        Token {
            kind: unsafe { dfa_id_to_kind::<TK>(kind_id) },
            span: Span::new(start, self.cur_pos()),
            text,
        }
    }

    /// Run the DFA greedily from `self.pos`, returning the longest match
    /// (length in bytes, accepted kind id). Follows the standard lexer rule:
    /// the longest match wins, and ties are resolved by the smallest (= earliest-declared)
    /// token id.
    #[inline]
    fn longest_match(&self) -> (usize, i16) {
        let mut state = self.dfa.start;
        let mut pos = self.pos;
        let mut best_len = 0usize;
        let mut best_kind = TOKEN_ERROR;
        while pos < self.buf.len() {
            let b = self.buf[pos];
            let next = self.dfa.trans[state as usize * 256 + b as usize];
            if next == 0 {
                break;
            }
            pos += 1;
            state = next;
            let acc = self.dfa.accept[state as usize];
            if acc != 0 {
                best_len = pos - self.pos;
                best_kind = acc;
            }
        }
        (best_len, best_kind)
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

/// Read a `Read` into a `String` in one shot. Convenience wrapper for
/// callers that want to hand a file or stream to [`Scanner`] after slurping
/// it to memory.
pub fn slurp_reader<R: Read>(mut reader: R) -> std::io::Result<String> {
    let mut s = String::new();
    reader.read_to_string(&mut s)?;
    Ok(s)
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
pub struct StreamingLexer<R: Read, TK: TokenKindEnum> {
    reader: R,
    buf: Vec<u8>,
    buf_pos: usize,
    eof: bool,
    offset: u32,
    line: u32,
    col: u32,
    dfa: &'static DfaConfig,
    _tk: std::marker::PhantomData<fn() -> TK>,
}

impl<R: Read, TK: TokenKindEnum> StreamingLexer<R, TK> {
    /// Build a streaming lexer that pulls from `reader` and runs `dfa`.
    pub fn new(reader: R, dfa: &'static DfaConfig) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(STREAM_CHUNK),
            buf_pos: 0,
            eof: false,
            offset: 0,
            line: 1,
            col: 1,
            dfa,
            _tk: std::marker::PhantomData,
        }
    }

    /// Produce the next token from the stream. See [`Scanner::next_token`]
    /// for the overall contract; the only difference is that `text` is
    /// owned because the buffer is not stable across calls.
    pub fn next_token(&mut self) -> Token<'static, TK> {
        self.ensure_bytes(STREAM_CHUNK);
        if self.view().is_empty() {
            let p = self.pos();
            return Token {
                kind: unsafe { dfa_id_to_kind::<TK>(TOKEN_EOF) },
                span: Span::point(p),
                text: Cow::Borrowed(""),
            };
        }
        let (len, kind_id) = self.longest_match();
        let start = self.pos();
        if len == 0 {
            let ch_len = utf8_char_len(self.view()[0]).min(self.view().len());
            let text = Cow::Owned(String::from_utf8_lossy(&self.view()[..ch_len]).into_owned());
            self.consume(ch_len);
            return Token {
                kind: unsafe { dfa_id_to_kind::<TK>(TOKEN_ERROR) },
                span: Span::new(start, self.pos()),
                text,
            };
        }

        let text = Cow::Owned(String::from_utf8_lossy(&self.view()[..len]).into_owned());
        self.consume(len);
        Token {
            kind: unsafe { dfa_id_to_kind::<TK>(kind_id) },
            span: Span::new(start, self.pos()),
            text,
        }
    }

    fn longest_match(&mut self) -> (usize, i16) {
        let mut state = self.dfa.start;
        let mut pos = 0usize;
        let mut best_len = 0usize;
        let mut best_kind = TOKEN_ERROR;
        loop {
            if pos == self.view().len() && !self.eof && state != self.dfa.start {
                if !self.read_more() {
                    break;
                }
            }
            let view = self.view();
            if pos >= view.len() {
                break;
            }
            let b = view[pos];
            let next = self.dfa.trans[state as usize * 256 + b as usize];
            if next == 0 {
                break;
            }
            pos += 1;
            state = next;
            let acc = self.dfa.accept[state as usize];
            if acc != 0 {
                best_len = pos;
                best_kind = acc;
            }
        }
        (best_len, best_kind)
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

/// Reinterpret a DFA-produced `i16` kind id as the generated kind enum.
///
/// # Safety
///
/// Sound because every generated `TokenKindEnum` is `#[repr(i16)]` and has a
/// variant for every id the DFA can produce. The [`AssertI16Sized`] check
/// below fails to compile when that invariant is violated.
#[inline(always)]
unsafe fn dfa_id_to_kind<TK: TokenKindEnum>(id: i16) -> TK {
    let _ = AssertI16Sized::<TK>::OK;
    std::mem::transmute_copy::<i16, TK>(&id)
}

struct AssertI16Sized<TK: TokenKindEnum>(std::marker::PhantomData<TK>);
impl<TK: TokenKindEnum> AssertI16Sized<TK> {
    const OK: () = assert!(
        std::mem::size_of::<TK>() == std::mem::size_of::<i16>(),
        "TokenKindEnum impl must be `#[repr(i16)]` (same size as i16)",
    );
}

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
