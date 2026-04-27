use std::marker::PhantomData;
use std::mem::MaybeUninit;

use crate::events::{Error, Event, RuleKindEnum, Token, TokenKindEnum};
use crate::lexer::LexerBackend;
use crate::span::Pos;

/// Fixed-capacity FIFO ring used as the parser's event queue.
///
/// Sized at construction by the `CAP` const generic the parser is
/// instantiated with — codegen passes the value computed by lowering
/// from the longest state body's emit burst, so the queue is exactly
/// large enough for the worst case the grammar can produce. No heap
/// allocation, no growth, no `VecDeque` bookkeeping.
///
/// Invariant: `len <= CAP`. `push_back` debug-asserts the bound; the
/// runtime layer above only pushes inside `drive()`/pump/recovery and
/// each path is bounded by the lowering pass, so an overflow is a
/// codegen bug.
struct EventRing<T, const CAP: usize> {
    buf: [MaybeUninit<T>; CAP],
    head: u32,
    len: u32,
}

impl<T, const CAP: usize> EventRing<T, CAP> {
    fn new() -> Self {
        // Inline `const` block ensures the slot constructor is
        // evaluated per-element rather than copied — which would
        // require `T: Copy` even though every slot is uninitialised.
        Self {
            buf: [const { MaybeUninit::uninit() }; CAP],
            head: 0,
            len: 0,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn push_back(&mut self, t: T) {
        let idx = (self.head as usize + self.len as usize) % CAP;
        self.buf[idx].write(t);
        self.len += 1;
    }

    #[inline]
    fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        // SAFETY: `head` always points at an initialised slot when
        // `len > 0` — `push_back` initialises the slot at
        // `(head + len - 1) % CAP` and increments `len`; we never
        // hand out the same slot twice without a `push_back` in
        // between because `head` advances on every pop.
        let val = unsafe { self.buf[self.head as usize].assume_init_read() };
        self.head = ((self.head as usize + 1) % CAP) as u32;
        self.len -= 1;
        Some(val)
    }
}

impl<T, const CAP: usize> Drop for EventRing<T, CAP> {
    fn drop(&mut self) {
        // Drain any in-flight events so their destructors run. A live
        // queue at parser drop time is not an error — callers may
        // discard a parser mid-stream — but the contained events still
        // own their data and need cleanup.
        while self.pop_front().is_some() {}
    }
}

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
    /// the runtime skip the per-pump skip check when no skips exist.
    const HAS_SKIPS: bool;
    /// Does `kind` denote a skip token (dropped from the structural stream
    /// and re-attached around structural events)?
    fn is_skip(kind: Self::TokenKind) -> bool;
    /// Run the generated dispatch loop. Executes states until either the
    /// event queue has something to emit or the parser hits [`TERMINATED`].
    fn drive<'a, const CAP: usize, L: LexerBackend<'a, Self::TokenKind>>(
        p: &mut Parser<'a, L, K, CAP, Self>,
    );
}

/// In-flight error recovery. Set by [`Parser::expect_slow`] and
/// [`Parser::recover_to`]; cleared once the lookahead lands on a sync
/// token (or EOF). Each call to [`Parser::next_event`] drains exactly
/// one garbage token before yielding, so a long run of unexpected
/// input doesn't pile up in the queue — the consumer sees recovery
/// tokens interleaved with their lex order.
struct Recovery<TK> {
    /// Token kinds to recover *to*. The recovery loop consumes garbage
    /// until the lookahead matches one of these (or EOF).
    sync: Vec<TK>,
    /// `Some(kind)` when the recovery was triggered by an `expect` for
    /// `kind`: if the sync set lands on `kind`, the recovery
    /// finalisation also consumes the token (so the surrounding rule
    /// proceeds as if it had matched). `None` for `Op::Dispatch`'s
    /// error path, where there's no expected kind to swallow on exit.
    expected: Option<TK>,
}

