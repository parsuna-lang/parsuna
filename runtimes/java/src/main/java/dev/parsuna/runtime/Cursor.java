package dev.parsuna.runtime;

/**
 * The handle that generated {@code step} bodies talk to the runtime
 * through. Wraps a {@link Parser} and re-exports just the operations
 * dispatch needs — lookahead access, return stack pushes, event
 * builders, recovery arming.
 *
 * <p>The emit methods ({@link #enter}, {@link #exit}, {@link #tryConsume},
 * {@link #errorHere}, {@link #consume}) are {@code void}: they write the
 * event payload into the parser's scalar event fields and the runtime
 * decides whether to yield based on those fields after step returns. No
 * Event record is allocated unless the caller asks for one via
 * {@link Parser#next()}.
 *
 * <p>External code can't construct a {@code Cursor} (the constructor is
 * package-private, and only the runtime's pull loop calls it), so the
 * only way one ever exists is inside a call to {@link DriveStep#step}
 * from the runtime's pull loop. That keeps the parser's internal state
 * from being poked at out of band.
 */
public final class Cursor {
    private final Parser p;

    Cursor(Parser p) { this.p = p; }

    /** Kind id of the i-th lookahead token (0 &lt;= i &lt; k). Generated
     *  step code only runs after pump-mode has filled every slot, so the
     *  slot is always populated. */
    public int look(int i) { return p.lookKind[i]; }

    /** Current state id. Read at the top of every step iteration. */
    public int state() { return p.state; }

    /** Overwrite the current state. */
    public void setState(int s) { p.state = s; }

    /** Push a return state onto the call stack. */
    public void pushRet(int s) {
        int top = p.retTop + 1;
        int[] stk = p.retStack;
        if (top >= stk.length) {
            int[] next = new int[stk.length * 2];
            System.arraycopy(stk, 0, next, 0, stk.length);
            p.retStack = next;
            stk = next;
        }
        stk[top] = s;
        p.retTop = top;
    }

    /** Pop the top return state, or {@link Parser#TERMINATED} if empty. */
    public int popRet() {
        int top = p.retTop;
        if (top < 0) return Parser.TERMINATED;
        p.retTop = top - 1;
        return p.retStack[top];
    }

    /** True iff the current lookahead matches any of the given prefixes. */
    public boolean matchesFirst(int[][] set) {
        outer: for (int[] seq : set) {
            for (int i = 0; i < seq.length; i++) {
                if (p.lookKind[i] != seq[i]) continue outer;
            }
            return true;
        }
        return false;
    }

    /** Stage an Enter event for the given rule-kind id. Records the
     *  rule's start position so a later Exit without any intervening
     *  tokens still yields a zero-width span at the expected place. */
    public void enter(int rule) {
        Parser pp = p;
        pp.evtKind = Parser.EVT_ENTER;
        pp.evtRule = rule;
        int sOff = pp.lookSOff[0];
        int sLine = pp.lookSLine[0];
        int sCol = pp.lookSCol[0];
        pp.evtPosOff = sOff; pp.evtPosLine = sLine; pp.evtPosCol = sCol;
        pp.prevEndOff = sOff; pp.prevEndLine = sLine; pp.prevEndCol = sCol;
    }

    /** Stage an Exit event for the given rule-kind id, positioned at the
     *  end of the last consumed token (or the rule's start for empty
     *  rules). */
    public void exit(int rule) {
        Parser pp = p;
        pp.evtKind = Parser.EVT_EXIT;
        pp.evtRule = rule;
        pp.evtPosOff = pp.prevEndOff;
        pp.evtPosLine = pp.prevEndLine;
        pp.evtPosCol = pp.prevEndCol;
    }

    /** Consume the current lookahead token and stage it as a TOKEN
     *  event. Used on tryConsume's success path and on recovery's
     *  "synced-to-expected" path — both yield legitimate parse data. */
    public void consume() {
        p.evtKind = Parser.EVT_TOKEN;
        p.evtFromLookahead = true;
    }

    /**
     * Try to consume a token of {@code kind}. On a hit stages a TOKEN
     * event. On a miss stages an ERROR event and arms recovery —
     * subsequent {@link Parser#nextKind()} calls yield GARBAGE events
     * until the lookahead lands on {@code sync} (when it does, the
     * matching token comes through as a normal TOKEN event).
     *
     * <p>Hot path is the consume-hit branch; the miss branch is split
     * into {@link #consumeMiss} so JIT keeps {@code tryConsume} small
     * enough to inline at every call site.
     */
    public void tryConsume(int kind, int[] sync, String name) {
        Parser pp = p;
        if (pp.lookKind[0] == kind) {
            pp.evtKind = Parser.EVT_TOKEN;
            pp.evtFromLookahead = true;
            return;
        }
        consumeMiss(pp, sync, kind, name);
    }

    private static void consumeMiss(Parser pp, int[] sync, int kind, String name) {
        pp.evtKind = Parser.EVT_ERROR;
        pp.evtErrorMsg = "expected " + name;
        pp.evtErrSOff = pp.lookSOff[0]; pp.evtErrSLine = pp.lookSLine[0]; pp.evtErrSCol = pp.lookSCol[0];
        pp.evtErrEOff = pp.lookEOff[0]; pp.evtErrELine = pp.lookELine[0]; pp.evtErrECol = pp.lookECol[0];
        pp.recovery = new Parser.Recovery(sync, kind);
    }

    /**
     * Arm recovery without an expected kind. Called from a dispatch
     * error leaf: the surrounding {@code cur} is already pointing at
     * the post-recovery state, and the {@link #errorHere} the caller
     * pairs with this call makes step yield so recovery-mode can take
     * over.
     */
    public void recoverTo(int[] sync) {
        p.recovery = new Parser.Recovery(sync, -1);
    }

    /** Stage a recoverable error event at the current lookahead. */
    public void errorHere(String msg) {
        Parser pp = p;
        pp.evtKind = Parser.EVT_ERROR;
        pp.evtErrorMsg = msg;
        pp.evtErrSOff = pp.lookSOff[0]; pp.evtErrSLine = pp.lookSLine[0]; pp.evtErrSCol = pp.lookSCol[0];
        pp.evtErrEOff = pp.lookEOff[0]; pp.evtErrELine = pp.lookELine[0]; pp.evtErrECol = pp.lookECol[0];
    }
}
