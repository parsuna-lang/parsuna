package dev.parsuna.runtime;

/**
 * Half-open span [start, end) over the source.
 *
 * <p>Mutable so the runtime can pool spans alongside {@link Token}s
 * and {@link Event}s. {@link #start()} and {@link #end()} return
 * stable references to two pooled {@link Pos} instances; their
 * contents change on each pull. Snapshot via {@link #snapshot()} if
 * you need to keep the span around.
 */
public final class Span {
    private final Pos start = new Pos();
    private final Pos end = new Pos();

    public Span() {}
    public Span(Pos start, Pos end) {
        this.start.copyFrom(start);
        this.end.copyFrom(end);
    }

    public Pos start() { return start; }
    public Pos end() { return end; }

    void set(int sOff, int sLine, int sCol, int eOff, int eLine, int eCol) {
        start.set(sOff, sLine, sCol);
        end.set(eOff, eLine, eCol);
    }
    void copyFrom(Span other) {
        start.copyFrom(other.start);
        end.copyFrom(other.end);
    }

    /** Convenience: build a fresh zero-width span at a single point.
     *  Allocates — only used by callers building synthetic spans. */
    public static Span point(Pos p) { return new Span(p, p); }

    /** Snapshot a stable copy. Use this to keep the span past the next
     *  {@link Parser#next()} call. */
    public Span snapshot() {
        Span s = new Span();
        s.copyFrom(this);
        return s;
    }
}
