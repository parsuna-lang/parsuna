package dev.parsuna.runtime;

import java.util.Iterator;
import java.util.NoSuchElementException;

/**
 * Pull-based, recoverable parser. Obtain one via a generated {@code parseXxx}
 * factory and iterate it (or call {@link #next()} directly) to walk the
 * parse as a flat {@link Event} stream.
 *
 * <p>The parser keeps a ring of {@code k} lookahead slots (sized to the
 * grammar's LL(k)) and a return stack. Each call to {@link #next()} runs
 * in one of three modes — pump (refill lookahead, possibly yielding a
 * skip), recovery (one {@link Event.Garbage} or synced {@link Event.Token}),
 * or drive (one call to the generated {@code step}) — and yields at most
 * one event before looping again.
 *
 * <p><b>Memory model.</b> The parser pools every per-call object: one
 * {@link Token} per lookahead slot plus a spare for the just-yielded
 * event ({@code k+1} total), one {@link Event} of each variant, and one
 * {@link ParseError}. The reference returned by {@code next()} (and any
 * nested {@link Token} / {@link ParseError} / {@link Pos} / {@link Span})
 * stays valid only until the next {@code next()} call. If you need to
 * keep an event past that — for AST construction, etc. — call
 * {@code snapshot()} on whatever you want to keep.
 *
 * <p>The runtime hooks generated code calls into (lookahead access,
 * return stack, event builders, recovery arming) live on {@link Cursor}
 * instead, and the runtime owns a single shared {@code Cursor} that's
 * only handed to {@link DriveStep#step} from inside the pull loop —
 * external callers can't poke at parser internals out of band.
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    /**
     * In-flight error recovery. Set by {@link Cursor#tryConsume(int, int[], String)}'s
     * slow path and by {@link Cursor#recoverTo(int[])}; cleared once the
     * lookahead lands on a sync token (or EOF). Each call to
     * {@link #next()} yields exactly one garbage token (as
     * {@link Event.Garbage}) before looping, so a long run of unexpected
     * input doesn't pile up.
     */
    static final class Recovery {
        /** Token kinds to recover *to*. */
        final int[] sync;
        /**
         * {@code != -1} when the recovery was triggered by an expect for
         * this kind: if the sync set lands on it, recovery finalisation
         * also consumes the token. {@code -1} for the dispatch-error
         * path, where there's no expected kind to swallow on exit.
         */
        final int expected;
        Recovery(int[] sync, int expected) {
            this.sync = sync;
            this.expected = expected;
        }
    }

    final Lexer lex;
    final int k;

    // -----------------------------------------------------------------
    // Token pool — k+1 mutable Tokens. The pump rotates round-robin
    // through them, so at any point at most k+1 are live: k held by the
    // lookahead, plus either the just-yielded event's token *or* a
    // {@link #pendingLexGarbage} token (the two are mutually exclusive
    // in time — see the lex-failure absorb in {@link #next}). The slot
    // the round-robin counter is about to write next is always free.
    // -----------------------------------------------------------------
    private final Token[] pool;
    private int nextLexSlot = 0;

    /** Lookahead ring. Slots hold references into {@link #pool} (or
     *  {@code null} for empty). Pump fills slot {@code lookFilled} and
     *  increments; takeToken shifts and decrements. */
    final Token[] lookBuf;
    int lookFilled;

    /** End position of the most recently consumed token, stored as
     *  primitives so a freshly-created rule with no consumed tokens
     *  yet can still emit a zero-width Exit span without allocating
     *  a {@link Pos} per drive. */
    int prevEndOff, prevEndLine = 1, prevEndCol = 1;
    int state;

    /** Return stack as a primitive int[], grown on push. Beats
     *  {@code ArrayDeque<Integer>}: no autoboxing per push/pop. */
    int[] retStack = new int[16];
    int retTop = -1;

    Recovery recovery;

    /**
     * Holds the lex-failure token (kind = {@link Lexer#ERROR_KIND}) whose
     * paired {@link Event.Error} the previous {@link #next()} call returned;
     * this call owes the matching {@link Event.Garbage}. Lex-failure
     * tokens never enter {@link #lookBuf}, so dispatch can read
     * {@code look(i).kind()} as a real grammar token id without a
     * {@code ERROR_KIND} guard, and a stray bad byte doesn't push the
     * parser out of an active Star into SYNC recovery.
     */
    private Token pendingLexGarbage;

    // -----------------------------------------------------------------
    // Pooled event instances — one per variant. Cursor / pull-loop
    // emitters mutate these and return the matching reference.
    // -----------------------------------------------------------------
    final Event.Enter   evtEnter   = new Event.Enter();
    final Event.Exit    evtExit    = new Event.Exit();
    final Event.Token   evtToken   = new Event.Token();
    final Event.Garbage evtGarbage = new Event.Garbage();
    final Event.Error   evtError   = new Event.Error();

    final ParserConfig cfg;
    final ParserOptions opts;

    /** Single cursor instance reused across every drive call. */
    private final Cursor cursor = new Cursor(this);

    /**
     * Lookahead one event so {@link #hasNext()} can answer without
     * advancing. {@code aheadComputed} guards staleness; the cached
     * event is the same pooled reference {@link #next()} hands back, so
     * peek-then-consume is a no-op.
     */
    private Event ahead;
    private boolean aheadComputed;

    public Parser(Lexer lex, int entry, ParserConfig cfg) {
        this(lex, entry, cfg, new ParserOptions());
    }

    public Parser(Lexer lex, int entry, ParserConfig cfg, ParserOptions opts) {
        this.lex = lex;
        this.cfg = cfg;
        this.opts = opts;
        this.k = cfg.k;
        this.state = entry;
        this.lookBuf = new Token[cfg.k];
        this.pool = new Token[cfg.k + 1];
        for (int i = 0; i < pool.length; i++) pool[i] = new Token();
    }

    /** Round-robin handout from the token pool. The pool has k+1 slots
     *  while at most k+1 are ever live at once (k lookahead + 1
     *  just-yielded event), so the slot the cursor is about to return
     *  is guaranteed to not be currently observable by the caller. */
    private Token nextLexToken() {
        Token t = pool[nextLexSlot];
        nextLexSlot = nextLexSlot + 1;
        if (nextLexSlot == pool.length) nextLexSlot = 0;
        return t;
    }

    /** Pop the head of the lookahead and shift the rest up by one slot.
     *  Used on the consume / garbage paths — the runtime hands the
     *  consumed slot's {@link Token} reference straight to a pooled
     *  {@link Event.Token} / {@link Event.Garbage}. */
    Token takeToken() {
        Token t = lookBuf[0];
        prevEndOff = t.span().end().offset();
        prevEndLine = t.span().end().line();
        prevEndCol = t.span().end().column();
        for (int i = 0; i < lookFilled - 1; i++) lookBuf[i] = lookBuf[i + 1];
        lookBuf[lookFilled - 1] = null;
        lookFilled--;
        return t;
    }

    Event consume() {
        evtToken.token = takeToken();
        return evtToken;
    }

    Event garbage() {
        evtGarbage.token = takeToken();
        return evtGarbage;
    }

    Event errorHere(String msg) {
        Span s = lookBuf[0].span();
        evtError.error().set(msg,
            s.start().offset(), s.start().line(), s.start().column(),
            s.end().offset(), s.end().line(), s.end().column());
        return evtError;
    }

    /** True iff some lookahead slot still needs to be filled. */
    private boolean pumpPending() { return lookFilled < k; }

    /**
     * Produce the next event from the parse, or throw if input is fully
     * consumed. The whole pull loop lives here.
     *
     * <p>The loop runs three modes — pump (refill lookahead, possibly
     * yielding a skip), recovery (one Garbage / synced-Token event per
     * call), and drive (one step call). Each mode yields at most one
     * event before the loop runs again.
     */
    @Override public Event next() {
        if (aheadComputed) {
            aheadComputed = false;
            Event e = ahead;
            ahead = null;
            if (e == null) throw new NoSuchElementException();
            return e;
        }
        while (true) {
            // Pump-time-deferred Garbage half of a lex-failure pair: the
            // previous call returned the paired Error event; this call
            // returns the Garbage carrying the bad codepoint.
            if (pendingLexGarbage != null) {
                evtGarbage.token = pendingLexGarbage;
                pendingLexGarbage = null;
                return evtGarbage;
            }

            if (pumpPending()) {
                Token t = nextLexToken();
                lex.nextToken(t);
                int kind = t.kind();
                cfg.applyActions.apply(kind, lex);
                // Lex pattern miss — surface as a paired error+garbage so
                // the bad codepoint never enters lookahead. Keeps
                // dispatch's kind-based switch honest (it never has to
                // match against ERROR_KIND) and stops a stray bad byte
                // from pushing the parser out of an active Star into
                // SYNC recovery.
                if (kind == Lexer.ERROR_KIND) {
                    pendingLexGarbage = t;
                    Span s = t.span();
                    evtError.error().set("unexpected character",
                        s.start().offset(), s.start().line(), s.start().column(),
                        s.end().offset(), s.end().line(), s.end().column());
                    return evtError;
                }
                if (cfg.isSkip.test(kind)) {
                    if (!opts.dropSkips) {
                        evtToken.token = t;
                        return evtToken;
                    }
                    continue;
                }
                lookBuf[lookFilled++] = t;
                continue;
            }
            // Lookahead is guaranteed to carry a real grammar kind —
            // pump strips lex failures before they reach the buffer.
            if (recovery != null) {
                int look0 = lookBuf[0].kind();
                boolean synced = look0 == ParserConfig.EOF_KIND
                    || containsKind(recovery.sync, look0);
                if (synced) {
                    boolean wasExpected = recovery.expected == look0;
                    recovery = null;
                    if (wasExpected) return consume();
                    continue;
                }
                return garbage();
            }
            // EOF gate. On the first visit with trailing input, raise
            // an error and arm a sync-empty recovery so the rest of
            // the input drains as Garbage events one per call. Once
            // recovery has eaten its way to EOF the lookahead pins at
            // EOF (the lexer keeps yielding it), so this is naturally
            // idempotent — subsequent visits throw NoSuchElementException.
            if (state == TERMINATED) {
                if (lookBuf[0].kind() == ParserConfig.EOF_KIND) {
                    throw new NoSuchElementException();
                }
                Event ev = errorHere("expected end of input");
                recovery = new Recovery(new int[0], -1);
                return ev;
            }
            Event ev = cfg.step.step(cursor);
            if (ev != null) return ev;
        }
    }

    @Override public boolean hasNext() {
        if (aheadComputed) return ahead != null;
        try {
            ahead = next();
        } catch (NoSuchElementException e) {
            ahead = null;
        }
        aheadComputed = true;
        return ahead != null;
    }

    static boolean containsKind(int[] set, int k) {
        for (int x : set) if (x == k) return true;
        return false;
    }
}
