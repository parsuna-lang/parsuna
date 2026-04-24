package dev.parsuna.runtime;

/** Half-open span [start, end) over the source. */
public final class Span {
    public final Pos start;
    public final Pos end;
    public Span(Pos start, Pos end) {
        this.start = start;
        this.end = end;
    }
    public static Span point(Pos p) { return new Span(p, p); }
}
