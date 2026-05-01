use std::marker::PhantomData;

use crate::events::{Error, Event, RuleKindEnum, Token, TokenKindEnum};
use crate::lexer::LexerBackend;
use crate::span::Pos;

/// Sentinel state for "the parser has reached the end of the program".
///
/// When the current state equals this value the generated [`Grammar::step`]
/// exits its dispatch, and the runtime's pull loop performs the final
/// EOF check before yielding `None`.
pub const TERMINATED: u32 = 0;

/// Compile-time configuration for the [`Parser`].
///
/// Implementors are zero-sized markers that flip behaviour through
/// associated `const`s. The runtime only branches on `EMIT_SKIPS` today;
/// when it's `false` the dead arm in [`Iterator::next`] is removed by
/// monomorphization, so a parser that drops whitespace pays no runtime
/// cost for the decision.
pub trait ParserConfig {
    /// When `false`, skip-kind tokens (whitespace, comments, etc.) are
    /// silently consumed instead of yielded as `Event::Token`. The
    /// structural event stream is unchanged.
    const EMIT_SKIPS: bool;
}

/// Default config: skip tokens are surfaced as `Event::Token` so the
/// caller can re-attach trivia, render the input verbatim, or drop them
/// itself.
pub struct EmitSkips;
impl ParserConfig for EmitSkips {
    const EMIT_SKIPS: bool = true;
}

/// Drop skip tokens at the source. The parser still lexes them (skip
/// patterns delimit the structural ones), but they never reach the
/// iterator. Picking this lets monomorphization remove the skip-emit
/// branch entirely.
pub struct DropSkips;
impl ParserConfig for DropSkips {
    const EMIT_SKIPS: bool = false;
}

/// Bridge from a generated grammar into the runtime's pull loop.
///
/// The code generator produces a zero-sized marker type (named after
/// the grammar — e.g. `Json`) that implements this trait.
/// [`Grammar::step`] is the generated state-dispatch function;
/// [`Grammar::is_skip`] checks whether a token kind is a skip
/// (whitespace, comments, etc.); `K` is the lookahead required to
/// decide every alternative in the grammar (= LL(k)).
pub trait Grammar<const K: usize>: Sized {
    type TokenKind: TokenKindEnum;
    type RuleKind: RuleKindEnum;
    /// True iff the grammar declares any tokens with a `-> skip` action.
    /// Lets the runtime skip the per-pump skip check when no skips exist.
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
    /// was a pure transition step: it changed the current state (and
    /// possibly pushed/popped the return stack) but produced no event.
    /// The runtime's pull loop calls `step` again in that case.
    ///
    /// The [`Cursor`] handle is the only public surface for talking back
    /// to the runtime — it's a thin wrapper around the parser that
    /// exposes just the operations generated dispatch needs (read
    /// lookahead, push/pop the return stack, build events, arm
    /// recovery). External callers can't construct one, so the parser's
    /// internal state stays sealed: the only way to drive a parse is
    /// through [`Iterator::next`].
    fn step<'a, 'p, L: LexerBackend<'a, Self::TokenKind>, C: ParserConfig>(
        p: &mut Cursor<'p, 'a, L, K, Self, C>,
    ) -> Option<Event<'a, Self::TokenKind, Self::RuleKind>>;

    /// Apply the mode-stack actions declared on `kind` to `lex`.
    ///
    /// Generated code emits this as a `match` over token kinds, calling
    /// `lex.push_mode(...)` / `lex.pop_mode()` in source order for each
    /// declared `-> push(...)` / `-> pop` action. Tokens with no actions
    /// fall through with no effect. Called once per token immediately
    /// after the lexer hands it to the runtime, so by the time the next
    /// `next_token()` runs the lexer's mode stack is up to date.
    ///
    /// Default impl is a no-op for grammars that declare no modes.
    #[inline]
    fn apply_actions<'a, L: LexerBackend<'a, Self::TokenKind>>(
        kind: Option<Self::TokenKind>,
        lex: &mut L,
    ) {
        let _ = (kind, lex);
    }
}

