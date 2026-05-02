//! Dispatch insertion-recovery analysis.
//!
//! Sibling to [`super::state_first`]: walks every `Tail::Dispatch` (top-
//! level *and* nested inside `Star`/`Opt` arm bodies) and decides
//! which `look[0]` kinds the arm could fold in if its first token were
//! treated as silently inserted. The output is stamped into the
//! `Tail::Dispatch::insertions` field by [`stamp`] so every backend
//! reads pre-rendered data instead of re-deriving per-arm continuation
//! FIRST sets.
//!
//! Each [`Insertion`] knows the kinds to match, the message to emit,
//! and a [`PostFirst`] disposition that maps directly to one of three
//! `cur = …` recipes. Backends pick the branch and emit identical
//! control flow.
//!
//! ## Unified recovery model
//!
//! Two failure sites can produce a missing-token error: `Cursor::expect`
//! (one expected kind) and a `Tail::Dispatch` arm (one expected kind
//! per arm). Both observe the same mental model:
//!
//! 1. **Insertion** — if `look[0]` is already a valid continuation past
//!    the missing token, emit `Error("expected X")`, advance to the
//!    post-first state, and *don't* arm recovery. The lookahead is
//!    untouched and the surrounding rule keeps making progress.
//! 2. **Deletion** — otherwise, emit the error, arm `recover_to(SYNC)`,
//!    and let the runtime consume tokens as `Garbage` until `look[0]`
//!    lands in the rule's FOLLOW.
//!
//! The two sites differ only in *which* "valid continuation" set they
//! consult: `Expect` uses the rule's SYNC (= FOLLOW + EOF) as a
//! cheap proxy because there's only one possible insertion; a
//! `Dispatch` has to disambiguate among arms, so each arm carries its
//! own per-arm "post-first FIRST" derived from the destination state.

use std::collections::{BTreeMap, BTreeSet};

use crate::lowering::{Body, DispatchLeaf, DispatchTree, Instr, StateId, StateTable, Tail};

/// One arm's worth of insertion-recovery info.
///
/// Built from a `Tail::Dispatch` arm body that begins with an
/// `Instr::Expect`. The arm is a candidate when its body's tail is a
/// pure transfer (`Tail::Jump` or `Tail::Ret`) — anything else after
/// an `Expect` violates the lowering invariant, so this filter is
/// total in practice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insertion {
    /// Pretty token name baked into the error message (e.g. `"GT"`).
    /// Backends emit this verbatim inside `errorHere("expected …")`.
    pub token_name: String,
    /// `look[0]` kinds for which this arm's body could continue past
    /// the missing first token. Sorted ascending; never empty (the
    /// builder discards arms whose post-first FIRST is empty).
    pub kinds_to_match: Vec<u16>,
    /// What `cur` should become after the synthetic error event.
    pub post_first: PostFirst,
}

/// How a backend should advance `cur` after the synthetic error.
///
/// Three variants, each tied to a single `cur = …` recipe:
///
/// | variant       | render                     |
/// |---------------|----------------------------|
/// | `Goto(N)`     | `cur = N`                  |
/// | `PushAndGoto` | `pushRet(push); cur = jump`|
/// | `Return`      | `cur = popRet()`           |
///
/// They cover the four `(arm body tail, dispatch cont)` combinations,
/// with `Goto` doubling for both the no-push case (`Jump`-tail / no
/// cont) *and* the push-and-cancel case (`Ret`-tail / `Some(cont)`),
/// since the wrapper's `pushRet(cont)` is popped by the body's `Ret`
/// and the runtime sees a plain `cur = cont`.
///
/// | body tail | dispatch cont | variant                       |
/// |-----------|---------------|-------------------------------|
/// | `Jump(N)` | `None`        | `Goto(N)`                     |
/// | `Jump(N)` | `Some(C)`     | `PushAndGoto { push: C, jump: N }` |
/// | `Ret`     | `Some(C)`     | `Goto(C)`                     |
/// | `Ret`     | `None`        | `Return`                      |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostFirst {
    /// `cur = state`. Direct transition with no stack effect.
    Jump(StateId),
    /// `pushRet(push); cur = jump`. Push a continuation, then jump.
    PushRetAndJump { push: StateId, jump: StateId },
    /// `cur = popRet()`. Tail-return into the caller's stack top.
    Ret,
}

