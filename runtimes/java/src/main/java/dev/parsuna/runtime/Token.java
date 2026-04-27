package dev.parsuna.runtime;

/**
 * A lexed token: kind id, span, and the matched text.
 *
 * <p>{@code kind} is an {@code int} (treated as unsigned 16-bit) matching
 * the generated TokenKind enum's {@code id} field. EOF = 0; grammar
 * tokens have ids starting at 1; the lexer emits {@link Lexer#ERROR_KIND}
 * (0xFFFF) when no DFA pattern matched at the current position.
 */
public final class Token {
    public final int kind;
    public final Span span;
    public final String text;
    public Token(int kind, Span span, String text) {
        this.kind = kind;
        this.span = span;
        this.text = text;
    }
}
