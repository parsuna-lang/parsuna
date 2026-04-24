package dev.parsuna.runtime;

/**
 * A single pull-parser event. Sealed over the four subtypes below — use a
 * switch expression with pattern labels to extract payloads:
 *
 * <pre>{@code
 * switch (ev) {
 *     case Event.Enter e -> ...;       // e.rule(), e.pos()
 *     case Event.Exit e  -> ...;       // e.rule(), e.pos()
 *     case Event.Token t -> ...;       // t.token()
 *     case Event.Error x -> ...;       // x.error()
 * }
 * }</pre>
 */
public sealed interface Event
    permits Event.Enter, Event.Exit, Event.Token, Event.Error
{
    /** Opens the subtree of the given rule; {@code pos} is the start of the first child. */
    record Enter(int rule, Pos pos) implements Event {}

    /** Closes the subtree of the given rule; {@code pos} is the end of the last child. */
    record Exit(int rule, Pos pos) implements Event {}

    /** A token consumed from the input, including skip tokens. */
    record Token(dev.parsuna.runtime.Token token) implements Event {}

    /** A recoverable parse or lex error. */
    record Error(ParseError error) implements Event {}
}
