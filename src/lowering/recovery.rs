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

use crate::lowering::{Body, DispatchLeaf, DispatchTree, StateId, StateTable, Tail};

/// One equivalence class of look-ahead kinds + the recovery payload
/// the dispatch should fire for that class.
///
/// The dispatch's lookahead space is partitioned by "which arms'
/// continuation FIRST contains this kind?" — different classes can
/// take different `post_first` transitions because their winning arm
/// differs. `candidate_kinds` is *the same on every entry* of a
/// given dispatch though: it enumerates every primary kind the
/// dispatch accepts, independent of the specific look-ahead that
/// triggered recovery. That way an empty JSON document recovers
/// reporting all of `{`, `[`, STRING, NUMBER, `true`, `false`,
/// `null` rather than only the subset of arms whose continuation
/// happens to accept EOF.
///
/// Display rendering — turning the `u16` kinds into a user-facing
/// "`expected X | Y | Z`" string — is a codegen concern and lives
/// in [`crate::codegen::common::expected_message`].
///
/// `kinds_to_match` sets are disjoint across entries of the same
/// dispatch, so the codegen-emitted `if`-chain is mutually exclusive
/// regardless of order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insertion {
    /// Token-kind ids of every arm of the host dispatch. Same on
    /// every entry of a given dispatch. Backends pass this to
    /// [`crate::codegen::common::expected_message`] to render the
    /// `errorHere` argument.
    pub candidate_kinds: Vec<u16>,
    /// `look[0]` kinds in this equivalence class. Sorted ascending;
    /// disjoint with every other entry's set in the same dispatch.
    pub kinds_to_match: Vec<u16>,
    /// What `cur` should become after the synthetic error event.
    /// Picked from the lowest-indexed arm whose continuation FIRST
    /// covers `kinds_to_match` — that's the arm whose post-first
    /// state will actually accept the current lookahead.
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

/// Per-arm extraction result, before the partition pass merges arms
/// that share lookaheads.
struct ArmCandidate {
    primary_kind: u16,
    kinds: BTreeSet<u16>,
    post_first: PostFirst,
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

    let arm_data: Vec<ArmCandidate> = arms
        .iter()
        .filter_map(|(primary_kind, sub)| {
            extract(*primary_kind, host_rule, state_firsts, rule_follows, sub, cont)
        })
        .collect();
    if arm_data.is_empty() {
        return Vec::new();
    }
    partition(&arm_data)
}

/// Partition the lookahead by "which arms' continuation FIRST cover
/// it". Each unique covering set becomes one [`Insertion`]; the
/// `post_first` transition is taken from the lowest-indexed arm in
/// the set (the only one guaranteed to accept the current lookahead
/// at its post-first state). The `candidate_kinds` list is *every*
/// dispatch arm's primary kind — independent of the partition — so
/// the error message rendered downstream reads as the full menu of
/// alternatives the user could have typed at this position rather
/// than the subset whose continuation happens to also accept the
/// current lookahead.
fn partition(arms: &[ArmCandidate]) -> Vec<Insertion> {
    let candidate_kinds: Vec<u16> = arms.iter().map(|a| a.primary_kind).collect();
    // Insertion only runs in the dispatch's `_ =>` default branch —
    // any `look[0]` that *is* a primary kind has already been routed
    // by the outer `match`. Dropping primary kinds from
    // `kinds_to_match` keeps the codegen-emitted `if`-chain free of
    // dead branches like `if look0 == LBRACE { … }` for a dispatch
    // whose `LBRACE` arm already fires through the primary path.
    let primary: BTreeSet<u16> = candidate_kinds.iter().copied().collect();

    // For each kind, list the indices of arms that cover it (in
    // declaration order — `arms` iterates in the dispatch's source
    // order via the BTreeMap-backed dispatch tree).
    let mut covers: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (idx, arm) in arms.iter().enumerate() {
        for k in &arm.kinds {
            if primary.contains(k) {
                continue;
            }
            covers.entry(*k).or_default().push(idx);
        }
    }

    // Group kinds by their cover signature.
    let mut groups: BTreeMap<Vec<usize>, Vec<u16>> = BTreeMap::new();
    for (kind, idxs) in covers {
        groups.entry(idxs).or_default().push(kind);
    }

    groups
        .into_iter()
        .map(|(idxs, kinds)| Insertion {
            candidate_kinds: candidate_kinds.clone(),
            kinds_to_match: kinds,
            post_first: arms[idxs[0]].post_first,
        })
        .collect()
}

fn extract(
    primary_kind: u16,
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    sub: &DispatchTree,
    cont: Option<StateId>,
) -> Option<ArmCandidate> {
    let body = match sub {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => b,
        // LL(>1) sub-tree, Fallthrough, or Error leaf: no clean
        // single-token insertion target.
        _ => return None,
    };

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

    Some(ArmCandidate {
        primary_kind,
        kinds,
        post_first,
    })
}