/// One slot on the parser's return stack: the state to resume at
/// when the current rule's body finishes, plus the lex mode-stack
/// depth captured at the moment the corresponding `PushRet` ran (so
/// recovery can unwind interior mode pushes).
#[derive(Clone, Copy, Debug)]
struct RetFrame {
    state: u32,
    mode_depth: usize,
}

/// In-flight error recovery. Armed by [`Cursor::expect`] (mismatch path)
/// and [`Cursor::recover_to`]; cleared by the pull loop's recovery branch
/// once the lookahead lands on a sync token (or EOF). Each call in
/// recovery mode yields exactly one garbage Token before re-checking
/// the sync set, so a long run of unexpected input shows up in source
/// order interleaved with skips.
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
/// On each call to [`Iterator::next`] the parser is in one of three modes:
///
/// * **Skip** — the lookahead has at least one empty slot. Lex one token;
///   if it's a skip, yield it as `Event::Token`, otherwise drop it into
///   the slot and loop. Each call yields *one* skip event at most, so a
///   long comment run stays bounded at O(1) per call.
/// * **Recovery** — `recovery` is armed. Yield exactly one garbage Token
///   (or finalize recovery — clearing the field, optionally consuming the
///   matching sync token, then falling through to drive on the next call).
/// * **Drive** — neither of the above. Run the generated [`Grammar::step`]
///   until it emits one event, or until it hits a yield condition (consume
///   left lookahead empty, an error armed recovery, the parser terminated).
///
/// The parser's only public surface is [`Parser::new`] plus the
/// [`Iterator`] impl. All the runtime hooks generated code calls into
/// (lookahead access, return stack, event builders, recovery arming)
/// live on [`Cursor`] instead, and a `Cursor` can only be obtained from
/// inside the pull loop — so external callers can't poke at parser
/// internals out of band.
pub struct Parser<
    'a,
    L: LexerBackend<'a, G::TokenKind>,
    const K: usize,
    G: Grammar<K>,
    C: ParserConfig = EmitSkips,