/// Stamp insertion candidates onto every `Tail::Dispatch` in `st`.
/// Walks state bodies recursively, including nested bodies inside
/// `Star`/`Opt` arms — those host their own dispatches and need their
/// own insertion analysis.
pub fn stamp(st: &mut StateTable, state_firsts: &BTreeMap<StateId, BTreeSet<u16>>) {
    let host_rules: BTreeMap<StateId, String> = st
        .states
        .iter()
        .map(|(id, s)| (*id, s.rule.clone()))
        .collect();
    let rule_follows: BTreeMap<String, BTreeSet<u16>> = st
        .rule_sync
        .iter()
        .filter_map(|(name, sid)| {
            st.sync_sets
                .get(*sid as usize)
                .map(|set| (name.clone(), set.kinds.iter().copied().collect()))
        })
        .collect();

    for (id, state) in st.states.iter_mut() {
        let host_rule = host_rules.get(id).cloned().unwrap_or_default();
        stamp_body(&mut state.body, &host_rule, state_firsts, &rule_follows);
    }
}

fn stamp_body(
    body: &mut Body,
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
) {
    match &mut body.tail {
        Tail::Jump(_) | Tail::Ret => {}
        Tail::Star { body, .. } | Tail::Opt { body, .. } => {
            stamp_body(body, host_rule, state_firsts, rule_follows);
        }
        Tail::Dispatch {
            tree,
            cont,
            insertions,
            ..
        } => {
            *insertions = collect_for_tree(host_rule, state_firsts, rule_follows, tree, *cont);
            // Recurse into arm bodies — an arm body can itself host
            // a `Tail::Dispatch` (e.g. an alt nested under another
            // alt), and each one needs its own insertions stamped.
            stamp_dispatch_subtrees(tree, host_rule, state_firsts, rule_follows);
        }
    }
}

fn stamp_dispatch_subtrees(
    tree: &mut DispatchTree,
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => {
            stamp_body(b, host_rule, state_firsts, rule_follows);
        }
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            for (_, sub) in arms {
                stamp_dispatch_subtrees(sub, host_rule, state_firsts, rule_follows);
            }
            if let DispatchLeaf::Arm(b) = default {
                stamp_body(b, host_rule, state_firsts, rule_follows);
            }
        }
    }
}

fn collect_for_tree(
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    tree: &DispatchTree,
    cont: Option<StateId>,
) -> Vec<Insertion> {
    // Insertion-recovery only applies at the top-level Switch (depth
    // 0). Deeper Switches disambiguate by `look[i>0]`, but the missing
    // token we're proposing to insert is `look[0]` itself, so the
    // ambient `look[0]` was already pinned by the outer Switch's
    // primary match.
    let DispatchTree::Switch { depth: 0, arms, .. } = tree else {
        return Vec::new();
    };
    arms.iter()
        .filter_map(|(_kind, sub)| extract(host_rule, state_firsts, rule_follows, sub, cont))
        .collect()
}

fn extract(
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    sub: &DispatchTree,
    cont: Option<StateId>,
) -> Option<Insertion> {
    let body = match sub {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => b,
        // LL(>1) sub-tree, Fallthrough, or Error leaf: no clean
        // single-token insertion target.
        _ => return None,
    };

    // First Expect anchors the missing token's identity; anything
    // before it (Enter/Exit/PushRet) doesn't read lookahead.
    let token_name = body.instrs.iter().find_map(|i| match i {
        Instr::Expect { token_name, .. } => Some(token_name.clone()),
        _ => None,
    })?;

    let post_first = match (&body.tail, cont) {
        (Tail::Jump(n), Some(c)) => PostFirst::PushRetAndJump { push: c, jump: *n },
        (Tail::Jump(n), None) => PostFirst::Jump(*n),
        // `pushRet(c); …; ret` cancels back to `cur = c`, so the
        // structural Ret-tail-with-cont collapses into the same
        // single-state goto as a Jump-tail.
        (Tail::Ret, Some(c)) => PostFirst::Jump(c),
        (Tail::Ret, None) => PostFirst::Ret,
        // Star/Opt/Dispatch as arm tail: the runtime invariant
        // forbids these after an Expect, so an arm with this shape
        // has no Expect to insert.
        _ => return None,
    };

    let kinds: BTreeSet<u16> = match &post_first {
        PostFirst::Jump(n) => state_firsts.get(n).cloned().unwrap_or_default(),
        PostFirst::PushRetAndJump { jump: n, .. } => {
            state_firsts.get(n).cloned().unwrap_or_default()
        }
        PostFirst::Ret => rule_follows.get(host_rule).cloned().unwrap_or_default(),
    };
    // EOF (kind 0) stays in `kinds`. Mid-rule EOF lookahead is
    // exactly the case the user-facing "expected X" message helps
    // most with — letting the dispatch pick the arm that exits the
    // rule cleanly produces a precise error rather than the
    // catch-all "unexpected token" the deletion fallback would
    // emit. The runtime's EOF gate only fires after `TERMINATED`,
    // so insertion-recovery here doesn't double-report.

    if kinds.is_empty() {
        return None;
    }

    Some(Insertion {
        token_name,
        kinds_to_match: kinds.into_iter().collect(),
        post_first,
    })
}
