package dev.parsuna.runtime;

/**
 * The handle that generated {@code step} bodies talk to the runtime
 * through. Wraps a {@link Parser} and re-exports just the operations
 * dispatch needs — lookahead access, return stack pushes, event
 * builders, recovery arming.
 *
 * <p>External code can't construct a {@code Cursor} (the constructor is
 * package-private, and only {@link Parser#nextEvent()} calls it), so the
 * only way one ever exists is inside a call to {@link ParserConfig#step}
 * from the runtime's pull loop. That keeps the parser's internal state
 * from being poked at out of band.
 */
public final class Cursor {
    private final Parser p;

    Cursor(Parser p) { this.p = p; }

    /**
     * Peek at the i-th lookahead token (0 &lt;= i &lt; k). Generated
     * step code only runs after pump-mode has filled every slot, so
     * this never sees a null.
     */
    public Token look(int i) { return p.lookBuf[i]; }

    /** Current state id. Read at the top of every step iteration. */
    public int state() { return p.state; }

    /** Overwrite the current state. */
    public void setState(int s) { p.state = s; }

    /** Push a return state onto the call stack. */
    public void pushRet(int s) { p.ret.push(s); }

    /** Pop the top return state, or {@link Parser#TERMINATED} if empty. */
    public int popRet() { return p.ret.isEmpty() ? Parser.TERMINATED : p.ret.pop(); }

    /** True iff the current lookahead matches any of the given prefixes. */
    public boolean matchesFirst(int[][] set) {
        outer: for (int[] seq : set) {
            for (int i = 0; i < seq.length; i++) {
                if (p.lookBuf[i].kind != seq[i]) continue outer;
            }
            return true;
        }
        return false;
    }

    /** Build an Enter event for the given rule-kind id. Records the
     *  rule's start position so a later Exit without any intervening
     *  tokens still yields a zero-width span at the expected place. */
    public Event enter(int rule) {
        Pos pos = p.lookBuf[0].span.start;
        p.prevEnd = pos;
        return new Event.Enter(rule, pos);
    }

    /** Build an Exit event for the given rule-kind id, positioned at the
     *  end of the last consumed token (or the rule's start for empty
     *  rules). */
    public Event exit(int rule) { return new Event.Exit(rule, p.prevEnd); }

    /** Consume the current lookahead token and return it as an
     *  {@link Event.Token}. Used on tryConsume's success path and on
     *  recovery's "synced-to-expected" path — both yield legitimate
     *  parse data. */
    public Event consume() { return p.consume(); }

    /**
     * Try to consume a token of {@code kind}. On a hit returns an
     * {@link Event.Token}. On a miss returns an {@link Event.Error}
     * and arms recovery — subsequent {@link Parser#nextEvent()} calls
     * yield {@link Event.Garbage} events until the lookahead lands on
     * {@code sync} (when it does, the matching token comes through as
     * a normal {@link Event.Token}).
     */
    public Event tryConsume(int kind, int[] sync, String name) {
        if (p.lookBuf[0].kind == kind) return p.consume();
        Event ev = p.errorHere("expected " + name);
        p.recovery = new Parser.Recovery(sync, kind);
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
        p.recovery = new Parser.Recovery(sync, -1);
    }

    /** Build a recoverable error event at the current lookahead. */
    public Event errorHere(String msg) { return p.errorHere(msg); }
}