> {
    lex: L,
    /// Lookahead ring. `None` slots are awaiting refill — Skip mode pulls
    /// lex tokens one-at-a-time until every slot holds a structural token.
    /// Generated `step()` code only ever reads lookahead when every slot
    /// is filled, so [`Cursor::look`] can unwrap unconditionally. Lex
    /// failures (`Token { kind: None, .. }`) are surfaced via the
    /// pump-time absorb below and never enter the buffer, so once a slot
    /// is filled its `kind` is always `Some(_)`.
    look: [Option<Token<'a, G::TokenKind>>; K],
    prev_end: Pos,
    state: u32,
    /// Return stack. Each entry pairs the state to resume at with the
    /// lex mode-stack depth at the moment the rule was *entered*. On
    /// recovery we use the top entry's depth to pop the lex mode
    /// stack back to where the now-erroring rule started — modes
    /// pushed mid-rule (e.g. by an opener token) get unwound so the
    /// SYNC scan happens in the right context, and stray input that
    /// would otherwise leave the lexer marooned in an interior mode
    /// can't propagate past the recovering rule.
    ret_stack: Vec<RetFrame>,
    recovery: Option<Recovery<G::TokenKind>>,
    /// Holds the lex-failure token whose paired [`Event::Error`] the
    /// previous [`Iterator::next`] call returned; this call owes the
    /// matching [`Event::Garbage`]. Lex-failure tokens never enter
    /// `look`, so dispatch can read `look[i].kind.unwrap()` without
    /// caring about the no-pattern-matched path.
    pending_lex_garbage: Option<Token<'a, G::TokenKind>>,
    _config: PhantomData<C>,
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Grammar<K>, C: ParserConfig>
    Parser<'a, L, K, G, C>
{
    /// Build a parser over `lex`, starting at `entry` (the state id of the
    /// rule you want to parse — generated code exposes `ENTRY_FOO` constants
    /// for each public rule and `parse_foo_from_str` wrappers that call
    /// this).
    ///
    /// All `K` lookahead slots start empty; the first call to [`Iterator::next`]
    /// enters Skip mode and primes them, so any leading skip tokens (e.g.
    /// whitespace before the first structural token) are emitted before the
    /// entry rule's `Enter`.
    pub fn new(lex: L, entry: u32) -> Self {
        Parser {
            lex,
            look: std::array::from_fn(|_| None),
            prev_end: Pos::default(),
            state: entry,
            ret_stack: Vec::with_capacity(64),
            recovery: None,
            pending_lex_garbage: None,
            _config: PhantomData,
        }
    }

    /// Pop the current lookahead token, shifting the buffer up by one.
    /// Slot `K-1` is left empty so Skip mode can refill it (yielding
    /// one skip per call) before the next `step()` reads lookahead.
    /// Internal — callers wrap the result in the appropriate `Event`
    /// variant.
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

    #[inline]
    fn consume(&mut self) -> Event<'a, G::TokenKind, G::RuleKind> {
        Event::Token(self.take_token())
    }

    fn error_here(
        &mut self,
        msg: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        let span = self.look[0]
            .as_ref()
            .expect("error_here called with empty lookahead")
            .span;
        Event::Error(Error::new(msg).at(span))
    }

    #[inline]
    fn arm_recovery(&mut self, sync: &'static [G::TokenKind], expected: Option<G::TokenKind>) {
        self.recovery = Some(Recovery { sync, expected });
        // Unwind any interior mode pushes the now-erroring rule made.
        // The top of `ret_stack` is the call frame for whichever
        // rule's body we're currently inside, and its `mode_depth`
        // was captured at the moment that rule was entered. Popping
        // back to it brings the lexer to the same context the
        // surrounding caller expects, so SYNC tokens are interpreted
        // in the right mode and a stray push can't leave the lexer
        // marooned past the recovery point. With an empty stack
        // (recovery in the entry rule) we restore to depth 1 — the
        // default mode, which is where the parser started.
        let target = self
            .ret_stack
            .last()
            .map(|f| f.mode_depth)
            .unwrap_or(1);
        self.lex.pop_modes_to(target);
    }
}

/// The handle that generated `step` bodies talk to the runtime through.
///
/// Wraps a `&mut Parser` and re-exports just the operations dispatch
/// needs — lookahead access, return stack pushes, event builders,
/// recovery arming. External code can't construct a `Cursor` (the field
/// is private and there's no public constructor), so the only way one
/// ever exists is inside a call to [`Grammar::step`] from the runtime's
/// pull loop. That keeps the parser's internal state from being poked
/// at out of band.
pub struct Cursor<
    'p,
    'a,
    L: LexerBackend<'a, G::TokenKind>,
    const K: usize,
    G: Grammar<K>,
    C: ParserConfig,
> {
    p: &'p mut Parser<'a, L, K, G, C>,
}

impl<
        'p,
        'a,
        L: LexerBackend<'a, G::TokenKind>,
        const K: usize,
        G: Grammar<K>,
        C: ParserConfig,
    > Cursor<'p, 'a, L, K, G, C>
{
    /// Current state id. Generated dispatch reads this at the start of
    /// each iteration and writes it back when the loop suspends.
    #[inline]
    pub fn state(&self) -> u32 {
        self.p.state
    }

    /// Overwrite the current state. Used by generated code to move
    /// between states inside a dispatch step.
    #[inline]
    pub fn set_state(&mut self, s: u32) {
        self.p.state = s;
    }

    /// Push a return address onto the call stack. Used for rule calls
    /// (the caller saves the state to resume after the callee's `Ret`)
    /// and for `*`/`+` loops (each iteration re-enters the loop state).
    ///
    /// The current lex mode-stack depth is captured alongside the
    /// state — recovery uses it to unwind interior mode pushes back
    /// to the rule's entry depth (see [`Parser::arm_recovery`]).
    #[inline]
    pub fn push_ret(&mut self, s: u32) {
        let mode_depth = self.p.lex.mode_depth();
        self.p.ret_stack.push(RetFrame {
            state: s,
            mode_depth,
        });
    }

    /// Pop the top return address, or [`TERMINATED`] if the stack is
    /// empty (meaning we have finished the entry rule).
    #[inline]
    pub fn ret(&mut self) -> u32 {
        self.p.ret_stack.pop().map(|f| f.state).unwrap_or(TERMINATED)
    }

    /// Peek at the `i`-th lookahead token. `i` must be `< K`.
    ///
    /// Generated `step()` only runs after Skip mode's pump has filled
    /// every slot, so the unwrap here is unconditional.
    #[inline]
    pub fn look(&self, i: usize) -> &Token<'a, G::TokenKind> {
        self.p.look[i]
            .as_ref()
            .expect("look slot empty inside step() — pump did not refill before dispatch")
    }

    /// Test whether the current lookahead starts with any of the given
    /// sequences. Used by `*`, `+`, `?` dispatch to check whether the
    /// body of the loop/option matches at the current position.
    ///
    /// Each inner slice is a prefix to try; the function returns `true`
    /// on the first full prefix match.
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

    /// Build an `Enter` event for `rule`. Records the rule's start
    /// position so a later `Exit` without any intervening tokens still
    /// produces a zero-width span at the expected place.
    #[inline(always)]
    pub fn enter(&mut self, rule: G::RuleKind) -> Event<'a, G::TokenKind, G::RuleKind> {
        let pos = self.look(0).span.start;
        self.p.prev_end = pos;
        Event::Enter { rule, pos }
    }

    /// Build an `Exit` event for `rule`, positioned at the end of the
    /// last consumed token (or the rule's start for empty rules).
    #[inline(always)]
    pub fn exit(&mut self, rule: G::RuleKind) -> Event<'a, G::TokenKind, G::RuleKind> {
        Event::Exit {
            rule,
            pos: self.p.prev_end,
        }
    }

    /// Build a recoverable error event pointing at the current lookahead.
    pub fn error_here(
        &mut self,
        msg: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        self.p.error_here(msg)
    }

    /// Try to consume a token of `kind`, returning the resulting event.
    /// On a hit this is a `consume` and yields a `Token` event. On a
    /// miss it produces an error event and arms recovery — `step` will
    /// return that error to the runtime, and subsequent pull-loop calls
    /// skip garbage one token at a time until the lookahead lands on
    /// `sync` (when it does, the matching token is also consumed so the
    /// surrounding rule continues as if `expect` had matched).
    #[inline(always)]
    pub fn expect(
        &mut self,
        kind: G::TokenKind,
        sync: &'static [G::TokenKind],
        expected_msg: &'static str,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        self.expect_labeled(kind, sync, expected_msg, None)
    }

    /// Same as [`Cursor::expect`] but stamps `label` on the consumed
    /// token's `Token::label` field on the success path. Used by
    /// generated dispatch code for `name:NAME`-form positions in the
    /// grammar; the label travels through to the consumer's `Event`
    /// stream so they can identify the position by name without
    /// tracking surrounding rule context.
    #[inline(always)]
    pub fn expect_labeled(
        &mut self,
        kind: G::TokenKind,
        sync: &'static [G::TokenKind],
        expected_msg: &'static str,
        label: Option<&'static str>,
    ) -> Event<'a, G::TokenKind, G::RuleKind> {
        if self.look(0).kind == Some(kind) {
            // Stamp the label directly on the slot's token before
            // `consume` rotates it out — saves a per-token branch in
            // the unlabeled case (label stays `None` from
            // construction).
            if let Some(t) = self.p.look[0].as_mut() {
                t.label = label;
            }
            return self.consume();
        }
        let event = self.error_here(expected_msg);
        self.p.arm_recovery(sync, Some(kind));
        event
    }

    /// Consume the current lookahead token and return it as an
    /// [`Event::Token`]. Used on `expect`'s success path and on
    /// recovery's "synced to the kind we were expecting" path — both
    /// yield legitimate parse data.
    #[inline]
    pub fn consume(&mut self) -> Event<'a, G::TokenKind, G::RuleKind> {
        self.p.consume()
    }

    /// Arm recovery without an expected kind. Called from a Dispatch
    /// op's error leaf: the surrounding state is already pointing at
    /// the post-recovery state, and the error event paired with this
    /// call makes the pull loop yield so recovery-mode can take over.
    pub fn recover_to(&mut self, sync: &'static [G::TokenKind]) {
        self.p.arm_recovery(sync, None);
    }
}

