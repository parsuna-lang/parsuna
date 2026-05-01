package dev.parsuna.runtime;

/**
 * A recoverable parse or lex error with its source span.
 *
 * <p>Mutable so the runtime can pool one of these per parser and
 * rewrite it on each error event. Stable until the next
 * {@link Parser#next()} call.
 */
public final class ParseError {
    private String message;
    private final Span span = new Span();

    public ParseError() {}

    public ParseError(String message, Span span) {
        this.message = message;
        this.span.copyFrom(span);
    }

    public String message() { return message; }
    public Span span() { return span; }

    void set(String message, Span span) {
        this.message = message;
        this.span.copyFrom(span);
    }
    void set(String message,
             int sOff, int sLine, int sCol,
             int eOff, int eLine, int eCol) {
        this.message = message;
        this.span.set(sOff, sLine, sCol, eOff, eLine, eCol);
    }

    @Override public String toString() {
        Pos s = span.start(), e = span.end();
        return (s.line() == e.line() && s.column() == e.column())
            ? String.format("error[%d:%d]: %s", s.line(), s.column(), message)
            : String.format("error[%d:%d-%d:%d]: %s",
                s.line(), s.column(), e.line(), e.column(), message);
    }
}
