package dev.parsuna.runtime;

import java.io.IOException;
import java.io.InputStream;

/**
 * DFA-driven byte-level lexer. Two construction shapes:
 *
 * <ul>
 *   <li>{@link #Lexer(InputStream, DfaMatcher)} reads in 16 KiB chunks
 *   so memory use stays bounded for arbitrary-sized input.</li>
 *   <li>{@link #Lexer(byte[], DfaMatcher)} parses an in-memory buffer
 *   directly — no chunking, no read calls, no compaction. Use this
 *   when the input is already a byte array (e.g.
 *   {@code Files.readAllBytes}) to skip the {@link InputStream} hop.</li>
 * </ul>
 *
 * <p>{@link #nextToken(Token)} writes the matched token's metadata into
 * the caller-supplied {@link Token} — the runtime hands the lexer one of
 * its pooled tokens so the lex step never allocates a Token on its own.
 */
public final class Lexer {
    static final int CHUNK = 16384;

    /**
     * Token-kind sentinel emitted when no DFA pattern matches at the current
     * position. Distinct from any grammar token id (0..0xFFFE), so dispatch
     * arms naturally fall through to the recovery path.
     */
    public static final int ERROR_KIND = 0xFFFF;

    private final InputStream in;
    private byte[] buf;
    private int bufLen;
    private int bufPos;
    private boolean eof;
    private int offset;
    private int line = 1, col = 1;
    private final DfaMatcher matcher;
    private final int[] matchOut = new int[3];
    /** Mode stack — top-of-stack is the active mode. Initialised with
     *  the default mode (id 0) and never empty; pop is a no-op when
     *  only the default remains so a stray `pop` action can't underflow. */
    private int[] modeStack = new int[]{0};
    private int modeTop = 0;

    public Lexer(InputStream in, DfaMatcher matcher) {
        this.in = in;
        this.buf = new byte[CHUNK * 2];
        this.matcher = matcher;
    }

    /** Lex directly out of an existing byte buffer. The buffer is held
     *  by reference (no copy); callers must not mutate it for the
     *  lifetime of the parse. */
    public Lexer(byte[] data, DfaMatcher matcher) {
        this.in = null;
        this.buf = data;
        this.bufLen = data.length;
        this.eof = true;
        this.matcher = matcher;
    }

    /** Push {@code mode} onto the mode stack — typically called from a
     *  generated apply-actions callback after the matched token. */
    public void pushMode(int mode) {
        if (modeTop + 1 >= modeStack.length) {
            int[] next = new int[modeStack.length * 2];
            System.arraycopy(modeStack, 0, next, 0, modeTop + 1);
            modeStack = next;
        }
        modeStack[++modeTop] = mode;
    }

    /** Pop the topmost mode, leaving at least the default mode in place. */
    public void popMode() {
        if (modeTop > 0) modeTop--;
    }

    /** Number of modes currently on the stack (always &gt;= 1). Used by
     *  the parser's recovery path to remember "the depth at the time
     *  we entered this rule". */
    public int modeDepth() { return modeTop + 1; }

    /** Pop modes until the stack reaches {@code targetDepth}, clamped
     *  at the bottom by the default mode. No-op when already at or
     *  below {@code targetDepth}. */
    public void popModesTo(int targetDepth) {
        int target = Math.max(1, targetDepth);
        if (modeTop + 1 > target) modeTop = target - 1;
    }

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

    /** Advance past {@code n} bytes, updating offset/line/col. ASCII-only
     *  spans take an unrolled fast path (no per-byte branch); a non-ASCII
     *  byte or newline triggers the precise walker. */
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
    }

    private void runMatch() {
        while (true) {
            matcher.longestMatch(buf, bufPos, bufLen, modeStack[modeTop], matchOut);
            int scanned = matchOut[2];
            int viewLen = bufLen - bufPos;
            if (!eof && scanned == viewLen) {
                if (readMore()) continue;
            }
            return;
        }
    }

    /**
     * Match the next token, writing its kind, byte slice, and start/end
     * position into {@code out}. Emits repeated EOF (kind 0, empty
     * slice) once input ends; lex failures (no DFA pattern matched)
     * come through with {@code out.kind() == }{@link #ERROR_KIND}.
     */
    public void nextToken(Token out) {
        if (in != null) ensure(CHUNK);
        if (bufLen - bufPos == 0) {
            out.set(0, buf, bufPos, 0,
                offset, line, col,
                offset, line, col);
            return;
        }
        // Auto-pop on mismatch: if no DFA match in the active mode and
        // we're not at the default mode, drop one mode and retry —
        // "if you can't find a token in this mode, you weren't
        // supposed to be in this mode anymore." Keeps a stray byte
        // (e.g. unescaped `&` followed by free-form text) from
        // stranding the lexer in an interior mode for the rest of
        // the input.
        runMatch();
        while (matchOut[0] == 0 && modeTop > 0) {
            modeTop--;
            runMatch();
        }
        int sOff = offset, sLine = line, sCol = col;
        int startBufPos = bufPos;
        int matchLen;
        int kind;
        if (matchOut[0] == 0) {
            int b = buf[bufPos] & 0xFF;
            int cpLen = b < 0x80 ? 1 : b < 0xE0 ? 2 : b < 0xF0 ? 3 : 4;
            matchLen = Math.min(cpLen, bufLen - bufPos);
            kind = ERROR_KIND;
        } else {
            matchLen = matchOut[0];
            kind = matchOut[1];
        }
        byte[] data;
        int byteOff;
        if (in == null) {
            // byte[] mode: the buffer IS the input and is never compacted
            // or grown, so tokens can share it by reference.
            data = buf;
            byteOff = startBufPos;
        } else {
            // InputStream mode: the buffer is volatile (compaction +
            // growth), so the token owns a private copy of its bytes.
            data = new byte[matchLen];
            System.arraycopy(buf, bufPos, data, 0, matchLen);
            byteOff = 0;
        }
        advance(matchLen);
        out.set(kind, data, byteOff, matchLen,
            sOff, sLine, sCol,
            offset, line, col);
        // InputStream mode: now that we've copied the bytes out, it's safe
        // to compact the read buffer.
        if (in != null && bufPos > 65536) {
            System.arraycopy(buf, bufPos, buf, 0, bufLen - bufPos);
            bufLen -= bufPos; bufPos = 0;
        }
    }
}
