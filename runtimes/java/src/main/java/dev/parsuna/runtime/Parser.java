package dev.parsuna.runtime;

import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.NoSuchElementException;

/**
 * Pull-based parser. Obtain one via a generated `parseXxx` factory and
 * iterate it (or call {@link #nextEvent()} directly) to walk the parse as
 * a flat Event stream.
 */
public final class Parser implements Iterator<Event> {
    /** Sentinel state meaning "the parser has terminated". */
    public static final int TERMINATED = -1;

    private final Lexer lex;
    private final Token[] lookBuf;
    private Pos prevEnd;
    private int state;
    private final ArrayDeque<Integer> ret = new ArrayDeque<>();
    private final ArrayDeque<Event> queue = new ArrayDeque<>();
    private final ArrayDeque<Token> pendingSkips = new ArrayDeque<>();
    private boolean eofChecked = false;
    private Event ahead;
    private final ParserConfig cfg;

    public Parser(Lexer lex, int entry, ParserConfig cfg) {
        this.lex = lex;
        this.cfg = cfg;
        this.state = entry;
        this.lookBuf = new Token[cfg.k];
        for (int i = 0; i < cfg.k; i++) lookBuf[i] = pumpToken();
        this.prevEnd = lookBuf[0].span.start;
    }

    /** Peek at the i-th lookahead token (0 <= i < k). */
    public Token look(int i) { return lookBuf[i]; }

    /** Current state id. Read at the top of every driver iteration. */
    public int state() { return state; }

    /** Overwrite the current state. */
    public void setState(int s) { state = s; }

    /** Push a return state onto the call stack. */
    public void pushRet(int s) { ret.push(s); }

    /** Pop the top return state, or {@link #TERMINATED} if empty. */
    public int popRet() { return ret.isEmpty() ? TERMINATED : ret.pop(); }

    /** True iff no events are queued. */
    public boolean queueIsEmpty() { return queue.isEmpty(); }

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
        emit(new Event.Enter(rule, pos));
    }

    /** Emit an Exit event for the given rule-kind id. */
    public void exit(int rule) { emit(new Event.Exit(rule, prevEnd)); }

    /** Consume the current lookahead and emit it as a token event. */
    public void consume() {
        Token prev = look(0);
        prevEnd = prev.span.end;
        emit(new Event.Token(prev));
        advanceLook();
    }

    /** Consume a token of `kind`; on mismatch, error, recover to `sync`, retry once. */
    public void tryConsume(int kind, int[] sync, String name) {
        if (look(0).kind == kind) { consume(); return; }
        errorHere("expected " + name);
        recoverTo(sync);
        if (look(0).kind == kind) consume();
    }

    /** Consume tokens until the lookahead matches `sync` (or EOF). */
    public void recoverTo(int[] sync) {
        while (look(0).kind != ParserConfig.EOF_KIND && !containsKind(sync, look(0).kind)) {
            emit(new Event.Token(look(0)));
            advanceLook();
        }
    }

    /** Raise a recoverable error at the current lookahead. */
    public void errorHere(String msg) { emit(new Event.Error(new ParseError(msg, look(0).span))); }

    /** Return the next event, or null once input is fully consumed. */
    public Event nextEvent() {
        while (true) {
            if (!queue.isEmpty()) return queue.pollFirst();
            if (state == TERMINATED) {
                if (!eofChecked) {
                    eofChecked = true;
                    if (look(0).kind != ParserConfig.EOF_KIND) {
                        errorHere("expected end of input");
                        while (look(0).kind != ParserConfig.EOF_KIND) { emit(new Event.Token(look(0))); advanceLook(); }
                    }
                    flushSkipsBefore(look(0).span.end);
                    continue;
                }
                return null;
            }
            cfg.drive.accept(this);
        }
    }

    @Override public boolean hasNext() { if (ahead != null) return true; ahead = nextEvent(); return ahead != null; }
    @Override public Event next() {
        if (ahead != null) { Event e = ahead; ahead = null; return e; }
        Event e = nextEvent();
        if (e == null) throw new NoSuchElementException();
        return e;
    }

    private Token pumpToken() {
        while (true) {
            Token t = lex.nextToken();
            if (cfg.isSkip.test(t.kind)) { pendingSkips.addLast(t); continue; }
            return t;
        }
    }

    private void advanceLook() {
        for (int i = 0; i < cfg.k - 1; i++) lookBuf[i] = lookBuf[i + 1];
        lookBuf[cfg.k - 1] = pumpToken();
    }

    private void flushSkipsBefore(Pos pos) {
        while (!pendingSkips.isEmpty() && pendingSkips.peekFirst().span.end.offset <= pos.offset) {
            queue.addLast(new Event.Token(pendingSkips.pollFirst()));
        }
    }

    private void emit(Event ev) {
        Pos start = switch (ev) {
            case Event.Enter e -> e.pos();
            case Event.Exit e  -> e.pos();
            case Event.Token t -> t.token().span.start;
            case Event.Error x -> x.error().span.start;
        };
        flushSkipsBefore(start);
        queue.addLast(ev);
    }

    private static boolean containsKind(int[] set, int k) {
        for (int x : set) if (x == k) return true;
        return false;
    }
}
