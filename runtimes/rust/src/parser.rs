use std::collections::VecDeque;
use std::marker::PhantomData;

use crate::events::{Error, Event, RuleKindEnum, Token, TokenKindEnum};
use crate::lexer::LexerBackend;
use crate::span::{Pos, Span};

/// Sentinel state for "the parser has reached the end of the program".
///
/// When [`Parser::state`] equals this value the generated driver exits its
/// dispatch loop, and [`Parser::next_event`] performs the final EOF check
/// before yielding `None`.
pub const TERMINATED: u32 = 0;

/// Bridge from a generated grammar into the runtime's pull loop.
///
/// The code generator produces a zero-sized `Grammar` type that implements
/// this trait. [`Drive::drive`] is the generated state-dispatch function;
/// [`Drive::is_skip`] checks whether a token kind is a skip (whitespace,
/// comments, etc.); `K` is the lookahead required to decide every
/// alternative in the grammar (= LL(k)).
pub trait Drive<const K: usize>: Sized {
    type TokenKind: TokenKindEnum;
    type RuleKind: RuleKindEnum;
    /// True iff the grammar declares any `[skip]`-annotated tokens. Lets
    /// the runtime skip the pending-skip bookkeeping when no skips exist.
    const HAS_SKIPS: bool;
    /// Does `kind` denote a skip token (dropped from the structural stream
    /// and re-attached around structural events)?
    fn is_skip(kind: Self::TokenKind) -> bool;
    /// Run the generated dispatch loop. Executes states until either the
    /// event queue has something to emit or the parser hits [`TERMINATED`].
    fn drive<'a, L: LexerBackend<'a, Self::TokenKind>>(p: &mut Parser<'a, L, K, Self>);
}

/// The pull-based parser over a grammar `G`.
///
/// The parser keeps a ring of `K` lookahead tokens (sized to the grammar's
/// LL(k)), a return stack, and a queue of events waiting to be emitted.
/// Each call to [`next_event`](Self::next_event) either drains a pending
/// event from the queue or asks the generated code to execute states until
/// one is produced. Skip tokens (`[skip]`-annotated in the grammar) are
/// held in a side queue and flushed into the output stream just before the next
/// structural event, so consumers see whitespace and comments in the
/// correct positions without the state machine needing to handle them.
pub struct Parser<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> {
    lex: L,
    look: [Token<'a, G::TokenKind>; K],
    prev_end: Pos,
    state: u32,
    ret_stack: Vec<u32>,
    queue: VecDeque<Event<'a, G::TokenKind, G::RuleKind>>,
    pending_skips: VecDeque<Token<'a, G::TokenKind>>,
    eof_checked: bool,
    _grammar: PhantomData<fn(&mut Parser<'a, L, K, G>)>,
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> Parser<'a, L, K, G> {
    /// Build a parser over `lex`, starting at `entry` (the state id of the
    /// rule you want to parse — generated code exposes `ENTRY_FOO` constants
    /// for each public rule and `parse_foo_from_str` wrappers that call
    /// this).
    ///
    /// The lookahead buffer is primed by pumping `K` tokens up-front so that
    /// `look(0)..look(K-1)` is valid from the very first dispatch.
    pub fn new(lex: L, entry: u32) -> Self {
        let mut parser = Parser {
            lex,
            look: std::array::from_fn(|_| Token {
                kind: Some(G::TokenKind::EOF),
                span: Span::default(),
                text: std::borrow::Cow::Borrowed(""),
            }),
            prev_end: Pos::default(),
            state: entry,
            ret_stack: Vec::with_capacity(64),
            queue: VecDeque::with_capacity(16),
            pending_skips: VecDeque::with_capacity(16),
            eof_checked: false,
            _grammar: PhantomData,
        };
        for i in 0..K {
            parser.look[i] = parser.pump_token();
        }
        parser.prev_end = parser.look[0].span.start;
        parser
    }

    /// Current state id. Generated dispatch reads this at the start of each
    /// iteration and writes it back when the loop suspends.
    #[inline]
    pub fn state(&self) -> u32 {
        self.state
    }

    /// Overwrite the current state. Primarily used by generated code to
    /// move between states inside a dispatch step.
    #[inline]
    pub fn set_state(&mut self, s: u32) {
        self.state = s;
    }

    /// Push a return address onto the call stack. Used for rule calls
    /// (the caller saves the state to resume after the callee's `Ret`) and
    /// for `*`/`+` loops (each iteration re-enters the loop state).
    #[inline]
    pub fn push_ret(&mut self, s: u32) {
        self.ret_stack.push(s);
    }

    /// Pop the top return address, or [`TERMINATED`] if the stack is empty
    /// (meaning we have finished the entry rule).
    #[inline]
    pub fn ret(&mut self) -> u32 {
        self.ret_stack.pop().unwrap_or(TERMINATED)
    }

    /// True if there is nothing queued to emit. Generated `drive` loops
    /// continue only while this holds — the moment an event lands in the
    /// queue, the loop must yield so `next_event` can return it.
    #[inline]
    pub fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Peek at the `i`-th lookahead token. `i` must be `< K`.
    #[inline]
    pub fn look(&self, i: usize) -> &Token<'a, G::TokenKind> {
        &self.look[i]
    }

    /// Test whether the current lookahead starts with any of the given
    /// sequences. Used by `*`, `+`, `?` dispatch to check whether the body
    /// of the loop/option matches at the current position.
    ///
    /// Each inner slice is a prefix to try; the function returns `true` on
    /// the first full prefix match.
    #[inline]
    pub fn matches_first(&self, set: &[&[G::TokenKind]]) -> bool {
        'seq: for seq in set {
            for (i, want) in seq.iter().enumerate() {
                if self.look[i].kind != Some(*want) {
                    continue 'seq;
                }
            }
            return true;
        }
        false
    }

