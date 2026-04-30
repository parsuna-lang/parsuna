use std::marker::PhantomData;

use crate::events::{Error, Event, RuleKindEnum, Token, TokenKindEnum};
use crate::lexer::LexerBackend;
use crate::span::Pos;

/// Sentinel state for "the parser has reached the end of the program".
///
/// When [`Parser::state`] equals this value the generated driver exits its
/// dispatch loop, and [`Parser::next_event`] performs the final EOF check
/// before yielding `None`.
pub const TERMINATED: u32 = 0;

/// Bridge from a generated grammar into the runtime's pull loop.
///
/// The code generator produces a zero-sized `Grammar` type that
/// implements this trait. [`Drive::step`] is the generated
/// state-dispatch function; [`Drive::is_skip`] checks whether a
/// token kind is a skip (whitespace, comments, etc.); `K` is the
/// lookahead required to decide every alternative in the grammar
/// (= LL(k)).
pub trait Drive<const K: usize>: Sized {
    type TokenKind: TokenKindEnum;
    type RuleKind: RuleKindEnum;
    /// True iff the grammar declares any `[skip]`-annotated tokens. Lets
    /// the runtime skip the per-pump skip check when no skips exist.
    const HAS_SKIPS: bool;
    /// Does `kind` denote a skip token (dropped from the structural stream
    /// and re-attached around structural events)?
    fn is_skip(kind: Self::TokenKind) -> bool;
    /// Run *one* state body — exactly one match arm of the
    /// generated dispatch — and return whatever event that body
    /// emitted, if any.
    ///
    /// `Some(event)` means the body's path through `Enter`/`Exit`/
    /// `Expect`/`error_here` produced an event. `None` means the body
    /// was a pure transition step: it changed `cur` (and possibly
    /// pushed/popped the return stack) but produced no event. The
    /// runtime's [`Parser::next_event`] loop calls `step` repeatedly
    /// until it returns `Some` (or until the modal logic above —
    /// pump, recovery, or termination — takes over).
    ///
    /// `step` is only invoked when the parser is in drive mode
    /// (lookahead full, no recovery armed, state not `TERMINATED`).
    fn step<'a, L: LexerBackend<'a, Self::TokenKind>>(
        p: &mut Parser<'a, L, K, Self>,
    ) -> Option<Event<'a, Self::TokenKind, Self::RuleKind>>;
}

/// In-flight error recovery. Armed by [`Parser::expect`] (mismatch
/// path) and [`Parser::recover_to`]; cleared by `next_event`'s
/// recovery branch once the lookahead lands on a sync token (or
/// EOF). Each `next_event` call in recovery mode yields exactly one
/// garbage Token before re-checking the sync set, so a long run of
/// unexpected input shows up in source order interleaved with skips.
struct Recovery<TK: 'static> {
    /// Token kinds to recover *to*. Recovery consumes garbage until
    /// the lookahead matches one of these (or EOF).
    sync: &'static [TK],
    /// `Some(kind)` when recovery was triggered by an `expect` for
    /// `kind`: if the sync set lands on `kind`, the recovery
    /// finaliser also consumes the token (so the surrounding rule
    /// proceeds as if it had matched). `None` for `Op::Dispatch`'s
    /// error path, where there's no expected kind to swallow on exit.
    expected: Option<TK>,
}

