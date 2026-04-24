package dev.parsuna.runtime;

/** A lexed token: kind id, span, and the matched text.
 * Kind is a short, matching the generated TokenKind enum's `id` field. */
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
