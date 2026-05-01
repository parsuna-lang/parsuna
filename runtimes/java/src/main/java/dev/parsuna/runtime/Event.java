package dev.parsuna.runtime;

/**
 * A single pull-parser event. The runtime pools one instance of each
 * variant and hands the same reference back from each {@link Parser#next()}
 * call with its fields rewritten — so the hot iteration path allocates
 * nothing per event. Switch on the variant via pattern matching:
 *
 * <pre>{@code
 * switch (ev) {
 *     case Event.Enter e   -> ...;     // e.rule(), e.pos()
 *     case Event.Exit e    -> ...;     // e.rule(), e.pos()
 *     case Event.Token t   -> ...;     // t.token()
 *     case Event.Garbage g -> ...;     // g.token()
 *     case Event.Error x   -> ...;     // x.error()
 * }
 * }</pre>
 *
 * <p>The reference returned by {@code next()} (and any nested {@link Token},
 * {@link ParseError}, {@link Pos}, {@link Span}) is only valid until the
 * next {@code next()} call. Snapshot via the inner type's {@code snapshot()}
 * helper if you need to keep the data around (for AST construction, etc.).
 */
public sealed interface Event
    permits Event.Enter, Event.Exit, Event.Token, Event.Garbage, Event.Error
{
    /** Opens the subtree of the given rule; {@code pos} is the start of the first child. */
    final class Enter implements Event {
        int rule;
        private final Pos pos = new Pos();

        public int rule() { return rule; }
        public Pos pos() { return pos; }

        void set(int rule, int posOff, int posLine, int posCol) {
            this.rule = rule;
            this.pos.set(posOff, posLine, posCol);
        }

        @Override public String toString() {
            return "Enter[rule=" + rule + ", pos=" + posStr(pos) + "]";
        }
    }

    /** Closes the subtree of the given rule; {@code pos} is the end of the last child. */
    final class Exit implements Event {
        int rule;
        private final Pos pos = new Pos();

        public int rule() { return rule; }
        public Pos pos() { return pos; }

        void set(int rule, int posOff, int posLine, int posCol) {
            this.rule = rule;
            this.pos.set(posOff, posLine, posCol);
        }

        @Override public String toString() {
            return "Exit[rule=" + rule + ", pos=" + posStr(pos) + "]";
        }
    }

    /** A token consumed from the input, including skip tokens and the
     *  synced-to-expected token after a recovery. The wrapped
     *  {@link dev.parsuna.runtime.Token} reference is stable for the
     *  lifetime of the event but its fields are rewritten on the next
     *  {@code next()} — call {@link dev.parsuna.runtime.Token#snapshot()}
     *  to keep the data. */
    final class Token implements Event {
        dev.parsuna.runtime.Token token;
        public dev.parsuna.runtime.Token token() { return token; }

        @Override public String toString() {
            return "Token[token=" + token + "]";
        }
    }

    /** A token consumed during error recovery — emitted between an
     *  {@link Error} event and the recovery's sync point. Distinct from
     *  {@link Token} so consumers can drop it from their AST or render
     *  it as an error span without tracking recovery state. */
    final class Garbage implements Event {
        dev.parsuna.runtime.Token token;
        public dev.parsuna.runtime.Token token() { return token; }

        @Override public String toString() {
            return "Garbage[token=" + token + "]";
        }
    }

    /** A recoverable parse or lex error. Wraps a pooled
     *  {@link ParseError} whose contents are rewritten on the next
     *  {@code next()} — snapshot if you need to keep it. */
    final class Error implements Event {
        private final ParseError error = new ParseError();
        public ParseError error() { return error; }

        @Override public String toString() { return "Error[" + error + "]"; }
    }

    private static String posStr(Pos p) {
        return "Pos[" + p.offset() + ", " + p.line() + ":" + p.column() + "]";
    }
}
