package dev.parsuna.runtime;

/**
 * Per-token mode-stack callback. Specialized over {@code int} so the
 * runtime doesn't pay an {@link Integer} box on every lex token.
 *
 * <p>Generated code provides this as a method reference to a switch on
 * the token kind. Tokens with no {@code -> push/pop} actions fall through
 * with no effect; grammars that declare no modes get a no-op callback.
 */
@FunctionalInterface
public interface ApplyActions {
    void apply(int kind, Lexer lex);
}
