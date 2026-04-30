package dev.parsuna.runtime;

import java.util.function.IntPredicate;

/** Grammar-specific callbacks a generated parser wires into the runtime.
 *
 *  <p>{@code step} runs one state body of the dispatch and (optionally)
 *  emits one event via the {@link Cursor} hooks. The runtime's
 *  {@code next} loop calls {@code step} again when the body was a pure
 *  transition.
 *
 *  <p>{@code step} receives a {@link Cursor} — a thin wrapper that
 *  exposes just the runtime hooks generated dispatch needs (look/state/
 *  pushRet/enter/exit/etc.). Outside callers only see {@link Parser},
 *  which has no such methods, so the parser's internal state stays
 *  sealed.
 *
 *  <p>{@code applyActions} fires the per-token mode-stack actions
 *  declared via {@code -> push(name)} / {@code -> pop} once the lexer
 *  has handed the runtime a fresh token. Tokens with no actions fall
 *  through with no effect; grammars that declare no modes get a no-op
 *  callback.
 *
 *  <p>{@code isSkip} and {@code applyActions} are specialized over
 *  {@code int} so the per-token runtime path doesn't pay an
 *  {@link Integer} box on every lex token. */
public final class ParserConfig {
    /** Token-kind id reserved for end-of-input. Always 0; exposed as a constant
     *  so generated code and consumers don't sprinkle magic numbers. */
    public static final int EOF_KIND = 0;

    /** Lookahead required to disambiguate every alternative (LL(k)). */
    public final int k;
    public final IntPredicate isSkip;
    public final DriveStep step;
    public final ApplyActions applyActions;

    public ParserConfig(int k, IntPredicate isSkip, DriveStep step) {
        this(k, isSkip, step, (kind, lex) -> {});
    }

    public ParserConfig(
        int k,
        IntPredicate isSkip,
        DriveStep step,
        ApplyActions applyActions
    ) {
        this.k = k;
        this.isSkip = isSkip;
        this.step = step;
        this.applyActions = applyActions;
    }
}
