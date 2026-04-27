package dev.parsuna.runtime;

import java.util.function.Predicate;
import java.util.function.Consumer;

/** Grammar-specific callbacks a generated parser wires into the runtime. */
public final class ParserConfig {
    /** Token-kind id reserved for end-of-input. Always 0; exposed as a constant
     *  so generated code and consumers don't sprinkle magic numbers. */
    public static final int EOF_KIND = 0;

    public final int k;
    public final Predicate<Integer> isSkip;
    public final Consumer<Parser> drive;
    public ParserConfig(int k, Predicate<Integer> isSkip, Consumer<Parser> drive) {
        this.k = k;
        this.isSkip = isSkip;
        this.drive = drive;
    }
}
