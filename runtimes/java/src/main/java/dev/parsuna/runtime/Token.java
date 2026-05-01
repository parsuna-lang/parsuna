package dev.parsuna.runtime;

import java.nio.charset.StandardCharsets;

/**
 * A lexed token: kind id, source span, and the matched text.
 *
 * <p>Mutable and pooled by the runtime — a parser holds K+1 of these
 * and rewrites their fields as the lex stream advances. The
 * {@link Token} reference returned via {@link Event.Token#token()}
 * stays valid only until the next {@link Parser#next()} call. If you
 * need to keep a token (e.g. to build an AST), call {@link #snapshot()}.
 *
 * <p>{@link #kind()} is an unsigned 16-bit kind id matching the
 * generated TokenKind enum's {@code id} field. EOF = 0; grammar tokens
 * have ids starting at 1; the lexer emits {@link Lexer#ERROR_KIND}
 * (0xFFFF) when no DFA pattern matched at the current position. The
 * parser runtime turns those into a paired {@link Event.Error}
 * +{@link Event.Garbage} sequence at pump time and never lets them
 * reach generated dispatch.
 *
 * <p>The decoded text and {@link Span} are built lazily — {@link #text()}
 * decodes UTF-8 on first access (and clears the cache on each set);
 * {@link #span()} returns a stable reference to a pooled span whose
 * fields are kept in sync with {@code byteOff/byteLen} and the start /
 * end positions.
 */
public final class Token {
    int kind;
    /** Backing byte buffer for {@link #text()}. May be the lexer's input
     *  array (zero-copy byte[] mode) or a per-token private copy
     *  (InputStream mode). */
    byte[] data;
    int byteOff;
    int byteLen;
    private final Span span = new Span();
    private String textCache;
    /** Grammar-position label from a {@code name:NAME} form, or
     *  {@code null} if the position wasn't labeled. Set by the
     *  dispatch's labeled {@code tryConsume} on the success path; the
     *  runtime clears it on every lex-time {@link #set} so the field
     *  defaults to {@code null} until a labeled-expect re-stamps it. */
    String label;

    public Token() {
        this.data = EMPTY;
    }

    /** Convenience constructor for synthetic tokens (e.g. EOF stubs).
     *  Allocates — not used on the runtime hot path. */
    public Token(int kind, Span span, String text) {
        byte[] bytes = text == null ? EMPTY : text.getBytes(StandardCharsets.UTF_8);
        this.kind = kind;
        this.data = bytes;
        this.byteOff = 0;
        this.byteLen = bytes.length;
        this.span.copyFrom(span);
        this.textCache = text == null ? "" : text;
    }

    /** Token-kind id; matches the generated {@code TokenKind} enum's {@code id}. */
    public int kind() { return kind; }

    /** Number of source bytes the token occupies. Cheap — no decode, no
     *  String materialization. */
    public int byteLen() { return byteLen; }

    /** Source span (stable reference; contents track this token). Snapshot
     *  via {@link Span#snapshot()} if you need to keep the span past the
     *  next {@link Parser#next()} call. */
    public Span span() { return span; }

    /** Grammar label (e.g. {@code "name"} for a {@code name:IDENT}
     *  position), or {@code null} for unlabeled positions. */
    public String label() { return label; }

    /** Decoded UTF-8 text. Cached on first call; the cache is cleared
     *  the next time the runtime rewrites this token. */
    public String text() {
        String t = textCache;
        if (t == null) {
            t = new String(data, byteOff, byteLen, StandardCharsets.UTF_8);
            textCache = t;
        }
        return t;
    }

    /** Return a fresh, fully-detached copy. Use to keep the token past
     *  the next {@link Parser#next()} call. */
    public Token snapshot() {
        Token c = new Token();
        c.kind = this.kind;
        if (this.byteLen == 0) {
            c.data = EMPTY;
        } else {
            byte[] copy = new byte[this.byteLen];
            System.arraycopy(this.data, this.byteOff, copy, 0, this.byteLen);
            c.data = copy;
        }
        c.byteOff = 0;
        c.byteLen = this.byteLen;
        c.span.copyFrom(this.span);
        c.textCache = this.textCache;
        c.label = this.label;
        return c;
    }

    /** Raw byte slice (start offset and length live in the token) for
     *  callers that want to read the bytes directly without UTF-8
     *  decoding. */
    public byte[] data() { return data; }
    public int byteOff() { return byteOff; }

    void set(int kind, byte[] data, int byteOff, int byteLen,
             int sOff, int sLine, int sCol,
             int eOff, int eLine, int eCol) {
        this.kind = kind;
        this.data = data;
        this.byteOff = byteOff;
        this.byteLen = byteLen;
        this.span.set(sOff, sLine, sCol, eOff, eLine, eCol);
        this.textCache = null;
        // Reset on every fresh lex hit; the labeled-expect path
        // re-stamps it on the success branch.
        this.label = null;
    }

    private static final byte[] EMPTY = new byte[0];
}
