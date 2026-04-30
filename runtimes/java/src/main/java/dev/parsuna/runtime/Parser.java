package dev.parsuna.runtime;

import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.NoSuchElementException;

/**
 * Pull-based, recoverable parser. Obtain one via a generated {@code parseXxx}
 * factory and iterate it (or call {@link #nextEvent()} directly) to walk the
 * parse as a flat {@link Event} stream.
 *
 * <p>The parser keeps a ring of {@code k} lookahead slots (sized to the
 * grammar's LL(k)) and a return stack. Each call to {@link #nextEvent()}
 * runs in one of three modes — pump (refill lookahead, possibly yielding
 * a skip), recovery (one {@link Event.Garbage} or synced {@link Event.Token}),
 * or drive (one call to the generated {@code step}) — and yields at most
 * one event before looping again.
 *
 * <p>Skip handling is driven by pump-mode: when the lexer hands the
 * runtime a skip token {@code nextEvent} returns it as a
 * {@link Event.Token} directly. The lookahead refills one lex token at
 * a time, so a long comment run never piles up — each skip is yielded
 * in turn before the next pump iteration runs.
 *
 * <p>{@code Parser}'s only public surface is the constructor,
 * {@link #nextEvent()}, and the {@link Iterator} impl. The runtime hooks
 * generated code calls into (lookahead access, return stack, event
 * builders, recovery arming) live on {@link Cursor} instead, and a
 * {@code Cursor} can only be obtained from inside the pull loop — so
 * external callers can't poke at parser internals out of band.
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    /**
     * In-flight error recovery. Set by {@link Cursor#tryConsume(int, int[], String)}'s
     * slow path and by {@link Cursor#recoverTo(int[])}; cleared once the
     * lookahead lands on a sync token (or EOF). Each call to
     * {@link #nextEvent()} yields exactly one garbage token (as
     * {@link Event.Garbage}) before looping, so a long run of unexpected
     * input doesn't pile up.
     */
    static final class Recovery {
        /** Token kinds to recover *to*. */
        final int[] sync;
        /**
         * {@code != -1} when the recovery was triggered by an expect for
         * this kind: if the sync set lands on it, recovery finalisation
         * also consumes the token. {@code -1} for the dispatch-error path,
         * where there's no expected kind to swallow on exit.
         */
        final int expected;
        Recovery(int[] sync, int expected) {
            this.sync = sync;
            this.expected = expected;
        }
    }

    private final Lexer lex;
    /**
     * Lookahead ring. Null slots are awaiting refill — pump-mode in
     * nextEvent pulls lex tokens one at a time until every slot holds a
     * structural token. Generated step code only ever runs when all slots
     * are filled, so {@link Cursor#look(int)} can return the value
     * unconditionally.
     *
     * <p>Empty slots are always a contiguous suffix — takeToken parks
     * the new null at index k-1 and pump fills the leftmost empty slot.
     *
     * <p>Fields are package-private rather than private so the sibling
     * {@link Cursor} wrapper can access them directly without an extra
     * method-call hop. External code can't see them either way — the
     * package seam keeps them out of consumers' hands.
     */
    final Token[] lookBuf;
    final int k;
    Pos prevEnd;
    int state;
    final ArrayDeque<Integer> ret = new ArrayDeque<>();

    Recovery recovery;
    /**
     * Lookahead one event so {@link #hasNext()} can answer without
     * consuming. {@link #next()} returns this if set, otherwise computes
     * one. Sentinel {@link #DONE} marks "no more events" so we don't
     * recompute exhaustion every call.
     */
    private Event ahead;
    private boolean aheadComputed;
    final ParserConfig cfg;

    public Parser(Lexer lex, int entry, ParserConfig cfg) {
        this.lex = lex;
        this.cfg = cfg;
        this.k = cfg.k;
        this.state = entry;
        this.lookBuf = new Token[cfg.k];
        this.prevEnd = new Pos(0, 0, 0);
    }

    /**
     * Pop the current lookahead token and shift the buffer up by one.
     * Slot k-1 is left null so pump-mode in nextEvent can refill it
     * (yielding one skip per call) before the next step reads lookahead.
     * Internal — callers wrap the result in the appropriate event subtype.
     */
    Token takeToken() {
        Token t = lookBuf[0];
        prevEnd = t.span.end;
        for (int i = 0; i < k - 1; i++) lookBuf[i] = lookBuf[i + 1];
        lookBuf[k - 1] = null;
        return t;
    }

    Event consume() { return new Event.Token(takeToken()); }

    Event errorHere(String msg) {
        return new Event.Error(new ParseError(msg, lookBuf[0].span));
    }

    /** True iff some lookahead slot still needs to be filled. Empty slots
     *  are always a contiguous suffix, so checking slot k-1 is the O(1)
     *  form of "any slot is null". */
    private boolean pumpPending() { return lookBuf[k - 1] == null; }

    /**
     * Produce the next event from the parse, or throw if the input is
     * fully consumed. The whole pull loop lives here — there's no
     * separate {@code nextEvent} indirection.
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
            if (pumpPending()) {
                Token t = lex.nextToken();
                if (cfg.isSkip.test(t.kind)) return new Event.Token(t);
                for (int i = 0; i < k; i++) {
                    if (lookBuf[i] == null) { lookBuf[i] = t; break; }
                }
                continue;
            }
            if (recovery != null) {
                int look0 = lookBuf[0].kind;
                boolean synced = look0 == ParserConfig.EOF_KIND
                    || containsKind(recovery.sync, look0);
                if (synced) {
                    boolean wasExpected = recovery.expected == look0;
                    recovery = null;
                    if (wasExpected) return consume();
                    continue;
                }
                return new Event.Garbage(takeToken());
            }
            // EOF gate. On the first visit with trailing input, raise
            // an error and arm a sync-empty recovery so the rest of
            // the input drains as Garbage events one per call. Once
            // recovery has eaten its way to EOF the lookahead pins at
            // EOF (the lexer keeps yielding it), so this is naturally
            // idempotent — subsequent visits throw NoSuchElementException.
            if (state == TERMINATED) {
                if (lookBuf[0].kind == ParserConfig.EOF_KIND) {
                    throw new NoSuchElementException();
                }
                Event ev = errorHere("expected end of input");
                recovery = new Recovery(new int[0], -1);
                return ev;
            }
            Event ev = cfg.step.apply(new Cursor(this));
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
