package dev.parsuna.runtime;

/**
 * Generated drive function. Runs one state body of the dispatch and
 * (optionally) writes one event into the parser's scalar event fields
 * via {@link Cursor}'s emit methods. The runtime decides whether to
 * yield based on {@link Parser#evtKind} after the call returns.
 *
 * <p>Specialized over a {@code void}-returning interface so the per-step
 * runtime path doesn't pay an allocation for an event wrapper — the
 * cursor methods write straight into scalar fields the runtime already
 * owns.
 */
@FunctionalInterface
public interface DriveStep {
    void step(Cursor cur);
}
