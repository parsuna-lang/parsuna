package dev.parsuna.runtime;

/** A recoverable parse or lex error with its source span. */
public final class ParseError {
    public final String message;
    public final Span span;
    public ParseError(String message, Span span) {
        this.message = message;
        this.span = span;
    }
    @Override public String toString() {
        Pos s = span.start, e = span.end;
        return (s.line == e.line && s.column == e.column)
            ? String.format("error[%d:%d]: %s", s.line, s.column, message)
            : String.format("error[%d:%d-%d:%d]: %s", s.line, s.column, e.line, e.column, message);
    }
}
