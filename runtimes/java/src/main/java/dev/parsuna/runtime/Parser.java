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
 * grammar's LL(k)), a return stack, and a fixed-size event queue. Each
 * call to {@link #nextEvent()} drains a pending event, pumps one lex
 * token, advances recovery by one step, or — when none of those have
 * anything to do — invokes the generated drive to execute the next state
 * body.
 *
 * <p>Skip handling is driven by pump-mode rather than a side queue: when
 * the lexer hands the runtime a skip token it lands directly in the event
 * queue, ahead of whatever structural event the next drive call will
 * produce. The lookahead refills one lex token at a time (yielding
 * between each), so a long comment run can't grow the queue past
 * {@code queueCap} — at any moment the queue holds either a single pump
 * push waiting to be drained, a single recovery push, or the structural
 * burst from one drive body.
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    /**
     * In-flight error recovery. Set by {@link #expect(int, int[], String)}'s
     * slow path and by {@link #recoverTo(int[])}; cleared once the lookahead
     * lands on a sync token (or EOF). Each call to {@link #nextEvent()}
     * drains exactly one garbage token before yielding, so a long run of
     * unexpected input doesn't pile up in the queue.
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
    private final Token[] lookBuf;
    private final int k;
    private Pos prevEnd;
    private int state;
    private final ArrayDeque<Integer> ret = new ArrayDeque<>();

    // Fixed-size ring buffer for events. Sized at construction from
    // ParserConfig.queueCap (the longest emit burst the grammar can produce
    // in any single state body). No growth, no allocation per push.
    private final Event[] queue;
    private final int queueCap;
    private int queueHead;
    private int queueLen;

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
        this.queueCap = cfg.queueCap;
        this.queue = new Event[cfg.queueCap];
        this.queueHead = 0;
        this.queueLen = 0;
        this.prevEnd = new Pos(0, 0, 0);
    }

    /**
     * Peek at the i-th lookahead token (0 &lt;= i &lt; k). Generated drive
     * code only runs after pump-mode has filled every slot, so this never
     * sees a null.
     */
    public Token look(int i) { return lookBuf[i]; }

    /** Current state id. Read at the top of every drive iteration. */
    public int state() { return state; }

    /** Overwrite the current state. */
    public void setState(int s) { state = s; }

    /** Push a return state onto the call stack. */
    public void pushRet(int s) { ret.push(s); }

    /** Pop the top return state, or {@link #TERMINATED} if empty. */
    public int popRet() { return ret.isEmpty() ? TERMINATED : ret.pop(); }

    /** True iff no events are queued. */
    public boolean queueIsEmpty() { return queueLen == 0; }

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

    /** Emit an Enter event for the given rule-kind id. */
    public void enter(int rule) {
        Pos pos = lookBuf[0].span.start;
        prevEnd = pos;
        pushEvent(new Event.Enter(rule, pos));
    }

    /** Emit an Exit event for the given rule-kind id. */
    public void exit(int rule) { pushEvent(new Event.Exit(rule, prevEnd)); }

    /**
     * Append an event to the output queue. Skip-token interleaving used to
     * happen here (a side queue flushed before each emit); pump-mode now
     * drains skips one at a time between drive calls, so this is a plain
     * push that respects the queue cap.
     */
    public void emit(Event ev) { pushEvent(ev); }

    /**
     * Consume the current lookahead token, emit it, and shift the buffer up
     * by one. Slot K-1 is left null so pump-mode can refill it (yielding
     * one skip per call) before the next drive reads lookahead.
     */
    public void consume() {
        Token t = lookBuf[0];
        prevEnd = t.span.end;
        for (int i = 0; i < k - 1; i++) lookBuf[i] = lookBuf[i + 1];
        lookBuf[k - 1] = null;
        pushEvent(new Event.Token(t));
    }

    /**
     * Try to consume a token of {@code kind}; on mismatch, emit an error
     * and arm recovery. Returns immediately on the slow path — drive will
     * see the queued error and yield, then nextEvent's recovery-mode
     * advances one garbage token per call until the sync set is hit.
     */
    public void tryConsume(int kind, int[] sync, String name) {
        if (look(0).kind == kind) { consume(); return; }
        errorHere("expected " + name);
        recovery = new Recovery(sync, kind);
    }

    /**
     * Arm recovery without an expected kind. Called from the dispatch
     * error leaf: the surrounding {@code cur} was already set to the
     * post-recovery state by codegen, and the queued Error event makes
     * drive yield immediately so recovery-mode can take over.
     */
    public void recoverTo(int[] sync) {
        recovery = new Recovery(sync, -1);
    }

    /** Raise a recoverable error at the current lookahead. */
    public void errorHere(String msg) {
        pushEvent(new Event.Error(new ParseError(msg, look(0).span)));
    }

    /**
     * Produce the next event from the parse, or {@code null} once the
     * entire input has been consumed.
     *
     * <p>The loop layers four kinds of progress, ordered so the consumer
     * always sees the soonest-available event:
     *
     * <ol>
     *   <li>Drain a queued event.</li>
     *   <li>Pump one lex token (filling lookahead, or queuing a skip).</li>
     *   <li>Advance recovery by one step.</li>
     *   <li>Run the generated dispatch for one drive call.</li>
     * </ol>
     */
    public Event nextEvent() {
        while (true) {
            if (queueLen > 0) return popEvent();
            if (pumpPending()) { pumpOne(); continue; }
            if (recovery != null) { recoverOne(); continue; }
            if (state == TERMINATED) {
                if (!eofChecked) {
                    eofChecked = true;
                    if (look(0).kind != ParserConfig.EOF_KIND) {
                        // Trailing input past the entry rule. Synthesize an
                        // error and use recovery-mode (with an empty sync
                        // set) to drain the rest as Token events one yield
                        // at a time.
                        errorHere("expected end of input");
                        recovery = new Recovery(new int[0], -1);
                        continue;
                    }
                    continue;
                }
                return null;
            }
            cfg.drive.accept(this);
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

    /** True iff some lookahead slot still needs to be filled. */
    private boolean pumpPending() {
        for (int i = 0; i < k; i++) if (lookBuf[i] == null) return true;
        return false;
    }

    /**
     * Lex one token. If it's a skip, push it directly onto the event queue
     * and leave pump-mode armed for another call. If it's a structural
     * token, fill the leftmost empty lookahead slot.
     */
    private void pumpOne() {
        Token t = lex.nextToken();
        if (cfg.isSkip.test(t.kind)) {
            pushEvent(new Event.Token(t));
            return;
        }
        for (int i = 0; i < k; i++) {
            if (lookBuf[i] == null) { lookBuf[i] = t; return; }
        }
        throw new IllegalStateException("pumpOne called with all slots filled");
    }

    /**
     * Advance recovery by one step. Either consume one garbage token (one
     * Token push, drive yield) or — if the lookahead is in the sync set /
     * EOF — finalise by clearing recovery and (when a matching expected
     * was set) swallowing the synced-to token.
     */
    private void recoverOne() {
        Recovery rec = recovery;
        int look0 = lookBuf[0].kind;
        if (look0 == ParserConfig.EOF_KIND) {
            recovery = null;
            return;
        }
        if (containsKind(rec.sync, look0)) {
            boolean wasExpected = rec.expected == look0;
            recovery = null;
            if (wasExpected) consume();
            return;
        }
        consume();
    }

    private void pushEvent(Event ev) {
        int idx = (queueHead + queueLen) % queueCap;
        queue[idx] = ev;
        queueLen++;
    }

    private Event popEvent() {
        Event ev = queue[queueHead];
        queue[queueHead] = null;
        queueHead = (queueHead + 1) % queueCap;
        queueLen--;
        return ev;
    }

    private static boolean containsKind(int[] set, int k) {
        for (int x : set) if (x == k) return true;
        return false;
    }
}
