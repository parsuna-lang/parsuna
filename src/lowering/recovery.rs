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
/// differs. The user-facing `candidate_names` list is *the same on
/// every entry* of a given dispatch though: it enumerates every
/// alternative the dispatch primarily accepts (every arm's
/// first-token display name), independent of the specific look-ahead
/// that triggered recovery. That way an empty JSON document recovers
/// with `expected `{` | `[` | STRING | NUMBER | `true` | `false` | `null` ``
/// rather than naming only the subset of arms whose continuation
/// happens to accept EOF.
///
/// `kinds_to_match` sets are disjoint across entries of the same
/// dispatch, so the codegen-emitted `if`-chain is mutually exclusive
/// regardless of order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insertion {
    /// Display names of every arm of the host dispatch — i.e. every
    /// "valid first token" the user could have typed at this
    /// position. Same on every entry of a given dispatch. Backends
    /// route this through [`Insertion::expected_msg`] to build the
    /// `errorHere` argument.
    pub candidate_names: Vec<String>,
    /// `look[0]` kinds in this equivalence class. Sorted ascending;
    /// disjoint with every other entry's set in the same dispatch.
    pub kinds_to_match: Vec<u16>,
    /// What `cur` should become after the synthetic error event.
    /// Picked from the lowest-indexed arm whose continuation FIRST
    /// covers `kinds_to_match` — that's the arm whose post-first
    /// state will actually accept the current lookahead.
    pub post_first: PostFirst,
}

impl Insertion {
    /// Render `candidate_names` as the `errorHere` argument:
    /// ``expected `{` | `[` | STRING | NUMBER | `true` | `false` | `null` ``
    /// for a multi-arm dispatch, ``expected `>` `` for a single-arm
    /// one. The candidate names are already debug-escaped (they
    /// come from `build::token_display_name`), so the result drops
    /// safely into any backend's `"..."` literal — every supported
    /// language uses the same `\\` / `\"` vocabulary.
    pub fn expected_msg(&self) -> String {
        format!("expected {}", self.candidate_names.join(" | "))
    }
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
    // kind id → display name. Built upfront so each `extract` call
    // is a single map lookup; uses the same pre-rendered display
    // form (backtick-quoted literal, or the grammar-declared name)
    // that `Instr::Expect.token_name` carries — keeps Expect and
    // Dispatch error messages consistent for the same token.
    let mut display_names: BTreeMap<u16, String> = BTreeMap::new();
    display_names.insert(0, "EOF".to_string());
    for t in &st.tokens {
        display_names.insert(t.kind, t.display_name.clone());
    }

    for (id, state) in st.states.iter_mut() {
        let host_rule = host_rules.get(id).cloned().unwrap_or_default();
        stamp_body(
            &mut state.body,
            &host_rule,
            state_firsts,
            &rule_follows,
            &display_names,
        );
    }
}

fn stamp_body(
    body: &mut Body,
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    display_names: &BTreeMap<u16, String>,
) {
    match &mut body.tail {
        Tail::Jump(_) | Tail::Ret => {}
        Tail::Star { body, .. } | Tail::Opt { body, .. } => {
            stamp_body(body, host_rule, state_firsts, rule_follows, display_names);
        }
        Tail::Dispatch {
            tree,
            cont,
            insertions,
            ..
        } => {
            *insertions = collect_for_tree(
                host_rule,
                state_firsts,
                rule_follows,
                display_names,
                tree,
                *cont,
            );
            // Recurse into arm bodies — an arm body can itself host
            // a `Tail::Dispatch` (e.g. an alt nested under another
            // alt), and each one needs its own insertions stamped.
            stamp_dispatch_subtrees(tree, host_rule, state_firsts, rule_follows, display_names);
        }
    }
}

fn stamp_dispatch_subtrees(
    tree: &mut DispatchTree,
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    display_names: &BTreeMap<u16, String>,
) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => {
            stamp_body(b, host_rule, state_firsts, rule_follows, display_names);
        }
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            for (_, sub) in arms {
                stamp_dispatch_subtrees(sub, host_rule, state_firsts, rule_follows, display_names);
            }
            if let DispatchLeaf::Arm(b) = default {
                stamp_body(b, host_rule, state_firsts, rule_follows, display_names);
            }
        }
    }
}

/// Per-arm extraction result, before the partition pass merges arms
/// that share lookaheads.
struct ArmCandidate {
    token_name: String,
    kinds: BTreeSet<u16>,
    post_first: PostFirst,
}

fn collect_for_tree(
    host_rule: &str,
    state_firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    rule_follows: &BTreeMap<String, BTreeSet<u16>>,
    display_names: &BTreeMap<u16, String>,
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
            extract(
                *primary_kind,
                host_rule,
                state_firsts,
                rule_follows,
                display_names,
                sub,
                cont,
            )
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
/// at its post-first state). The user-visible `candidate_names` list
/// is *every* dispatch arm — independent of the partition — so the
/// error message reads as the full menu of alternatives the user
/// could have typed at this position rather than the subset whose
/// continuation happens to also accept the current lookahead.
fn partition(arms: &[ArmCandidate]) -> Vec<Insertion> {
    let candidate_names: Vec<String> = arms.iter().map(|a| a.token_name.clone()).collect();

    // For each kind, list the indices of arms that cover it (in
    // declaration order — `arms` iterates in the dispatch's source
    // order via the BTreeMap-backed dispatch tree).
    let mut covers: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (idx, arm) in arms.iter().enumerate() {
        for k in &arm.kinds {
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
            candidate_names: candidate_names.clone(),
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
    display_names: &BTreeMap<u16, String>,
    sub: &DispatchTree,
    cont: Option<StateId>,
) -> Option<ArmCandidate> {
    let body = match sub {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => b,
        // LL(>1) sub-tree, Fallthrough, or Error leaf: no clean
        // single-token insertion target.
        _ => return None,
    };

    // The display name comes straight from the Switch arm's primary
    // kind — works uniformly for direct-Expect arms (e.g. `STRING`,
    // `NUMBER` in a JSON `value`) and rule-call arms (e.g. `object`
    // → `{`, `array` → `[`), which would otherwise have no
    // `Instr::Expect` in their body to read the name from.
    let token_name = display_names.get(&primary_kind).cloned()?;

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
        token_name,
        kinds,
        post_first,
    })
}
