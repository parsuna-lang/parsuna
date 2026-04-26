package dev.parsuna.runtime;

import java.util.function.Predicate;
import java.util.function.Consumer;

/** Grammar-specific callbacks a generated parser wires into the runtime. */
public final class ParserConfig {
    public final int k;
    public final short eofKind;
    public final Predicate<Short> isSkip;
    public final Consumer<Parser> drive;
    public ParserConfig(int k, short eofKind, Predicate<Short> isSkip, Consumer<Parser> drive) {
        this.k = k;
        this.eofKind = eofKind;
        this.isSkip = isSkip;
        this.drive = drive;
    }
}
