package dev.parsuna.runtime;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;

/**
 * DFA-driven byte-level lexer over an InputStream. Reads in 16 KiB chunks
 * so memory use stays bounded regardless of input size.
 */
public final class Lexer {
    static final int CHUNK = 16384;

    private final InputStream in;
    private byte[] buf;
    private int bufLen;
    private int bufPos;
    private boolean eof;
    private int offset;
    private int line = 1, col = 1;
    private final DfaConfig dfa;
    private final short eofKind;
    private final short errorKind;

    public Lexer(InputStream in, DfaConfig dfa, short eofKind, short errorKind) {
        this.in = in;
        this.buf = new byte[CHUNK * 2];
        this.dfa = dfa;
        this.eofKind = eofKind;
        this.errorKind = errorKind;
    }

    private Pos pos() { return new Pos(offset, line, col); }

    private boolean readMore() {
        if (eof) return false;
        if (buf.length - bufLen < CHUNK) {
            int newLen = buf.length;
            while (newLen - bufLen < CHUNK) newLen *= 2;
            byte[] nb = new byte[newLen];
            System.arraycopy(buf, 0, nb, 0, bufLen);
            buf = nb;
        }
        try {
            int n = in.read(buf, bufLen, buf.length - bufLen);
            if (n <= 0) { eof = true; return false; }
            bufLen += n;
            return true;
        } catch (IOException e) { eof = true; return false; }
    }

    private void ensure(int want) {
        while (!eof && bufLen - bufPos < want) { if (!readMore()) break; }
    }

    private void advance(int n) {
        int end = bufPos + n;
        boolean needsWalk = false;
        for (int k = bufPos; k < end; k++) {
            int b = buf[k] & 0xFF;
            if (b == '\n' || b >= 0x80) { needsWalk = true; break; }
        }
        if (!needsWalk) {
            col += n;
            offset += n;
            bufPos = end;
        } else {
            while (bufPos < end) {
                int b = buf[bufPos] & 0xFF; bufPos++; offset++;
                if (b == '\n') { line++; col = 1; }
                else if ((b & 0xC0) != 0x80) col++;
            }
        }
        if (bufPos > 65536) {
            System.arraycopy(buf, bufPos, buf, 0, bufLen - bufPos);
            bufLen -= bufPos; bufPos = 0;
        }
    }

    private int[] longestMatch() {
        while (true) {
            long state = dfa.start;
            int pos = bufPos;
            int bestLen = 0;
            int bestKind = errorKind;
            while (pos < bufLen) {
                int b = buf[pos] & 0xFF;
                long next = dfa.trans[(int)(state * 256) + b] & 0xFFFFFFFFL;
                if (next == 0) break;
                pos++;
                state = next;
                int acc = dfa.accept[(int) state] & 0xFFFF;
                if (acc != 0) {
                    bestLen = pos - bufPos;
                    bestKind = acc;
                }
            }
            if (!eof && pos == bufLen && bestLen > 0) {
                if (readMore()) continue;
            }
            return new int[] { bestLen, bestKind };
        }
    }

    /** Produce the next token. Emits repeated EOF once input ends. */
    public Token nextToken() {
        ensure(CHUNK);
        if (bufLen - bufPos == 0) { Pos p = pos(); return new Token(eofKind, Span.point(p), ""); }
        int[] best = longestMatch();
        Pos start = pos();
        if (best[0] == 0) {
            int b = buf[bufPos] & 0xFF;
            int cpLen = b < 0x80 ? 1 : b < 0xE0 ? 2 : b < 0xF0 ? 3 : 4;
            int n = Math.min(cpLen, bufLen - bufPos);
            String text = new String(buf, bufPos, n, StandardCharsets.UTF_8);
            advance(n);
            return new Token(errorKind, new Span(start, pos()), text);
        }
        String text = new String(buf, bufPos, best[0], StandardCharsets.UTF_8);
        advance(best[0]);
        return new Token((short) best[1], new Span(start, pos()), text);
    }
}
