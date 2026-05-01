package dev.parsuna.runtime;

/**
 * Runtime-level toggles the caller picks per parser construction.
 *
 * <p>Zero-argument construction matches the historic emit-everything
 * behaviour, so existing callers stay unchanged.
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
    /**
     * When {@code true}, every {@link Event.Token} whose {@link Token#label()}
     * is {@code null} (i.e. that didn't match a {@code name:NAME} position
     * in the grammar) is silently consumed. Structural events
     * ({@link Event.Enter}/{@link Event.Exit}/{@link Event.Error}) and
     * {@link Event.Garbage} still flow through. The "give me an AST shape,
     * drop the punctuation" mode for tree-building consumers.
     *
     * <p>Implies skip-token suppression — skip tokens never carry a label.
     * Default {@code false}.
     */
    public final boolean dropUnlabeledTokens;

    public ParserOptions() {
        this.dropSkips = false;
        this.dropUnlabeledTokens = false;
    }

    public ParserOptions(boolean dropSkips) {
        this.dropSkips = dropSkips;
        this.dropUnlabeledTokens = false;
    }

    public ParserOptions(boolean dropSkips, boolean dropUnlabeledTokens) {
        this.dropSkips = dropSkips;
        this.dropUnlabeledTokens = dropUnlabeledTokens;
    }
}
