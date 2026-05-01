package dev.parsuna.runtime;

/**
 * Generated drive function. Runs one state body of the dispatch and
 * returns the {@link Event} that body produced — one of the parser's
 * pooled event instances — or {@code null} for a pure transition step.
 * The runtime's pull loop calls {@code step} again in the {@code null}
 * case.
 */
@FunctionalInterface
public interface DriveStep {
    Event step(Cursor cur);
}
