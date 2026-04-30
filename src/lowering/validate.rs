//! Runtime invariants the lowering must preserve. There are two,
//! both per-state-body:
//!
//! 1. **At most one event per execution path** through the body.
//!    `Drive::step` runs one match arm and returns the event the
//!    body's path produced. Two events on the same path would mean
//!    one of them got lost.
//!
//! 2. **No lookahead-reading op after an `Expect`** in the same body.
//!    An `Expect` consume leaves slot `K-1` empty; the runtime can
//!    only refill between `step` calls, so any subsequent
//!    `Star`/`Opt`/`Dispatch`/`Expect` would observe an unfilled slot.
//!
//! 0-event paths *are* allowed — `step` returns `None` and
//! `next_event` loops the call. They show up in `Tail::Star`/`Tail::Opt`
//! miss paths (loop exit, optional skip) and in pure-control state
//! bodies that `inline_branch_bodies` didn't fold. Each optimizer pass
//! is also gated by [`is_valid_body`] so a rewrite never produces a
//! body that violates rule (1) or (2).
//!
//! The IR's `Body { instrs, tail }` split makes the structural rule
//! "every body ends in a terminator" unrepresentable to violate. These
//! checks cover the two semantic rules the type system can't express.

use crate::lowering::{Body, DispatchLeaf, DispatchTree, Instr, StateTable, Tail};

fn max_events_per_path(body: &Body) -> usize {
    let head: usize = body.instrs.iter().map(events_in_instr).sum();
    head + events_in_tail(&body.tail)
}

fn events_in_instr(op: &Instr) -> usize {
    match op {
        Instr::Enter(_) | Instr::Exit(_) | Instr::Expect { .. } => 1,
        Instr::PushRet(_) => 0,
    }
}

fn events_in_tail(tail: &Tail) -> usize {
    match tail {
        Tail::Jump(_) | Tail::Ret => 0,
        Tail::Star { body, .. } | Tail::Opt { body, .. } => max_events_per_path(body),
        Tail::Dispatch { tree, .. } => max_arm_events(tree),
    }
}

fn max_arm_events(tree: &DispatchTree) -> usize {
    fn arm(leaf: &DispatchLeaf) -> usize {
        match leaf {
            DispatchLeaf::Arm(b) => max_events_per_path(b),
            // Fallthrough is a pure state transition — 0 events.
            DispatchLeaf::Fallthrough => 0,
            // Error pushes one `Event::Error` and arms recovery; that
            // counts as one event toward the per-body cap.
            DispatchLeaf::Error => 1,
        }
    }
    match tree {
        DispatchTree::Leaf(l) => arm(l),
        DispatchTree::Switch { arms, default, .. } => arm(default).max(
            arms.iter()
                .map(|(_, sub)| max_arm_events(sub))
                .max()
                .unwrap_or(0),
        ),
    }
}

/// True iff `body` is a legal state body. Both rules apply
/// recursively — a body whose surface ops respect them can still be
/// invalid if a nested body inside its tail breaks them.
pub fn is_valid_body(body: &Body) -> bool {
    if max_events_per_path(body) > 1 {
        return false;
    }
    no_lookahead_after_expect(body)
}

fn no_lookahead_after_expect(body: &Body) -> bool {
    // Rule 2 only forbids lookahead-readers AFTER an Expect. The
    // `instrs` list contains only Enter/Exit/Expect/PushRet, none of
    // which read look(i) for i > 0 — Enter/Exit just record the
    // current span, PushRet doesn't touch lookahead. So the head is
    // always fine.
    //
    // The branchy ops (Star/Opt/Dispatch) live in the tail, and they
    // *do* read lookahead. If the head emitted an Expect, the tail
    // must therefore be one of the non-reading variants.
    let head_has_expect = body
        .instrs
        .iter()
        .any(|op| matches!(op, Instr::Expect { .. }));
    if head_has_expect {
        match &body.tail {
            Tail::Jump(_) | Tail::Ret => {}
            // Star/Opt/Dispatch read lookahead — illegal after Expect.
            _ => return false,
        }
    }
    // Recurse into nested bodies — they need to satisfy the rule too.
    match &body.tail {
        Tail::Star { body, .. } | Tail::Opt { body, .. } => no_lookahead_after_expect(body),
        Tail::Dispatch { tree, .. } => dispatch_tree_post_expect_ok(tree),
        Tail::Jump(_) | Tail::Ret => true,
    }
}

fn dispatch_tree_post_expect_ok(tree: &DispatchTree) -> bool {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(body)) => no_lookahead_after_expect(body),
        DispatchTree::Leaf(_) => true,
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                if !no_lookahead_after_expect(body) {
                    return false;
                }
            }
            arms.iter()
                .all(|(_, sub)| dispatch_tree_post_expect_ok(sub))
        }
    }
}

/// Walk every body in the table and panic if any invariant is broken.
/// Mandatory final step of `lower_with_opts` — catches source bugs
/// (a layout/optimizer pass producing a malformed body) before the
/// table reaches codegen.
pub fn assert_runtime_invariants(table: &StateTable) {
    for state in table.states.values() {
        if !is_valid_body(&state.body) {
            panic!(
                "lowering invariant: state {} ('{}') has an invalid \
                 body — exceeds 1 event per path or reads lookahead \
                 after `Expect`. body = {:?}",
                state.id, state.label, state.body
            );
        }
    }
}
