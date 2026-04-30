package dev.parsuna.runtime;

import java.util.function.Predicate;
import java.util.function.Function;

/** Grammar-specific callbacks a generated parser wires into the runtime.
 *
 *  <p>{@code step} runs one state body of the dispatch and returns the
 *  event that body produced, or {@code null} if the body was a pure
 *  transition step — the runtime's {@code nextEvent} loop calls
 *  {@code step} again in that case. */
public final class ParserConfig {
    /** Token-kind id reserved for end-of-input. Always 0; exposed as a constant
     *  so generated code and consumers don't sprinkle magic numbers. */
    public static final int EOF_KIND = 0;

    /** Lookahead required to disambiguate every alternative (LL(k)). */
    public final int k;
    public final Predicate<Integer> isSkip;
    public final Function<Parser, Event> step;
    public ParserConfig(int k, Predicate<Integer> isSkip, Function<Parser, Event> step) {
        this.k = k;
        this.isSkip = isSkip;
        this.step = step;
    }
}
