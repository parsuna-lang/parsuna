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
//!    `Star`/`Opt`/`Dispatch`/`Expect` would observe an unfilled
//!    slot.
//!
//! 0-event paths *are* allowed — `step` returns `None` and
//! `next_event` loops the call. They show up in `Op::Star`/`Op::Opt`
//! miss paths (loop exit, optional skip) and in pure-control state
//! bodies that branch_inline didn't fold. Each optimizer pass is
//! also gated by [`is_valid_body`] so a rewrite never produces a
//! body that violates rule (1) or (2).

use crate::lowering::{DispatchLeaf, DispatchTree, Op, StateTable};

fn max_events_per_path(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match op {
            Op::Enter(_) | Op::Exit(_) | Op::Expect { .. } => 1,
            Op::PushRet(_) | Op::Jump(_) | Op::Ret => 0,
            Op::Star { body, .. } | Op::Opt { body, .. } => max_events_per_path(body),
            Op::Dispatch { tree, .. } => max_arm_events(tree),
        })
        .sum()
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

/// True iff `ops` is a legal state body. Both rules apply
/// recursively — a body whose surface ops respect them can still be
/// invalid if a nested body inside one of its branchy ops breaks
/// them.
pub fn is_valid_body(ops: &[Op]) -> bool {
    if max_events_per_path(ops) > 1 {
        return false;
    }
    no_lookahead_after_expect(ops)
}

fn no_lookahead_after_expect(ops: &[Op]) -> bool {
    let mut seen_expect = false;
    for op in ops {
        if seen_expect && !matches!(op, Op::PushRet(_) | Op::Jump(_) | Op::Ret) {
            return false;
        }
        match op {
            Op::Expect { .. } => seen_expect = true,
            Op::Star { body, .. } | Op::Opt { body, .. } => {
                if !no_lookahead_after_expect(body) {
                    return false;
                }
            }
            Op::Dispatch { tree, .. } => {
                if !dispatch_tree_post_expect_ok(tree) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
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
        if !is_valid_body(&state.ops) {
            panic!(
                "lowering invariant: state {} ('{}') has an invalid \
                 body — exceeds 1 event per path or reads lookahead \
                 after `Expect`. ops = {:?}",
                state.id, state.label, state.ops
            );
        }
    }
}
