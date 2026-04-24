package dev.parsuna.runtime;

/** Packed lexer DFA table. State 0 is the dead sink; real states start at 1. */
public final class DfaConfig {
    public final int start;
    /** Flat transition table: `trans[state * 256 + byte]` is the next state (0 = dead). */
    public final int[] trans;
    /** Acceptance table: `accept[state]` is the token-kind id, or 0 if not accepting. */
    public final short[] accept;
    public DfaConfig(int start, int[] trans, short[] accept) {
        this.start = start;
        this.trans = trans;
        this.accept = accept;
    }
}
