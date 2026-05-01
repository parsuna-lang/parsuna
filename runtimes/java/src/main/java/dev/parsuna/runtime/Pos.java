package dev.parsuna.runtime;

/**
 * Source position: byte offset plus 1-based line/column.
 *
 * <p>Mutable so the runtime can pool a few of these and rewrite them
 * each pull instead of allocating per-event. The fields are
 * package-private setters; external code reads via the accessor methods
 * and never sees a stale position because the parser only mutates an
 * event's positions while it's being constructed inside {@link Parser#next()}.
 */
public final class Pos {
    private int offset;
    private int line = 1;
    private int column = 1;

    public Pos() {}
    public Pos(int offset, int line, int column) {
        this.offset = offset; this.line = line; this.column = column;
    }

    /** 0-based byte offset into the source. */
    public int offset() { return offset; }
    /** 1-based line number. */
    public int line() { return line; }
    /** 1-based column, counted in Unicode codepoints within the current line. */
    public int column() { return column; }

    void set(int offset, int line, int column) {
        this.offset = offset; this.line = line; this.column = column;
    }
    void copyFrom(Pos other) {
        this.offset = other.offset; this.line = other.line; this.column = other.column;
    }

    /** Snapshot a stable copy. Use this to keep a position past the next
     *  {@link Parser#next()} call. */
    public Pos snapshot() { return new Pos(offset, line, column); }
}
