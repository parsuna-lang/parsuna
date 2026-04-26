package dev.parsuna.runtime;

/**
 * A lexed token: kind id, span, and the matched text.
 *
 * <p>{@code kind} is a {@code short} matching the generated TokenKind
 * enum's {@code id} field. The runtime reserves {@code -1} as the "no
 * DFA pattern matched" sentinel that the lexer emits when it can't
 * classify a codepoint; real token kinds (including EOF = 0) are
 * non-negative.
 */
public final class Token {
    public final short kind;
    public final Span span;
    public final String text;
    public Token(short kind, Span span, String text) {
        this.kind = kind;
        this.span = span;
        this.text = text;
    }
}
