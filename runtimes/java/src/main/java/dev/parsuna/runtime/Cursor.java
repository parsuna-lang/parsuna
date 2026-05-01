package dev.parsuna.runtime;

/**
 * The handle that generated {@code step} bodies talk to the runtime
 * through. Wraps a {@link Parser} and re-exports just the operations
 * dispatch needs — lookahead access, return stack pushes, event
 * builders, recovery arming.
 *
 * <p>The emit methods ({@link #enter}, {@link #exit}, {@link #tryConsume},
 * {@link #errorHere}, {@link #consume}) all return the parser's pooled
 * {@link Event} instance for the relevant variant — the runtime never
 * allocates a fresh Event on the hot path.
 *
 * <p>External code can't construct a {@code Cursor} (the constructor is
 * package-private, and only the runtime's pull loop calls it), so the
 * only way one ever exists is inside a call to {@link DriveStep#step}.
 * That keeps the parser's internal state from being poked at out of band.
 */
public final class Cursor {
    private final Parser p;

    Cursor(Parser p) { this.p = p; }

    /** Kind id of the i-th lookahead token (0 &lt;= i &lt; k). Generated
     *  step code only runs after pump-mode has filled every slot, so
     *  the slot is always populated. */
    public int look(int i) { return p.lookBuf[i].kind(); }

    /** Current state id. Read at the top of every step iteration. */
    public int state() { return p.state; }

    /** Overwrite the current state. */
    public void setState(int s) { p.state = s; }

    /** Push a return state onto the call stack along with the
     *  current lex mode-stack depth, so recovery can unwind interior
     *  mode pushes back to the depth that was in place when this
     *  rule started. */
    public void pushRet(int s) {
        int top = p.retTop + 1;
        int[] stk = p.retStack;
        if (top >= stk.length) {
            int[] next = new int[stk.length * 2];
            System.arraycopy(stk, 0, next, 0, stk.length);
            p.retStack = next;
            stk = next;
            int[] nextDepth = new int[next.length];
            System.arraycopy(p.retModeDepth, 0, nextDepth, 0, p.retModeDepth.length);
            p.retModeDepth = nextDepth;
        }
        stk[top] = s;
        p.retModeDepth[top] = p.lex.modeDepth();
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
                if (p.lookBuf[i].kind() != seq[i]) continue outer;
            }
            return true;
        }
        return false;
    }

    /** Build an Enter event for the given rule-kind id. Records the
     *  rule's start position so a later Exit without any intervening
     *  tokens still yields a zero-width span at the expected place. */
    public Event enter(int rule) {
        Token look0 = p.lookBuf[0];
        Pos start = look0.span().start();
        int sOff = start.offset(), sLine = start.line(), sCol = start.column();
        p.prevEndOff = sOff;
        p.prevEndLine = sLine;
        p.prevEndCol = sCol;
        p.evtEnter.set(rule, sOff, sLine, sCol);
        return p.evtEnter;
    }

    /** Build an Exit event for the given rule-kind id, positioned at
     *  the end of the last consumed token (or the rule's start for
     *  empty rules). */
    public Event exit(int rule) {
        p.evtExit.set(rule, p.prevEndOff, p.prevEndLine, p.prevEndCol);
        return p.evtExit;
    }

    /** Consume the current lookahead token and return it as an
     *  {@link Event.Token}. */
    public Event consume() { return p.consume(); }

    /**
     * Try to consume a token of {@code kind}. On a hit returns an
     * {@link Event.Token}. On a miss returns an {@link Event.Error}
     * and arms recovery — subsequent {@link Parser#next()} calls
     * yield {@link Event.Garbage} events until the lookahead lands on
     * {@code sync} (when it does, the matching token comes through as
     * a normal {@link Event.Token}).
     *
     * <p>Hot path is the consume-hit branch; the miss branch is split
     * into {@link #consumeMiss} so JIT keeps {@code tryConsume} small
     * enough to inline at every call site.
     */
    public Event tryConsume(int kind, int[] sync, String name) {
        return tryConsumeLabeled(kind, sync, name, 0);
    }

    /**
     * {@link #tryConsume} variant that stamps the supplied {@code label}
     * on the consumed token's {@link Token#label()} on the success path.
     * Used by generated dispatch for {@code name:NAME} positions in the
     * grammar; the label travels through to the consumer's event stream
     * so they can identify the position by name without tracking
     * surrounding rule context. Pass {@code label = null} for unlabeled
     * positions.
     */
    public Event tryConsumeLabeled(int kind, int[] sync, String name, int label) {
        Parser pp = p;
        if (pp.lookBuf[0].kind() == kind) {
            // Stamp the label directly on the slot's token before
            // consume rotates it out — keeps the unlabeled hot path
            // branch-free (the field stays null from the lex-time
            // reset in Token.set).
            if (label != 0) {
                pp.lookBuf[0].label = label;
            }
            return pp.consume();
        }
        return consumeMiss(pp, sync, kind, name);
    }

    private static Event consumeMiss(Parser pp, int[] sync, int kind, String name) {
        Span s = pp.lookBuf[0].span();
        pp.evtError.error().set("expected " + name,
            s.start().offset(), s.start().line(), s.start().column(),
            s.end().offset(), s.end().line(), s.end().column());
        pp.recovery = new Parser.Recovery(sync, kind);
        unwindModes(pp);
        return pp.evtError;
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
        unwindModes(p);
    }

    /**
     * Unwind any interior mode pushes the now-erroring rule made.
     * The top of {@link Parser#retModeDepth} is the depth at the
     * moment the rule we're inside was entered; popping back to it
     * brings the lexer to the same context the surrounding caller
     * expects. With an empty stack (recovery in the entry rule) we
     * restore to depth 1 — the default mode.
     */
    private static void unwindModes(Parser pp) {
        int target = pp.retTop >= 0 ? pp.retModeDepth[pp.retTop] : 1;
        pp.lex.popModesTo(target);
    }

    /** Build a recoverable error event at the current lookahead. */
    public Event errorHere(String msg) { return p.errorHere(msg); }
}
