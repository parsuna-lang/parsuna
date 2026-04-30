package dev.parsuna.runtime;

/**
 * Grammar-specific compiled lexer DFA. Generated code supplies this as a
 * method reference — a per-state switch that avoids the data-dependent
 * table load a table-driven DFA pays on every byte.
 *
 * <p>The implementation scans {@code buf[start..bufLen]} for the longest
 * matching token and writes three ints to {@code out}:
 * <ol start="0">
 *   <li>{@code bestLen} — bytes consumed by the longest match (0 = no accept).</li>
 *   <li>{@code bestKind} — token-kind id of the longest match, or the error
 *       sentinel when no accept state was reached.</li>
 *   <li>{@code scanned} — total bytes walked past {@code start}; equals
 *       {@code bufLen - start} when the scan stopped at end of input rather
 *       than at a dead transition. The lexer uses this to detect buffer
 *       exhaustion mid-token.</li>
 * </ol>
 *
 * <p>Using a caller-supplied {@code int[3]} avoids allocating per call.
 */
@FunctionalInterface
public interface DfaMatcher {
    /** Run the DFA in {@code mode}; write [bestLen, bestKind, scanned]
     *  into {@code out}. {@code mode} is 0 for the default mode; further
     *  ids correspond to {@code @mode(name)} pre-annotations in
     *  declaration order. Generated code branches on it and dispatches
     *  to the per-mode state machine. */
    void longestMatch(byte[] buf, int start, int bufLen, int mode, int[] out);
}
