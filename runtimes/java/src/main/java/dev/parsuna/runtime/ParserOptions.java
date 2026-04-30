package dev.parsuna.runtime;

/**
 * Runtime-level toggles the caller picks per parser construction.
 *
 * <p>Today the only knob is {@link #dropSkips}; future runtime-level
 * options can extend the class without breaking call sites because the
 * default constructor stays compatible.
 */
public final class ParserOptions {
    /**
     * When {@code true}, skip tokens (whitespace, comments) are silently
     * consumed instead of yielded as {@link Event.Token}. The lexer still
     * matches them — they delimit structural tokens — but the parser's
     * iteration never returns them. Default {@code false}, matching the
     * historic behaviour.
     */
    public final boolean dropSkips;

    public ParserOptions() {
        this.dropSkips = false;
    }

    public ParserOptions(boolean dropSkips) {
        this.dropSkips = dropSkips;
    }
}
