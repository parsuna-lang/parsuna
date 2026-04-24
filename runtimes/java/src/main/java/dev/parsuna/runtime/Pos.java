package dev.parsuna.runtime;

/** Source position: byte offset plus 1-based line/column. */
public final class Pos {
    /** 0-based byte offset into the source. */
    public final int offset;
    /** 1-based line number. */
    public final int line;
    /** 1-based column, counted in Unicode codepoints within the current line. */
    public final int column;
    public Pos(int offset, int line, int column) {
        this.offset = offset;
        this.line = line;
        this.column = column;
    }
}