/// The pull-based parser over a grammar `G`.
///
/// The parser keeps a ring of `K` lookahead slots (sized to the grammar's
/// LL(k)), a return stack, and a bounded event queue. Each call to
/// [`next_event`](Self::next_event) drains a pending event, pumps one
/// lex token, advances recovery by one step, or — when none of those
/// have anything to do — invokes the generated [`Drive::drive`] to
/// execute the next state body.
///
/// **Skip handling** is now driven by pump-mode rather than a side
/// queue: when the lexer hands the runtime a skip token it lands
/// directly in the event queue, ahead of whatever structural event the
/// next `drive()` call will produce. The lookahead refills one lex
/// token at a time (yielding between each), so a long comment run
/// can't grow the queue past `QUEUE_CAP` — at any moment the queue
/// holds either a single pump push waiting to be drained, a single
/// recovery push, or the structural burst from one drive body.
pub struct Parser<
    'a,
    L: LexerBackend<'a, G::TokenKind>,
    const K: usize,
    const CAP: usize,
    G: Drive<K>,
> {
    lex: L,
    /// Lookahead ring. `None` slots are awaiting refill — the runtime's
    /// pump-mode in [`next_event`] pulls lex tokens one-at-a-time until
    /// every slot holds a structural token. Generated `drive()` code
    /// only ever runs when all slots are filled, so [`look`](Self::look)
    /// can unwrap unconditionally.
    look: [Option<Token<'a, G::TokenKind>>; K],
    prev_end: Pos,
    state: u32,
    ret_stack: Vec<u32>,
    queue: EventRing<Event<'a, G::TokenKind, G::RuleKind>, CAP>,
    recovery: Option<Recovery<G::TokenKind>>,
    eof_checked: bool,
    _grammar: PhantomData<fn(&mut Parser<'a, L, K, CAP, G>)>,
}

impl<
        'a,
        L: LexerBackend<'a, G::TokenKind>,
        const K: usize,
        const CAP: usize,
        G: Drive<K>,
    > Parser<'a, L, K, CAP, G>
{
    /// Build a parser over `lex`, starting at `entry` (the state id of the
    /// rule you want to parse — generated code exposes `ENTRY_FOO` constants
    /// for each public rule and `parse_foo_from_str` wrappers that call
    /// this).
    ///
    /// All `K` lookahead slots start empty; the first call to
    /// [`next_event`] enters pump-mode and primes them, so any leading
    /// skip tokens (e.g. whitespace before the first structural token)
    /// are emitted before the entry rule's `Enter`.
    pub fn new(lex: L, entry: u32) -> Self {
        Parser {
            lex,
            look: std::array::from_fn(|_| None),
            prev_end: Pos::default(),
            state: entry,
            ret_stack: Vec::with_capacity(64),
            queue: EventRing::new(),
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

    /// True if there is nothing queued to emit. Generated `drive` loops
    /// continue only while this holds — the moment an event lands in the
    /// queue, the loop must yield so `next_event` can return it.
    #[inline]
    pub fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Peek at the `i`-th lookahead token. `i` must be `< K`.
    ///
    /// Generated `drive()` only runs after [`next_event`]'s pump-mode
    /// has filled every slot, so the unwrap here is unconditional.
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

    /// Append an event to the output queue. The bound is enforced by
    /// `EventRing::push_back`'s debug-assert; this thin wrapper just
    /// keeps the call sites small and lets us add policy here later
    /// without touching every emit path.
    #[inline(always)]
    fn push_event(&mut self, ev: Event<'a, G::TokenKind, G::RuleKind>) {
        self.queue.push_back(ev);
    }

    /// Emit an `Enter` event for `rule`. Records the rule's start position
    /// as `prev_end` so a later `Exit` without any intervening tokens still
    /// produces a zero-width span at the expected place.
    #[inline(always)]
    pub fn enter(&mut self, rule: G::RuleKind) {
        let pos = self.look(0).span.start;
        self.prev_end = pos;
        self.push_event(Event::Enter { rule, pos });
    }

    /// Emit an `Exit` event for `rule`, positioned at the end of the last
    /// consumed token (or the rule's start for empty rules).
    #[inline(always)]
    pub fn exit(&mut self, rule: G::RuleKind) {
        let pos = self.prev_end;
        self.push_event(Event::Exit { rule, pos });
    }

    /// Append an event to the output queue.
    ///
    /// Skip-token interleaving used to live here (the old design held a
    /// `pending_skips` side queue and flushed it before any structural
    /// emit); pump-mode now drains skips one at a time between
    /// `drive()` calls, so this is a plain push that respects the
    /// queue cap.
    #[inline(always)]
    pub fn emit(&mut self, ev: Event<'a, G::TokenKind, G::RuleKind>) {
        self.push_event(ev);
    }

    /// Raise a recoverable error pointing at the current lookahead.
    pub fn error_here(&mut self, msg: impl Into<std::borrow::Cow<'static, str>>) {
        let span = self.look(0).span;
        self.push_event(Event::Error(Error::new(msg).at(span)));
    }

    /// Try to consume a token of `kind`; on mismatch, emit an error and
    /// hand recovery off to the runtime's recovery-mode.
    ///
    /// `sync` is typically the caller rule's FOLLOW set: recovery skips
    /// unexpected tokens until the lookahead lands on one of these,
    /// which gives the surrounding rule a reasonable place to resume.
    /// On the slow path `expect` returns immediately after staging the
    /// error and recovery — drive's loop sees the queued error and
    /// yields, then [`next_event`]'s recovery-mode advances one
    /// garbage token per call until the sync set is hit.
    #[inline(always)]
    pub fn expect(
        &mut self,
        kind: G::TokenKind,
        sync: &[G::TokenKind],
        expected_msg: &'static str,
    ) {
        if self.look(0).kind == Some(kind) {
            self.consume();
            return;
        }
        self.expect_slow(kind, sync, expected_msg);
    }

    /// Cold path of [`expect`]: stage the error event and arm
    /// recovery-mode with the expected kind so the post-recovery
    /// finaliser can swallow the matching token if the sync set lands
    /// on it. Returns immediately — the actual garbage-skipping happens
    /// across subsequent [`next_event`] calls.
    #[cold]
    #[inline(never)]
    fn expect_slow(
        &mut self,
        kind: G::TokenKind,
        sync: &[G::TokenKind],
        expected_msg: &'static str,
    ) {
        self.error_here(expected_msg);
        self.recovery = Some(Recovery {
            sync: sync.to_vec(),
            expected: Some(kind),
        });
    }

    /// Consume the current lookahead token, emit it, and shift the buffer
    /// up by one. Slot `K-1` is left empty so the runtime's pump-mode can
    /// refill it (yielding one skip per call) before the next `drive()`
    /// reads lookahead.
    ///
    /// The state-splitting invariant in `fuse` guarantees an `Op::Expect`
    /// is always followed only by control ops in the same state body —
    /// so `consume` never has to coexist with another lookahead-reading
    /// op before the next yield.
    #[inline]
    pub fn consume(&mut self) {
        let t = self.look[0]
            .take()
            .expect("consume called with empty lookahead");
        self.prev_end = t.span.end;
        for i in 0..K - 1 {
            self.look[i] = self.look[i + 1].take();
        }
        // After the shifts, slot K-1 is `None` (either freshly None or
        // moved out by the last `take()`). The runtime's pump-mode in
        // `next_event` refills it before the next `drive()` runs.
        self.push_event(Event::Token(t));
    }

    /// Arm recovery-mode without an expected kind. Called from
    /// `Op::Dispatch`'s error leaf: the surrounding `cur` was already
    /// set to the post-recovery state by codegen, and the queued
    /// `Error` event makes drive() yield immediately so recovery-mode
    /// can take over.
    pub fn recover_to(&mut self, sync: &[G::TokenKind]) {
        self.recovery = Some(Recovery {
            sync: sync.to_vec(),
            expected: None,
        });
    }

    /// True iff some lookahead slot still needs to be filled. Pump-mode
    /// runs whenever this holds, so generated `drive()` code can read
    /// any `look(i)` unconditionally.
    fn pump_pending(&self) -> bool {
        self.look.iter().any(|s| s.is_none())
    }

    /// Lex one token. If it's a skip, push it directly onto the event
    /// queue and leave pump-mode armed for another call. If it's a
    /// structural token, fill the leftmost empty lookahead slot.
    ///
    /// Yielding per skip is what makes the queue cap honest — a long
    /// comment run can't grow the queue past 1 (next_event drains the
    /// just-pushed skip on the next loop iteration before pumping again).
    fn pump_one(&mut self) {
        let t = self.lex.next_token();
        if G::HAS_SKIPS {
            if let Some(k) = t.kind {
                if G::is_skip(k) {
                    self.push_event(Event::Token(t));
                    return;
                }
            }
        }
        let slot = self
            .look
            .iter()
            .position(|s| s.is_none())
            .expect("pump_one called with all slots filled");
        self.look[slot] = Some(t);
    }

    /// Advance recovery by one step. Either consume one garbage token
    /// (one Token push, drive yield) or — if the lookahead is in the
    /// sync set / EOF — finalise by clearing recovery and (when a
    /// matching `expected` was set) swallowing the synced-to token.
    fn recover_one(&mut self) {
        let rec = self
            .recovery
            .as_ref()
            .expect("recover_one called without active recovery");
        let look0_kind = self.look[0].as_ref().and_then(|t| t.kind);
        match look0_kind {
            Some(k) if k == G::TokenKind::EOF => {
                self.recovery = None;
            }
            Some(k) if rec.sync.contains(&k) => {
                let was_expected = rec.expected == Some(k);
                self.recovery = None;
                if was_expected {
                    self.consume();
                }
            }
            _ => {
                self.consume();
            }
        }
    }

    /// Produce the next event from the parse, or `None` once the entire
    /// input has been consumed.
    ///
    /// The loop layers four kinds of progress, ordered so the consumer
    /// always sees the soonest-available event:
    ///
    /// 1. Drain a queued event.
    /// 2. Pump one lex token (filling lookahead, or queuing a skip).
    /// 3. Advance recovery by one step.
    /// 4. Run the generated dispatch for one drive call.
    ///
    /// The bound on (2) and (3) per iteration is what keeps the queue
    /// honest: each contributes at most one event before yielding, so
    /// the queue never grows past `QUEUE_CAP` (the structural burst
    /// from a single drive body).
    #[inline]
    pub fn next_event(&mut self) -> Option<Event<'a, G::TokenKind, G::RuleKind>> {
        loop {
            if let Some(e) = self.queue.pop_front() {
                return Some(e);
            }
            if self.pump_pending() {
                self.pump_one();
                continue;
            }
            if self.recovery.is_some() {
                self.recover_one();
                continue;
            }
            if self.state == TERMINATED {
                if !self.eof_checked {
                    self.eof_checked = true;
                    if self.look[0].as_ref().and_then(|t| t.kind)
                        != Some(G::TokenKind::EOF)
                    {
                        // Trailing input past the entry rule. Synthesize
                        // an error and use recovery-mode (with an empty
                        // sync set) to drain the rest as Token events
                        // one yield at a time.
                        self.error_here("expected end of input");
                        self.recovery = Some(Recovery {
                            sync: Vec::new(),
                            expected: None,
                        });
                        continue;
                    }
                    continue;
                }
                return None;
            }
            G::drive(self);
        }
    }
}

impl<
        'a,
        L: LexerBackend<'a, G::TokenKind>,
        const K: usize,
        const CAP: usize,
        G: Drive<K>,
    > Iterator for Parser<'a, L, K, CAP, G>
{
    type Item = Event<'a, G::TokenKind, G::RuleKind>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_event()
    }
}