/// The pull-based parser over a grammar `G`.
///
/// On each call to [`next_event`](Self::next_event) the parser is in one
/// of three modes:
///
/// * **Skip** — the lookahead has at least one empty slot. Lex one token;
///   if it's a skip, yield it as `Event::Token`, otherwise drop it into
///   the slot and loop. Each call yields *one* skip event at most, so a
///   long comment run stays bounded at O(1) per `next_event`.
/// * **Recovery** — `recovery` is armed. Yield exactly one garbage Token
///   (or finalize recovery — clearing the field, optionally consuming the
///   matching sync token, then falling through to drive on the next call).
/// * **Drive** — neither of the above. Run the generated [`Drive::drive`]
///   until it emits one event, or until it hits a yield condition (consume
///   left lookahead empty, an error armed recovery, the parser terminated).
pub struct Parser<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> {
    lex: L,
    /// Lookahead ring. `None` slots are awaiting refill — Skip mode pulls
    /// lex tokens one-at-a-time until every slot holds a structural token.
    /// Generated `step()` code only ever reads lookahead when every slot
    /// is filled, so [`look`](Self::look) can unwrap unconditionally.
    look: [Option<Token<'a, G::TokenKind>>; K],
    prev_end: Pos,
    state: u32,
    ret_stack: Vec<u32>,
    recovery: Option<Recovery<G::TokenKind>>,
    eof_checked: bool,
    _grammar: PhantomData<fn(&mut Parser<'a, L, K, G>)>,
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> Parser<'a, L, K, G> {
    /// Build a parser over `lex`, starting at `entry` (the state id of the
    /// rule you want to parse — generated code exposes `ENTRY_FOO` constants
    /// for each public rule and `parse_foo_from_str` wrappers that call
    /// this).
    ///
    /// All `K` lookahead slots start empty; the first call to
    /// [`next_event`] enters Skip mode and primes them, so any leading
    /// skip tokens (e.g. whitespace before the first structural token)
    /// are emitted before the entry rule's `Enter`.
    pub fn new(lex: L, entry: u32) -> Self {
        Parser {
            lex,
            look: std::array::from_fn(|_| None),
            prev_end: Pos::default(),
            state: entry,
            ret_stack: Vec::with_capacity(64),
            recovery: None,
            eof_checked: false,
            _grammar: PhantomData,
        }
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

    /// Peek at the `i`-th lookahead token. `i` must be `< K`.
    ///
    /// Generated `drive()` only runs after Skip mode's pump has filled
    /// every slot, so the unwrap here is unconditional.
    #[inline]
    pub fn look(&self, i: usize) -> &Token<'a, G::TokenKind> {
        self.look[i]
            .as_ref()
            .expect("look slot empty inside drive() — pump did not refill before dispatch")
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
                if self.look(i).kind != Some(*want) {
                    continue 'seq;
                }
            }
            return true;
        }
        false
    }

