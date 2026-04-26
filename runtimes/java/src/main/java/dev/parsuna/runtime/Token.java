package dev.parsuna.runtime;

/**
 * A lexed token: kind id, span, and the matched text.
 *
 * <p>{@code kind} is a uint16-range value matching the generated TokenKind
 * enum's {@code id} field. Read it only when {@code kindOk} is true;
 * {@code kindOk} is false only when the lexer could not match any
 * pattern at the current position. EOF has its own kind value
 * ({@code kindOk == true}).
 */
public final class Token {
    public final int kind;
    public final boolean kindOk;
    public final Span span;
    public final String text;

    public Token(int kind, boolean kindOk, Span span, String text) {
        this.kind = kind;
        this.kindOk = kindOk;
        this.span = span;
        this.text = text;
    }
}