    /// Emit an `Enter` event for `rule`. Records the rule's start position
    /// as `prev_end` so a later `Exit` without any intervening tokens still
    /// produces a zero-width span at the expected place.
    #[inline(always)]
    pub fn enter(&mut self, rule: G::RuleKind) {
        let pos = self.look[0].span.start;
        self.prev_end = pos;
        self.emit(Event::Enter { rule, pos });
    }

    /// Emit an `Exit` event for `rule`, positioned at the end of the last
    /// consumed token (or the rule's start for empty rules).
    #[inline(always)]
    pub fn exit(&mut self, rule: G::RuleKind) {
        self.emit(Event::Exit {
            rule,
            pos: self.prev_end,
        });
    }

    /// Append an event to the output queue.
    ///
    /// If skip tokens are pending and the event is positioned past them, we
    /// flush those skips first — this keeps the consumer-visible order
    /// faithful (comments before the structural event that follows them).
    #[inline(always)]
    pub fn emit(&mut self, ev: Event<'a, G::TokenKind, G::RuleKind>) {
        if G::HAS_SKIPS && !self.pending_skips.is_empty() {
            let start = match &ev {
                Event::Enter { pos, .. } | Event::Exit { pos, .. } => *pos,
                Event::Token(t) => t.span.start,
                Event::Error(e) => e.span.start,
            };
            self.flush_skips_before(start);
        }
        self.queue.push_back(ev);
    }

    /// Raise a recoverable error pointing at the current lookahead.
    pub fn error_here(&mut self, msg: impl Into<std::borrow::Cow<'static, str>>) {
        let span = self.look[0].span;
        self.emit(Event::Error(Error::new(msg).at(span)));
    }

    /// Try to consume a token of `kind`; on mismatch, emit an error,
    /// recover to the nearest sync token, and try once more.
    ///
    /// `sync` is typically the caller rule's FOLLOW set: the parser skips
    /// unexpected tokens until it reaches one of these, which gives the
    /// surrounding rule a reasonable place to resume. The retry after
    /// recovery handles the common case where the expected token is what
    /// we skipped *to* — the caller still wants to swallow it so the rest
    /// of the rule sees a clean input.
    pub fn expect(
        &mut self,
        kind: G::TokenKind,
        sync: &[G::TokenKind],
        expected_msg: &'static str,
    ) {
        if self.look[0].kind == Some(kind) {
            self.consume();
            return;
        }
        self.error_here(expected_msg);
        self.recover_to(sync);
        if self.look[0].kind == Some(kind) {
            self.consume();
        }
    }

    /// Consume the current lookahead token, emit it, and shift the buffer
    /// up by one. The new slot `K-1` is refilled from the lexer.
    #[inline]
    pub fn consume(&mut self) {
        self.prev_end = self.look[0].span.end;
        let next = self.pump_token();
        let t = std::mem::replace(&mut self.look[0], next);
        for i in 0..K - 1 {
            self.look.swap(i, i + 1);
        }
        self.emit(Event::Token(t));
    }

    /// Consume tokens until the current lookahead matches `sync` (or EOF).
    /// Called after an error to skip past garbage.
    pub fn recover_to(&mut self, sync: &[G::TokenKind]) {
        loop {
            match self.look[0].kind {
                Some(k) if k == G::TokenKind::EOF => return,
                Some(k) if sync.contains(&k) => return,
                _ => self.consume(),
            }
        }
    }

    /// Produce the next event from the parse, or `None` once the entire
    /// input has been consumed.
    ///
    /// On termination the parser performs one extra pass to make sure we
    /// have reached EOF (if we haven't, it emits a trailing error and
    /// consumes the remaining tokens so consumers see them as tokens rather
    /// than silent drops), and then flushes any final pending skip tokens.
    #[inline]
    pub fn next_event(&mut self) -> Option<Event<'a, G::TokenKind, G::RuleKind>> {
        loop {
            if let Some(e) = self.queue.pop_front() {
                return Some(e);
            }
            if self.state == TERMINATED {
                if !self.eof_checked {
                    self.eof_checked = true;
                    if self.look[0].kind != Some(G::TokenKind::EOF) {
                        self.error_here("expected end of input");
                        while self.look[0].kind != Some(G::TokenKind::EOF) {
                            self.consume();
                        }
                    }
                    let end = self.look[0].span.end;
                    self.flush_skips_before(end);
                    continue;
                }
                return None;
            }
            G::drive(self);
        }
    }

    /// Pull one token from the lexer, routing skip tokens into a side queue
    /// so the state machine only sees structural tokens.
    #[inline]
    fn pump_token(&mut self) -> Token<'a, G::TokenKind> {
        loop {
            let t = self.lex.next_token();
            if G::HAS_SKIPS {
                if let Some(k) = t.kind {
                    if G::is_skip(k) {
                        self.pending_skips.push_back(t);
                        continue;
                    }
                }
            }
            return t;
        }
    }

    /// Drain pending skip tokens whose end offset is `<= pos.offset`.
    /// Called just before emitting any event so skips are interleaved with
    /// structure in source order.
    fn flush_skips_before(&mut self, pos: Pos) {
        while let Some(front) = self.pending_skips.front() {
            if front.span.end.offset <= pos.offset {
                let t = self.pending_skips.pop_front().unwrap();
                self.queue.push_back(Event::Token(t));
            } else {
                break;
            }
        }
    }
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> Iterator
    for Parser<'a, L, K, G>
{
    type Item = Event<'a, G::TokenKind, G::RuleKind>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_event()
    }
}
