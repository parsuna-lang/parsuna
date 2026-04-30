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
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    /**
     * In-flight error recovery. Set by {@link #tryConsume(int, int[], String)}'s
     * slow path and by {@link #recoverTo(int[])}; cleared once the lookahead
     * lands on a sync token (or EOF). Each call to {@link #nextEvent()}
     * yields exactly one garbage token (as {@link Event.Garbage}) before
     * looping, so a long run of unexpected input doesn't pile up.
     */
    private static final class Recovery {
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
     * are filled, so {@link #look(int)} can return the value unconditionally.
     *
     * <p>Empty slots are always a contiguous suffix — consume parks the
     * new null at index k-1 and pump fills the leftmost empty slot.
     */
    private final Token[] lookBuf;
    private final int k;
    private Pos prevEnd;
    private int state;
    private final ArrayDeque<Integer> ret = new ArrayDeque<>();

    private Recovery recovery;
    private boolean eofChecked;
    private Event ahead;
    private final ParserConfig cfg;

    public Parser(Lexer lex, int entry, ParserConfig cfg) {
        this.lex = lex;
        this.cfg = cfg;
        this.k = cfg.k;
        this.state = entry;
        this.lookBuf = new Token[cfg.k];
        this.prevEnd = new Pos(0, 0, 0);
    }

    /**
     * Peek at the i-th lookahead token (0 &lt;= i &lt; k). Generated step
     * code only runs after pump-mode has filled every slot, so this never
     * sees a null.
     */
    public Token look(int i) { return lookBuf[i]; }

    /** Current state id. Read at the top of every step iteration. */
    public int state() { return state; }

    /** Overwrite the current state. */
    public void setState(int s) { state = s; }

    /** Push a return state onto the call stack. */
    public void pushRet(int s) { ret.push(s); }

    /** Pop the top return state, or {@link #TERMINATED} if empty. */
    public int popRet() { return ret.isEmpty() ? TERMINATED : ret.pop(); }

    /** True iff the current lookahead matches any of the given prefixes. */
    public boolean matchesFirst(int[][] set) {
        outer: for (int[] seq : set) {
            for (int i = 0; i < seq.length; i++) {
                if (look(i).kind != seq[i]) continue outer;
            }
            return true;
        }
        return false;
    }

    /** Build an Enter event for the given rule-kind id. Records the
     *  rule's start position so a later Exit without any intervening
     *  tokens still yields a zero-width span at the expected place. */
    public Event enter(int rule) {
        Pos pos = lookBuf[0].span.start;
        prevEnd = pos;
        return new Event.Enter(rule, pos);
    }

    /** Build an Exit event for the given rule-kind id, positioned at the
     *  end of the last consumed token (or the rule's start for empty
     *  rules). */
    public Event exit(int rule) { return new Event.Exit(rule, prevEnd); }

    /**
     * Pop the current lookahead token and shift the buffer up by one.
     * Slot k-1 is left null so pump-mode in nextEvent can refill it
     * (yielding one skip per call) before the next step reads lookahead.
     * Internal — callers wrap the result in the appropriate event subtype.
     */
    private Token takeToken() {
        Token t = lookBuf[0];
        prevEnd = t.span.end;
        for (int i = 0; i < k - 1; i++) lookBuf[i] = lookBuf[i + 1];
        lookBuf[k - 1] = null;
        return t;
    }

    /** Consume the current lookahead token and return it as an
     *  {@link Event.Token}. Used on tryConsume's success path and on
     *  recovery's "synced-to-expected" path — both yield legitimate
     *  parse data. */
    public Event consume() { return new Event.Token(takeToken()); }

    /**
     * Try to consume a token of {@code kind}. On a hit returns an
     * {@link Event.Token}. On a miss returns an {@link Event.Error} and
     * arms recovery — subsequent {@link #nextEvent()} calls yield
     * {@link Event.Garbage} events until the lookahead lands on
     * {@code sync} (when it does, the matching token comes through as
     * a normal {@link Event.Token}).
     */
    public Event tryConsume(int kind, int[] sync, String name) {
        if (look(0).kind == kind) return consume();
        Event ev = errorHere("expected " + name);
        recovery = new Recovery(sync, kind);
        return ev;
    }

    /**
     * Arm recovery without an expected kind. Called from a dispatch
     * error leaf: the surrounding {@code cur} is already pointing at
     * the post-recovery state, and the {@link Event.Error} the caller
     * pairs with this call makes step yield so recovery-mode can take
     * over.
     */
    public void recoverTo(int[] sync) {
        recovery = new Recovery(sync, -1);
    }

    /** Build a recoverable error event at the current lookahead. */
    public Event errorHere(String msg) {
        return new Event.Error(new ParseError(msg, look(0).span));
    }

    /** True iff some lookahead slot still needs to be filled. Empty slots
     *  are always a contiguous suffix, so checking slot k-1 is the O(1)
     *  form of "any slot is null". */
    private boolean pumpPending() { return lookBuf[k - 1] == null; }

    /**
     * Produce the next event from the parse, or {@code null} once the
     * entire input has been consumed.
     *
     * <p>The loop runs three modes — pump (refill lookahead, possibly
     * yielding a skip), recovery (one Garbage / synced-Token event per
     * call), and drive (one step call). Each mode yields at most one
     * event before the loop runs again.
     */
    public Event nextEvent() {
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
            if (state == TERMINATED) {
                if (!eofChecked) {
                    eofChecked = true;
                    if (look(0).kind != ParserConfig.EOF_KIND) {
                        Event ev = errorHere("expected end of input");
                        recovery = new Recovery(new int[0], -1);
                        return ev;
                    }
                    continue;
                }
                return null;
            }
            Event ev = cfg.step.apply(this);
            if (ev != null) return ev;
        }
    }

    @Override public boolean hasNext() {
        if (ahead != null) return true;
        ahead = nextEvent();
        return ahead != null;
    }

    @Override public Event next() {
        if (ahead != null) { Event e = ahead; ahead = null; return e; }
        Event e = nextEvent();
        if (e == null) throw new NoSuchElementException();
        return e;
    }

    private static boolean containsKind(int[] set, int k) {
        for (int x : set) if (x == k) return true;
        return false;
    }
}