impl<'a, L: LexerBackend<'a, G::TokenKind>, const K: usize, G: Grammar<K>, C: ParserConfig>
    Iterator for Parser<'a, L, K, G, C>
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
            // Pump-time-deferred `Garbage` half of a lex-failure pair:
            // the previous call returned the paired `Error` event;
            // this call returns the `Garbage` carrying the bad
            // codepoint.
            if let Some(t) = self.pending_lex_garbage.take() {
                return Some(Event::Garbage(t));
            }

            // Pump mode: refill any empty lookahead slot. Slots fill
            // leftmost-first and `consume`'s `rotate_left(1)` parks
            // the new `None` at index `K-1`, so slot `K-1` is `None`
            // iff *any* slot is empty. Three pump outcomes:
            //   - skip token: yield it (when `EMIT_SKIPS`) or loop;
            //   - lex failure (`kind: None`): surface as a paired
            //     error+garbage, don't enter the buffer — keeps
            //     `look(i).kind` always a real `TK` for dispatch, and
            //     stops a stray bad byte from pushing the parser out
            //     of an active Star into SYNC recovery;
            //   - structural token: fill the slot and loop.
            if self.look[K - 1].is_none() {
                let t = self.lex.next_token();
                // Apply token-level mode-stack actions before deciding
                // skip vs. structural — `-> skip, push(foo)` is invalid
                // (parser-level check), so a skip token never has actions
                // and a token-with-actions never skips. Calling
                // unconditionally keeps the call site simple; the default
                // impl is empty for action-free grammars.
                G::apply_actions(t.kind, &mut self.lex);
                let Some(k) = t.kind else {
                    let span = t.span;
                    self.pending_lex_garbage = Some(t);
                    return Some(Event::Error(
                        Error::new("unexpected character").at(span),
                    ));
                };
                if G::HAS_SKIPS && G::is_skip(k) {
                    // Compile-time gate: with `C = DropSkips`,
                    // monomorphization removes the yield arm, so
                    // the skip token is silently consumed.
                    if C::EMIT_SKIPS {
                        return Some(Event::Token(t));
                    }
                    continue;
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
            // loop (sync hit on a non-expected kind / EOF). Lookahead
            // is guaranteed to carry a real kind — pump strips lex
            // failures before they reach the buffer.
            if let Some(rec) = self.recovery.as_ref() {
                let look0_kind = self.look[0]
                    .as_ref()
                    .expect("look slot empty in recovery — invariant broken")
                    .kind
                    .expect("look kind None in recovery — pump should have absorbed it");
                let synced = look0_kind == G::TokenKind::EOF || rec.sync.contains(&look0_kind);
                if synced {
                    let was_expected = rec.expected == Some(look0_kind);
                    self.recovery = None;
                    if was_expected {
                        return Some(self.consume());
                    }
                    continue;
                }
                return Some(Event::Garbage(self.take_token()));
            }

            // EOF gate. On the first visit with trailing input, raise
            // an error and arm a sync-empty recovery so the rest of the
            // input is drained as garbage Tokens, one per call. Once
            // recovery has eaten its way to EOF the lookahead pins at
            // EOF (the lexer keeps yielding it), so this is naturally
            // idempotent — subsequent visits just return `None`.
            if self.state == TERMINATED {
                if self.look[0].as_ref().and_then(|t| t.kind) == Some(G::TokenKind::EOF) {
                    return None;
                }
                let event = self.error_here("expected end of input");
                self.arm_recovery(&[], None);
                return Some(event);
            }

            // Drive mode: run one state body. If that body emitted,
            // yield. Otherwise it just transitioned the current state
            // (and maybe the return stack); loop and run the next body.
            if let Some(e) = G::step(&mut Cursor { p: self }) {
                return Some(e);
            }
        }
    }
}
