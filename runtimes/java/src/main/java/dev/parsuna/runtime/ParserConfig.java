package dev.parsuna.runtime;

import java.util.function.Predicate;
import java.util.function.Consumer;

/** Grammar-specific callbacks a generated parser wires into the runtime. */
public final class ParserConfig {
    /** Token-kind id reserved for end-of-input. Always 0; exposed as a constant
     *  so generated code and consumers don't sprinkle magic numbers. */
    public static final int EOF_KIND = 0;

    /** Lookahead required to disambiguate every alternative (LL(k)). */
    public final int k;
    /**
     * Hard cap on events the parser's fixed-size queue can hold. Equal to
     * the longest emit burst across every state body in this grammar —
     * the lowering pass computes this from the state table so the queue
     * is exactly large enough for the worst-case structural burst.
     */
    public final int queueCap;
    public final Predicate<Integer> isSkip;
    public final Consumer<Parser> drive;
    public ParserConfig(int k, int queueCap, Predicate<Integer> isSkip, Consumer<Parser> drive) {
        this.k = k;
        this.queueCap = queueCap;
        this.isSkip = isSkip;
        this.drive = drive;
    }
}
