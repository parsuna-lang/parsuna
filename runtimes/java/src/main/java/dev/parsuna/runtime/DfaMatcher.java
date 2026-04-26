package dev.parsuna.runtime;

/**
 * Grammar-specific compiled lexer DFA. Generated code supplies this as a
 * method reference — a per-state switch that avoids the data-dependent
 * table load a table-driven DFA pays on every byte.
 *
 * <p>The implementation scans {@code buf[start..bufLen]} for the longest
 * matching token and writes three ints to {@code out}:
 * <ol start="0">
 *   <li>{@code bestLen} — bytes consumed by the longest match.
 *       {@code 0} means no accept state was reached, in which case
 *       {@code bestKind} is meaningless.</li>
 *   <li>{@code bestKind} — token-kind id of the longest match (only
 *       valid when {@code bestLen > 0}).</li>
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
    /** Run the DFA; write [bestLen, bestKind, scanned] into {@code out}. */
    void longestMatch(byte[] buf, int start, int bufLen, int[] out);
}
