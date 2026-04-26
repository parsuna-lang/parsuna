package dev.parsuna.runtime;

import java.util.function.IntPredicate;
import java.util.function.Consumer;

/** Grammar-specific callbacks a generated parser wires into the runtime. */
public final class ParserConfig {
    public final int k;
    public final int eofKind;
    public final IntPredicate isSkip;
    public final Consumer<Parser> drive;
    public ParserConfig(int k, int eofKind, IntPredicate isSkip, Consumer<Parser> drive) {
        this.k = k;
        this.eofKind = eofKind;
        this.isSkip = isSkip;
        this.drive = drive;
    }
}
