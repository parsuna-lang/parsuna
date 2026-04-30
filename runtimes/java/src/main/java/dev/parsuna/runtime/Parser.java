package dev.parsuna.runtime;

import java.util.Iterator;
import java.util.NoSuchElementException;

/**
 * Pull-based, recoverable parser. Obtain one via a generated {@code parseXxx}
 * factory and iterate it (or call {@link #next()} directly) to walk the
 * parse as a flat {@link Event} stream.
 *
 * <p>The parser keeps a ring of {@code k} lookahead slots (sized to the
 * grammar's LL(k)) and a return stack. Each call to {@link #nextKind()}
 * runs in one of three modes — pump (refill lookahead, possibly yielding
 * a skip), recovery (one Garbage / synced-Token event), or drive (one
 * call to the generated {@code step}) — and yields at most one event
 * before looping again.
 *
 * <p>Two iteration shapes:
 * <ul>
 *   <li>{@link #nextKind()} returns an {@code int} kind code
 *       ({@code EVT_*}) and exposes the event's payload via accessor
 *       methods ({@link #tokenKind()}, {@link #tokenByteLen()},
 *       {@link #tokenText()}, {@link #ruleKind()}, {@link #errorMessage()},
 *       …). It allocates only when the caller asks for a derived value
 *       (e.g. {@code tokenText()}). This is the path StAX-style consumers
 *       use to chase per-event throughput.</li>
 *   <li>{@link #next()} (the {@link Iterator} method) wraps the same
 *       state into the sealed {@link Event} record hierarchy. Convenient
 *       for pattern matching; allocates one Event record per call.</li>
 * </ul>
 *
 * <p>{@code Parser}'s only public surface is the constructors,
 * {@link #next()} / {@link #hasNext()} (Iterator), {@link #nextKind()}
 * + accessors, and the runtime configuration knobs. The runtime hooks
 * generated code calls into (lookahead access, return stack, event
 * builders, recovery arming) live on {@link Cursor} instead, and the
 * runtime owns a single shared {@code Cursor} that's only handed to
 * {@link DriveStep#step} from inside the pull loop — so external
 * callers can't poke at parser internals out of band.
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    /** {@link #nextKind()} returns this when there are no more events. */
    public static final int EVT_END     = -1;
    public static final int EVT_ENTER   = 0;
    public static final int EVT_EXIT    = 1;
    public static final int EVT_TOKEN   = 2;
    public static final int EVT_GARBAGE = 3;
    public static final int EVT_ERROR   = 4;

    /**
     * In-flight error recovery. Set by {@link Cursor#tryConsume(int, int[], String)}'s
     * slow path and by {@link Cursor#recoverTo(int[])}; cleared once the
     * lookahead lands on a sync token (or EOF). Each call to
     * {@link #nextKind()} yields exactly one garbage token before
     * looping, so a long run of unexpected input doesn't pile up.
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

    final Lexer lex;
    final int k;

    // -----------------------------------------------------------------
    // Lookahead — parallel arrays so the runtime never has to alloc a
    // {@link Token} just to refill a slot. Only when the user pulls the
    // current event as an {@link Event.Token} record do we materialize
    // a Token object out of these.
    //
    // Slot semantics match the original ring: slot 0 is the current
    // lookahead; slot k-1 is the leading edge that pump-mode refills.
    // After takeToken, slots 0..k-2 are populated and slot k-1 is empty.
    // -----------------------------------------------------------------
    final int[] lookKind;
    final byte[][] lookData;
    final int[] lookDataOff;
    final int[] lookDataLen;
    final int[] lookSOff, lookSLine, lookSCol;
    final int[] lookEOff, lookELine, lookECol;
    /** Number of contiguous filled slots starting at index 0. Pump fills
     *  slot {@code lookFilled} and increments; takeToken shifts and
     *  decrements. */
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

    // -----------------------------------------------------------------
    // Scalar event state — written by Cursor's emit methods (or by the
    // pull loop directly for skip / garbage / synthetic-EOF events) and
    // read back by {@link #next()} / accessors. The runtime never
    // allocates an Event record on the hot path; only the convenience
    // {@link #next()} wrapper does, on demand.
    // -----------------------------------------------------------------

    int evtKind = EVT_END;
    int evtRule;
    /** Position payload for ENTER/EXIT (start of the rule's first child
     *  for ENTER; end of the last consumed token for EXIT). */
    int evtPosOff, evtPosLine, evtPosCol;
    /** Token payload for TOKEN/GARBAGE when {@link #evtFromLookahead} is
     *  {@code false} — populated by skip-token yields where the token
     *  never sits in lookahead. */
    int evtTokenKind;
    byte[] evtTokenData;
    int evtTokenDataOff, evtTokenDataLen;
    int evtTokenSOff, evtTokenSLine, evtTokenSCol;
    int evtTokenEOff, evtTokenELine, evtTokenECol;
    /** {@code true} when the current TOKEN/GARBAGE event's data is still
     *  sitting in {@link #lookKind}{@code [0]} (and friends) instead of
     *  the {@code evtToken*} fields. We defer the lookahead shift to the
     *  next {@link #nextKind()} call so the common drive-and-consume
     *  path doesn't have to copy 10 fields off the lookahead per token —
     *  accessors just read straight off slot 0. */
    boolean evtFromLookahead;
    /** Error payload for ERROR (message + span). */
    String evtErrorMsg;
    int evtErrSOff, evtErrSLine, evtErrSCol;
    int evtErrEOff, evtErrELine, evtErrECol;

    final ParserConfig cfg;
    final ParserOptions opts;

    /** Single cursor instance reused across every drive call — saves a
     *  per-step allocation that would otherwise be the hottest small
     *  alloc in the parse loop. */
    private final Cursor cursor = new Cursor(this);

    /**
     * Lookahead one event so {@link #hasNext()} can answer without
     * advancing. Holds the kind code; {@code aheadComputed} guards
     * staleness. This is the scalar mirror of the old {@code ahead}
     * Event field — no record allocation just to peek.
     */
    private int aheadKind = EVT_END;
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
        this.lookKind    = new int[cfg.k];
        this.lookData    = new byte[cfg.k][];
        this.lookDataOff = new int[cfg.k];
        this.lookDataLen = new int[cfg.k];
        this.lookSOff    = new int[cfg.k];
        this.lookSLine   = new int[cfg.k];
        this.lookSCol    = new int[cfg.k];
        this.lookEOff    = new int[cfg.k];
        this.lookELine   = new int[cfg.k];
        this.lookECol    = new int[cfg.k];
    }

    /**
     * Pop the head of the lookahead and shift the rest up by one slot.
     * Internal — callers wrap the resulting position into an event via
     * the appropriate cursor / pull-loop builder.
     */
    void shiftLookahead() {
        prevEndOff = lookEOff[0];
        prevEndLine = lookELine[0];
        prevEndCol = lookECol[0];
        for (int i = 0; i < lookFilled - 1; i++) {
            lookKind[i]    = lookKind[i + 1];
            lookData[i]    = lookData[i + 1];
            lookDataOff[i] = lookDataOff[i + 1];
            lookDataLen[i] = lookDataLen[i + 1];
            lookSOff[i] = lookSOff[i + 1];
            lookSLine[i] = lookSLine[i + 1];
            lookSCol[i] = lookSCol[i + 1];
            lookEOff[i] = lookEOff[i + 1];
            lookELine[i] = lookELine[i + 1];
            lookECol[i] = lookECol[i + 1];
        }
        lookFilled--;
    }

    /** Stage a TOKEN / GARBAGE event whose data is still in lookahead
     *  slot 0. The shift is deferred to the next {@link #nextKind()}
     *  call so we don't pay a 10-field copy per token in the hot
     *  drive-and-consume loop — accessors read straight off
     *  {@link #lookKind}{@code [0]}. */
    void emitTokenFromLookahead(int evtKind) {
        this.evtKind = evtKind;
        evtFromLookahead = true;
    }

    /** Push the lexer's last-matched token directly into the event
     *  fields without storing it in lookahead — used for skip-token
     *  yields where the token never sits in lookahead so the
     *  deferred-shift trick doesn't apply. Skips don't update
     *  {@link #prevEndOff} because they're not "consumed" in the
     *  grammar sense; the next consumed token will. */
    void emitTokenFromLexer(int evtKind) {
        this.evtKind = evtKind;
        evtFromLookahead = false;
        evtTokenKind    = lex.lastKind;
        evtTokenData    = lex.lastData;
        evtTokenDataOff = lex.lastDataOff;
        evtTokenDataLen = lex.lastDataLen;
        evtTokenSOff = lex.lastSOff; evtTokenSLine = lex.lastSLine; evtTokenSCol = lex.lastSCol;
        evtTokenEOff = lex.lastEOff; evtTokenELine = lex.lastELine; evtTokenECol = lex.lastECol;
    }

    /** Stage a synthetic ERROR ("expected end of input") whose span is
     *  pinned to the current lookahead head. Used by the EOF gate in the
     *  pull loop. */
    void emitErrorAtLook(String msg) {
        evtKind = EVT_ERROR;
        evtErrorMsg = msg;
        evtErrSOff = lookSOff[0]; evtErrSLine = lookSLine[0]; evtErrSCol = lookSCol[0];
        evtErrEOff = lookEOff[0]; evtErrELine = lookELine[0]; evtErrECol = lookECol[0];
    }

    /** True iff some lookahead slot still needs to be filled. */
    private boolean pumpPending() { return lookFilled < k; }

    /**
     * Drive one event. The whole pull loop lives here — there's no
     * separate {@code nextEvent} indirection. The event payload lands
     * in the parser's scalar event fields; the returned int identifies
     * which payload is valid.
     *
     * <p>The loop runs three modes — pump (refill lookahead, possibly
     * yielding a skip), recovery (one Garbage / synced-Token event per
     * call), and drive (one step call). Each mode yields at most one
     * event before the loop runs again. Returns {@link #EVT_END} when
     * the input is fully consumed.
     */
    public int nextKind() {
        if (evtFromLookahead) {
            shiftLookahead();
            evtFromLookahead = false;
        }
        if (aheadComputed) {
            aheadComputed = false;
            int ak = aheadKind;
            aheadKind = EVT_END;
            return ak;
        }
        while (true) {
            if (pumpPending()) {
                lex.nextToken();
                int kind = lex.lastKind;
                cfg.applyActions.apply(kind, lex);
                if (cfg.isSkip.test(kind)) {
                    if (!opts.dropSkips) {
                        emitTokenFromLexer(EVT_TOKEN);
                        return EVT_TOKEN;
                    }
                    continue;
                }
                int slot = lookFilled++;
                lookKind[slot]    = kind;
                lookData[slot]    = lex.lastData;
                lookDataOff[slot] = lex.lastDataOff;
                lookDataLen[slot] = lex.lastDataLen;
                lookSOff[slot] = lex.lastSOff; lookSLine[slot] = lex.lastSLine; lookSCol[slot] = lex.lastSCol;
                lookEOff[slot] = lex.lastEOff; lookELine[slot] = lex.lastELine; lookECol[slot] = lex.lastECol;
                continue;
            }
            if (recovery != null) {
                int look0 = lookKind[0];
                boolean synced = look0 == ParserConfig.EOF_KIND
                    || containsKind(recovery.sync, look0);
                if (synced) {
                    boolean wasExpected = recovery.expected == look0;
                    recovery = null;
                    if (wasExpected) {
                        emitTokenFromLookahead(EVT_TOKEN);
                        return EVT_TOKEN;
                    }
                    continue;
                }
                emitTokenFromLookahead(EVT_GARBAGE);
                return EVT_GARBAGE;
            }
            // EOF gate. On the first visit with trailing input, raise
            // an error and arm a sync-empty recovery so the rest of
            // the input drains as Garbage events one per call. Once
            // recovery has eaten its way to EOF the lookahead pins at
            // EOF (the lexer keeps yielding it), so this is naturally
            // idempotent — subsequent visits return EVT_END.
            if (state == TERMINATED) {
                if (lookKind[0] == ParserConfig.EOF_KIND) {
                    return EVT_END;
                }
                emitErrorAtLook("expected end of input");
                recovery = new Recovery(new int[0], -1);
                return EVT_ERROR;
            }
            evtKind = EVT_END;
            cfg.step.step(cursor);
            if (evtKind != EVT_END) return evtKind;
        }
    }

    @Override public Event next() {
        int k = aheadComputed ? consumeAhead() : nextKind();
        return buildCurrentEvent(k);
    }

    private int consumeAhead() {
        aheadComputed = false;
        int ak = aheadKind;
        aheadKind = EVT_END;
        return ak;
    }

    /** Build an {@link Event} record from the current scalar event
     *  fields. Allocates — call {@link #nextKind()} + accessors instead
     *  for hot-path iteration. */
    private Event buildCurrentEvent(int kind) {
        switch (kind) {
            case EVT_ENTER:
                return new Event.Enter(evtRule, new Pos(evtPosOff, evtPosLine, evtPosCol));
            case EVT_EXIT:
                return new Event.Exit(evtRule, new Pos(evtPosOff, evtPosLine, evtPosCol));
            case EVT_TOKEN:
                return new Event.Token(materializeEvtToken());
            case EVT_GARBAGE:
                return new Event.Garbage(materializeEvtToken());
            case EVT_ERROR:
                return new Event.Error(new ParseError(
                    evtErrorMsg,
                    new Span(
                        new Pos(evtErrSOff, evtErrSLine, evtErrSCol),
                        new Pos(evtErrEOff, evtErrELine, evtErrECol))));
            default:
                throw new NoSuchElementException();
        }
    }

    private Token materializeEvtToken() {
        if (evtFromLookahead) {
            return new Token(lookKind[0], lookData[0], lookDataOff[0], lookDataLen[0],
                lookSOff[0], lookSLine[0], lookSCol[0],
                lookEOff[0], lookELine[0], lookECol[0]);
        }
        return new Token(evtTokenKind, evtTokenData, evtTokenDataOff, evtTokenDataLen,
            evtTokenSOff, evtTokenSLine, evtTokenSCol,
            evtTokenEOff, evtTokenELine, evtTokenECol);
    }

    @Override public boolean hasNext() {
        if (aheadComputed) return aheadKind != EVT_END;
        int k = nextKind();
        aheadKind = k;
        aheadComputed = true;
        // The next nextKind() call will shift the lookahead before
        // re-running the pull loop. If the cached event's data still
        // lives in lookahead slot 0, we'd lose it — copy it into the
        // evt* fields now so accessors keep working.
        if (evtFromLookahead) {
            evtTokenKind    = lookKind[0];
            evtTokenData    = lookData[0];
            evtTokenDataOff = lookDataOff[0];
            evtTokenDataLen = lookDataLen[0];
            evtTokenSOff = lookSOff[0]; evtTokenSLine = lookSLine[0]; evtTokenSCol = lookSCol[0];
            evtTokenEOff = lookEOff[0]; evtTokenELine = lookELine[0]; evtTokenECol = lookECol[0];
            evtFromLookahead = false;
            shiftLookahead();
        }
        return k != EVT_END;
    }

    // -----------------------------------------------------------------
    // Low-allocation accessors — read scalar event fields directly. Only
    // valid for the most recently returned {@link #nextKind()} kind, and
    // only for the matching event variant.
    // -----------------------------------------------------------------

    /** Token-kind id for the current TOKEN / GARBAGE event. */
    public int tokenKind() {
        return evtFromLookahead ? lookKind[0] : evtTokenKind;
    }

    /** Byte length of the current token's text. No allocation, no UTF-8
     *  decoding — purely a field read. */
    public int tokenByteLen() {
        return evtFromLookahead ? lookDataLen[0] : evtTokenDataLen;
    }

    /** Decoded UTF-8 text for the current TOKEN / GARBAGE event.
     *  Allocates a {@link String}; cache-free, so repeated calls cost
     *  repeated allocations. */
    public String tokenText() {
        if (evtFromLookahead) {
            return new String(lookData[0], lookDataOff[0], lookDataLen[0],
                java.nio.charset.StandardCharsets.UTF_8);
        }
        return new String(evtTokenData, evtTokenDataOff, evtTokenDataLen,
            java.nio.charset.StandardCharsets.UTF_8);
    }

    /** Backing byte buffer for the current token. May be the lexer's
     *  read buffer (zero-copy byte[] mode) or a per-token private copy
     *  (InputStream mode). Bytes live in {@code [tokenDataOff,
     *  tokenDataOff + tokenByteLen)}. */
    public byte[] tokenData() {
        return evtFromLookahead ? lookData[0] : evtTokenData;
    }
    public int tokenDataOff() {
        return evtFromLookahead ? lookDataOff[0] : evtTokenDataOff;
    }

    /** Rule-kind id for the current ENTER / EXIT event. */
    public int ruleKind() { return evtRule; }

    /** Error message for the current ERROR event. */
    public String errorMessage() { return evtErrorMsg; }

    static boolean containsKind(int[] set, int k) {
        for (int x : set) if (x == k) return true;
        return false;
    }
}
