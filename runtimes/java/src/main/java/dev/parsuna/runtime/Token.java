package dev.parsuna.runtime;

import java.nio.charset.StandardCharsets;

/**
 * A lexed token: kind id, span, and the matched text.
 *
 * <p>{@link #kind} is an {@code int} (treated as unsigned 16-bit) matching
 * the generated TokenKind enum's {@code id} field. EOF = 0; grammar
 * tokens have ids starting at 1; the lexer emits {@link Lexer#ERROR_KIND}
 * (0xFFFF) when no DFA pattern matched at the current position.
 *
 * <p>The matched bytes, the start/end positions, and the {@link Span} are
 * built lazily — {@link #text()} decodes UTF-8 on first access, {@link
 * #start()}/{@link #end()}/{@link #span()} build their {@link Pos}/{@link
 * Span} on first access. Hot dispatch paths only ever read {@link #kind},
 * so the typical bench workload doesn't allocate any of them.
 *
 * <p>{@link #data} is the byte buffer holding this token's matched bytes,
 * sliced by {@link #byteOff} and {@link #byteLen}. In byte[] lexer mode
 * it's a reference into the original input array (zero-copy); in
 * InputStream mode the lexer hands the token a private byte[] copy.
 */
public final class Token {
    /** Token-kind id; matches the generated TokenKind enum's {@code id}. */
    public final int kind;

    /** Backing byte buffer. May be shared with the lexer's input array
     *  (byte[] mode) or owned by this token (InputStream mode). */
    final byte[] data;
    final int byteOff;
    final int byteLen;

    final int sOff, sLine, sCol;
    final int eOff, eLine, eCol;

    private String textCache;
    private Pos startCache;
    private Pos endCache;
    private Span spanCache;

    Token(int kind, byte[] data, int byteOff, int byteLen,
          int sOff, int sLine, int sCol,
          int eOff, int eLine, int eCol) {
        this.kind = kind;
        this.data = data;
        this.byteOff = byteOff;
        this.byteLen = byteLen;
        this.sOff = sOff; this.sLine = sLine; this.sCol = sCol;
        this.eOff = eOff; this.eLine = eLine; this.eCol = eCol;
    }

    /** Convenience constructor for the EOF/error/synthetic case where the
     *  matched text and span are computed up front. */
    public Token(int kind, Span span, String text) {
        this(kind,
             text == null ? new byte[0] : text.getBytes(StandardCharsets.UTF_8),
             0,
             text == null ? 0 : text.getBytes(StandardCharsets.UTF_8).length,
             span.start.offset, span.start.line, span.start.column,
             span.end.offset, span.end.line, span.end.column);
        this.spanCache = span;
        this.startCache = span.start;
        this.endCache = span.end;
        this.textCache = text == null ? "" : text;
    }

    /** Number of source bytes the token occupies. Cheap — no decode, no
     *  String materialization. Use this in hot loops that only need a
     *  size proxy (e.g. byte-throughput benchmarks). */
    public int byteLen() { return byteLen; }

    /** Decoded UTF-8 text. Cached on first call. */
    public String text() {
        String t = textCache;
        if (t == null) {
            t = new String(data, byteOff, byteLen, StandardCharsets.UTF_8);
            textCache = t;
        }
        return t;
    }

    public Pos start() {
        Pos p = startCache;
        if (p == null) { p = new Pos(sOff, sLine, sCol); startCache = p; }
        return p;
    }

    public Pos end() {
        Pos p = endCache;
        if (p == null) { p = new Pos(eOff, eLine, eCol); endCache = p; }
        return p;
    }

    public Span span() {
        Span s = spanCache;
        if (s == null) { s = new Span(start(), end()); spanCache = s; }
        return s;
    }
}