    /// Build an `Enter` event for `rule` and return it. Records the
    /// rule's start position as `prev_end` so a later `Exit` without any
    /// intervening tokens still produces a zero-width span at the
    /// expected place.
    #[inline(always)]
    pub fn enter(&mut self, rule: G::RuleKind) -> Event<'a, G::TokenKind, G::RuleKind> {
        let pos = self.look(0).span.start;
        self.prev_end = pos;
        Event::Enter { rule, pos }
    }

    /// Build an `Exit` event for `rule`, positioned at the end of the last
    /// consumed token (or the rule's start for empty rules).
    #[inline(always)]
    pub fn exit(&mut self, rule: G::RuleKind) -> Event<'a, G::TokenKind, G::RuleKind> {
        let pos = self.prev_end;
        Event::Exit { rule, pos }
    }

    /// Build a recoverable error event pointing at the current lookahead.
    pub fn error_here(
        &mut self,
        msg: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        let span = self.look(0).span;
        Event::Error(Error::new(msg).at(span))
    }

    /// Try to consume a token of `kind`, returning the resulting event.
    /// On a hit this is a `consume` and yields a `Token` event. On a
    /// miss it produces an error event and arms recovery — `step` will
    /// return that error to the runtime, and subsequent
    /// [`next_event`] calls skip garbage one token at a time until the
    /// lookahead lands on `sync` (when it does, the matching token is
    /// also consumed so the surrounding rule continues as if `expect`
    /// had matched).
    #[inline(always)]
    pub fn expect(
        &mut self,
        kind: G::TokenKind,
        sync: &'static [G::TokenKind],
        expected_msg: &'static str,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        if self.look(0).kind == Some(kind) {
            return self.consume();
        }
        let event = self.error_here(expected_msg);
        self.arm_recovery(sync, Some(kind));
        event
    }

    /// Pop the current lookahead token, shifting the buffer up by one.
    /// Slot `K-1` is left empty so Skip mode can refill it (yielding
    /// one skip per call) before the next `step()` reads lookahead.
    /// Internal — callers wrap the result in the appropriate `Event`
    /// variant ([`consume`] for normal data, [`step_recovery`] for
    /// error-recovery garbage).
    #[inline]
    fn take_token(&mut self) -> Token<'a, G::TokenKind> {
        let t = self.look[0]
            .take()
            .expect("take_token called with empty lookahead");
        self.prev_end = t.span.end;
        // After the `take` above, slot 0 is `None`. `rotate_left(1)`
        // turns `[None, a, b, …]` into `[a, b, …, None]`, leaving
        // slot K-1 empty for Skip mode to refill before the next
        // `step()` reads lookahead.
        self.look.rotate_left(1);
        t
    }

    /// Consume the current lookahead token and return it as a
    /// [`Event::Token`]. Used on `expect`'s success path and on
    /// recovery's "synced to the kind we were expecting" path —
    /// both yield legitimate parse data.
    #[inline]
    pub fn consume(&mut self) -> Event<'a, G::TokenKind, G::RuleKind> {
        Event::Token(self.take_token())
    }

    /// Arm recovery without an expected kind. Called from a Dispatch
    /// op's error leaf: the surrounding `cur` is already pointing at
    /// the post-recovery state, and the queued error event makes
    /// drive() yield so recovery-mode can take over.
    pub fn recover_to(&mut self, sync: &'static [G::TokenKind]) {
        self.arm_recovery(sync, None);
    }

    /// Set up `self.recovery`. Internal — the two public arming paths
    /// (`expect` mismatch and `recover_to`) funnel through here so the
    /// shape stays in one place.
    #[inline]
    fn arm_recovery(&mut self, sync: &'static [G::TokenKind], expected: Option<G::TokenKind>) {
        self.recovery = Some(Recovery { sync, expected });
    }
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Drive<K>> Iterator
    for Parser<'a, L, K, G>
{
    type Item = Event<'a, G::TokenKind, G::RuleKind>;

    /// Produce the next event from the parse, or `None` once the entire
    /// input has been consumed.
    ///
    /// One iteration of the loop fires exactly one of three modes; each
    /// mode either yields one event or transitions out (so the loop
    /// retries).
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Skip mode: refill any empty lookahead slot. A skip token
            // becomes the yielded event; a structural token fills the
            // slot and the loop retries.
            //
            // Empty slots are always a contiguous suffix of `look` —
            // `consume`'s `rotate_left(1)` parks the new `None` at
            // index `K-1`, and `pump_one` always fills the leftmost
            // empty slot. So slot `K-1` is `None` iff *any* slot is
            // empty, and the leftmost empty slot is at `self.filled`
            // (the count of `Some`s currently in the prefix).
            if self.look[K - 1].is_none() {
                let t = self.lex.next_token();
                if G::HAS_SKIPS {
                    if let Some(k) = t.kind {
                        if G::is_skip(k) {
                            return Some(Event::Token(t));
                        }
                    }
                }
                let slot = self
                    .look
                    .iter()
                    .position(Option::is_none)
                    .expect("look[K-1] was None but no empty slot found — invariant broken");
                self.look[slot] = Some(t);
                continue;
            }

            // Recovery mode: advance one step. Three outcomes —
            // yield a `Garbage` event for an unexpected token, yield
            // a normal `Token` event when the sync hit on the kind
            // we were expecting, or finalise without consuming and
            // loop (sync hit on a non-expected kind / EOF).
            if let Some(rec) = self.recovery.as_ref() {
                let look0_kind = self.look[0].as_ref().and_then(|t| t.kind);
                let synced = matches!(
                    look0_kind,
                    Some(k) if k == G::TokenKind::EOF || rec.sync.contains(&k)
                );
                if synced {
                    let was_expected = rec.expected == look0_kind;
                    self.recovery = None;
                    if was_expected {
                        return Some(self.consume());
                    }
                    continue;
                }
                return Some(Event::Garbage(self.take_token()));
            }

            // EOF gate at the top of TERMINATED. If trailing input
            // remains, raise an error and arm a sync-empty recovery so
            // the rest of the input is drained as garbage Tokens, one
            // per call.
            if self.state == TERMINATED {
                if !self.eof_checked {
                    self.eof_checked = true;
                    if self.look[0].as_ref().and_then(|t| t.kind) != Some(G::TokenKind::EOF) {
                        let event = self.error_here("expected end of input");
                        self.arm_recovery(&[], None);
                        return Some(event);
                    }
                    continue;
                }
                return None;
            }

            // Drive mode: run one state body. If that body emitted,
            // yield. Otherwise it just transitioned `cur` (and maybe
            // the return stack); loop and run the next body.
            if let Some(e) = G::step(self) {
                return Some(e);
            }
        }
    }
}
