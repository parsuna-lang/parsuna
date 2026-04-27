//! Post-layout clean-ups: splice deterministic jump chains into their
//! predecessors, then drop any state no entry point can reach.
//!
//! Splicing turns `A -> B -> C -> D` (all unconditional jumps) into a
//! single `A -> D` fall-through. There are two regimes:
//!
//! * **Single-predecessor splice** — the target is referenced from
//!   exactly one place (the Jump we're following). Inlining moves the
//!   target's ops into the predecessor and leaves the target dead, so
//!   `eliminate_dead` drops it. No duplication. Unbounded depth, and
//!   the target's first op may be branchy (`Opt`/`Dispatch`) because no
//!   other caller will be confused by the move.
//!
//! * **Multi-predecessor splice** — the target has other callers, so
//!   inlining duplicates its ops at the call site while keeping the
//!   original. Bounded by [`DUPLICATION_BUDGET`] to keep generated
//!   files reasonable, and gated to non-branchy first ops because
//!   duplicating a branchy entry into one caller would lose the
//!   shared dispatch point that other callers rely on.
//!
//! Entry states are never inlined-and-deleted because they need to
//! remain as a real dispatch target the runtime can name. Reference
//! counting is *external* — a state's self-references (e.g. an
//! `Op::Star` whose `head` field points back to its containing state)
//! don't count against the single-predecessor check. After splicing,
//! a Star op moves with its `head` field intact, so the original loop
//! head stays alive (it's still referenced from the new home of the
//! Star) and the loop continues to land at the same id between
//! iterations.

use std::collections::{BTreeMap, HashSet, VecDeque};

use crate::lowering::{Body, DispatchLeaf, DispatchTree, Op, StateId, StateTable};

/// Max chain depth to absorb when each step duplicates the target's
/// ops (i.e. the target has multiple predecessors). Bounds generated
/// file size for chains that would otherwise expand multiplicatively.
pub const DUPLICATION_BUDGET: usize = 6;

/// Splice straight-line jump chains, eliminate tail-call trampolines,
/// mark single-predecessor branch targets for inline emission, then drop
/// every state no entry can reach.
///
/// The four sub-passes feed each other:
///
/// * Splicing creates the trampolines TCE eliminates (a state that
///   ends in a "call" pattern where the saved continuation has been
///   collapsed down to a single `Ret`).
/// * TCE rewrites `cont: Some(s) → None`, which removes `s` from the
///   reachability set so it gets dropped.
/// * Branch inlining removes the dispatch hop on the *match* path of
///   single-predecessor `Opt`/`Star` bodies and `Dispatch` arms,
///   which can in turn shrink the reference counts of those bodies'
///   own targets.
///
/// The whole pipeline runs to a fixpoint — repeating until a pass
/// produces no changes — because any of those rewrites can expose new
/// opportunities for the others.
pub fn fuse(table: &mut StateTable) {
    loop {
        let snapshot = ops_snapshot(table);
        splice_chains(table);
        eliminate_tail_pushes(table);
        inline_branch_targets(table);
        eliminate_dead(table);
        if ops_snapshot(table) == snapshot {
            break;
        }
    }
}

/// Snapshot every reachable state's ops keyed by id. Used by [`fuse`]'s
/// fixpoint loop to detect when the IR has stopped changing.
fn ops_snapshot(table: &StateTable) -> BTreeMap<StateId, Vec<Op>> {
    table
        .states
        .iter()
        .map(|(id, s)| (*id, s.ops.clone()))
        .collect()
}

fn splice_chains(table: &mut StateTable) {
    let snapshot: BTreeMap<StateId, Vec<Op>> = table
        .states
        .iter()
        .map(|(id, s)| (*id, s.ops.clone()))
        .collect();
    let ref_counts = count_external_refs(&snapshot);
    let entry_ids: HashSet<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();
    for state in table.states.values_mut() {
        state.ops = fuse_ops(&snapshot, &ref_counts, &entry_ids, state.id);
    }
}

/// Tally references to each state from *other* states. Self-references
/// (e.g. an `Op::Star` whose `head` points back to its containing state)
/// don't count: they travel with the op when the op is spliced, so they
/// don't pin the original state in place. A state with an external-ref
/// count of 1 has exactly one foreign caller, and splicing into that
/// caller's Jump leaves no other transition into the original.
fn count_external_refs(snapshot: &BTreeMap<StateId, Vec<Op>>) -> BTreeMap<StateId, usize> {
    let mut counts: BTreeMap<StateId, usize> = BTreeMap::new();
    for (source, ops) in snapshot {
        for op in ops {
            for target in op_targets(op) {
                if target != *source {
                    *counts.entry(target).or_default() += 1;
                }
            }
        }
    }
    counts
}

fn fuse_ops(
    snapshot: &BTreeMap<StateId, Vec<Op>>,
    ref_counts: &BTreeMap<StateId, usize>,
    entry_ids: &HashSet<StateId>,
    start_id: StateId,
) -> Vec<Op> {
    let mut ops = snapshot.get(&start_id).cloned().unwrap_or_default();
    // Guard against a jump chain that loops back on itself.
    let mut visited: HashSet<StateId> = HashSet::new();
    visited.insert(start_id);
    let mut duplications_used = 0usize;
    loop {
        // Only chains ending in `Jump` are splice candidates — anything
        // else (Ret, Expect, Dispatch, …) already has semantic effect we
        // must preserve at this state boundary.
        let target = match ops.last() {
            Some(Op::Jump(t)) => *t,
            _ => break,
        };
        if !visited.insert(target) {
            break;
        }
        let Some(target_ops) = snapshot.get(&target) else {
            break;
        };
        if target_ops.is_empty() {
            break;
        }
        // The Jump itself accounts for one external reference; anything
        // beyond that means another caller still needs the original
        // state, so splicing here duplicates the target's body.
        let total_refs = ref_counts.get(&target).copied().unwrap_or(0);
        let is_entry = entry_ids.contains(&target);
        // Single-predecessor splice: free code motion. Allowed even when
        // the target's first op is branchy, because no other predecessor
        // depends on the shared entry — there isn't one.
        let single_ref = total_refs <= 1 && !is_entry;
        if !single_ref {
            // Multi-ref or entry: would duplicate the target's body. Gate
            // on a non-branchy first op (so we don't splinter a shared
            // dispatch point) and on the duplication budget.
            let branchy_first = matches!(
                target_ops.first(),
                Some(Op::Star { .. }) | Some(Op::Opt { .. }) | Some(Op::Dispatch { .. })
            );
            if branchy_first {
                break;
            }
            if duplications_used >= DUPLICATION_BUDGET {
                break;
            }
            duplications_used += 1;
        }
        ops.pop();
        ops.extend(target_ops.iter().cloned());
    }
    ops
}

/// Eliminate "trampoline" pushes whose target is a pure-`Ret` state.
///
/// Four patterns get optimized, all of which boil down to "the
/// continuation we'd push or jump to is a single `Ret`, so the called
/// code's trailing `Ret` may as well pop our caller directly":
///
/// 1. **Explicit [`Op::PushRet(B)`]** where `B = [Op::Ret]` — the push
///    is dropped outright. Typical post-splice shape:
///
///    ```text
///    state A: [PushRet(B), Enter(R), Expect(...), Jump(C)]
///    state B: [Ret]
///    ```
///
/// 2. **[`Op::Opt`] whose `cont` is `[Op::Ret]`** — the codegen would
///    have emitted `push_ret(cont); cur = body` on the matched path
///    and `cur = cont` on the miss path. After this pass `cont` is
///    `None`, and the backends emit `cur = body` (no push) on match
///    and `cur = ret()` on miss.
///
/// 3. **[`Op::Dispatch`] whose `cont` is `[Op::Ret]`** — same shape as
///    `Opt`, applied across every `Arm` leaf and the
///    `Fallthrough`/`Error` recovery paths.
///
/// 4. **[`Op::Star`] whose `cont` is `[Op::Ret]`** — only the miss /
///    fall-through path is rewritten (the `head` push that runs every
///    matched iteration can never be a trampoline because `head` is
///    the state holding this very `Star` op). Backends emit
///    `cur = ret()` on miss instead of `cur = cont`.
///
/// Safety:
/// * The trampoline state must be exactly `[Op::Ret]`. Any other op
///   (including `Enter`/`Exit`/`Expect`) is observable.
/// * Entry states are never optimized away — they must remain
///   callable from outside the dispatch.
///
/// `eliminate_dead` runs immediately after this pass and drops any
/// trampoline states that became unreferenced.
fn eliminate_tail_pushes(table: &mut StateTable) {
    let entry_ids: HashSet<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();
    let return_only: HashSet<StateId> = table
        .states
        .iter()
        .filter(|(id, s)| !entry_ids.contains(id) && matches!(s.ops.as_slice(), [Op::Ret]))
        .map(|(id, _)| *id)
        .collect();
    if return_only.is_empty() {
        return;
    }
    for state in table.states.values_mut() {
        // Pattern 1: drop explicit PushRets to trampolines.
        state.ops.retain(|op| match op {
            Op::PushRet(b) => !return_only.contains(b),
            _ => true,
        });
        // Patterns 2, 3 & 4: flip Opt/Dispatch/Star's `cont` to `None`
        // when the continuation is a trampoline. Backends pattern-match
        // on `cont` and emit either push-and-jump (or `cur = next` for
        // Star's miss path) or a direct tail-`ret()`.
        for op in state.ops.iter_mut() {
            let cont = match op {
                Op::Opt { cont, .. } | Op::Dispatch { cont, .. } | Op::Star { cont, .. } => cont,
                _ => continue,
            };
            if let Some(s) = *cont {
                if return_only.contains(&s) {
                    *cont = None;
                }
            }
        }
    }
}

/// Move single-predecessor branch-target states into the calling
/// branchy op as a [`Body::Inline`], saving a dispatch hop.
///
/// The targets we consider are the `body` of `Op::Opt` and `Op::Star`
/// and the `Body::State` inside [`DispatchLeaf::Arm`]s. Splice and
/// TCE handle `Jump`/`PushRet` chains; this pass handles the
/// *branchy* paths they don't touch.
///
/// Eligibility (mirrors the single-predecessor regime in
/// [`splice_chains`]): the target's external reference count must be
/// exactly 1 — meaning the only place that mentions it is *this*
/// branch — and it must not be a public entry. After the move, the
/// target state goes unreferenced and `eliminate_dead` drops it on
/// the same pass.
///
/// Run repeatedly by [`fuse`]'s fixpoint loop: inlining can shrink
/// other states' refcounts (a body B that referenced C is now
/// referenced from A's `Body::Inline` view of B; C's inbound count
/// from B drops to 0), exposing further inlining opportunities.
fn inline_branch_targets(table: &mut StateTable) {
    let snap_ops: BTreeMap<StateId, Vec<Op>> = table
        .states
        .iter()
        .map(|(id, s)| (*id, s.ops.clone()))
        .collect();
    let ref_counts = count_external_refs(&snap_ops);
    let entry_ids: HashSet<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();

    // Branchy-target counts: how many `Op::Opt`/`Op::Star` bodies and
    // how many `DispatchLeaf::Arm`s currently reach each state. We only
    // inline a target whose *every* external reference is branchy —
    // otherwise splice_chains is the right tool.
    let mut branch_target_callers: BTreeMap<StateId, usize> = BTreeMap::new();
    for ops in snap_ops.values() {
        for op in ops {
            for target in branch_target_states(op) {
                *branch_target_callers.entry(target).or_default() += 1;
            }
        }
    }

    let inlinable: HashSet<StateId> = snap_ops
        .keys()
        .copied()
        .filter(|id| {
            !entry_ids.contains(id)
                && ref_counts.get(id).copied().unwrap_or(0) == 1
                && branch_target_callers.get(id).copied().unwrap_or(0) == 1
        })
        .collect();
    if inlinable.is_empty() {
        return;
    }

    // Walk every op and rewrite Body::State(s) to Body::Inline(s.ops)
    // when s is inlinable. We snapshot ops up-front so the source of
    // an inlined body comes from the pre-rewrite version (a body that
    // itself contains another inlinable target gets resolved on the
    // *next* fixpoint iteration, not this one — keeps the rewrite
    // simple and avoids order-dependence).
    for state in table.states.values_mut() {
        for op in state.ops.iter_mut() {
            inline_in_op(op, &snap_ops, &inlinable);
        }
    }
}

fn inline_in_op(op: &mut Op, snap_ops: &BTreeMap<StateId, Vec<Op>>, inlinable: &HashSet<StateId>) {
    match op {
        Op::Opt { body, .. } | Op::Star { body, .. } => maybe_inline_body(body, snap_ops, inlinable),
        Op::Dispatch { tree, .. } => inline_in_tree(tree, snap_ops, inlinable),
        _ => {}
    }
}

fn inline_in_tree(
    tree: &mut DispatchTree,
    snap_ops: &BTreeMap<StateId, Vec<Op>>,
    inlinable: &HashSet<StateId>,
) {
    match tree {
        DispatchTree::Leaf(leaf) => {
            if let DispatchLeaf::Arm(body) = leaf {
                maybe_inline_body(body, snap_ops, inlinable);
            }
        }
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                maybe_inline_body(body, snap_ops, inlinable);
            }
            for (_, sub) in arms {
                inline_in_tree(sub, snap_ops, inlinable);
            }
        }
    }
}

fn maybe_inline_body(
    body: &mut Body,
    snap_ops: &BTreeMap<StateId, Vec<Op>>,
    inlinable: &HashSet<StateId>,
) {
    let Body::State(s) = body else { return };
    if !inlinable.contains(s) {
        return;
    }
    let ops = snap_ops.get(s).cloned().unwrap_or_default();
    *body = Body::Inline(ops);
}

/// State ids reached as a *body* of a branchy op — `Op::Opt`/`Op::Star`'s
/// `body` and every `Arm` leaf of an `Op::Dispatch`. Only counts
/// references where the body is still a `Body::State` (a `Body::Inline`
/// no longer references its source state). Excludes `cont` and `head`,
/// which are post-branch fall-through and self-loop respectively, not
/// fresh calls.
fn branch_target_states(op: &Op) -> Vec<StateId> {
    match op {
        Op::Opt { body, .. } | Op::Star { body, .. } => body.state().into_iter().collect(),
        Op::Dispatch { tree, .. } => {
            let mut out = Vec::new();
            collect_arm_states(tree, &mut out);
            out
        }
        _ => Vec::new(),
    }
}

fn collect_arm_states(tree: &DispatchTree, out: &mut Vec<StateId>) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => {
            if let Some(s) = b.state() {
                out.push(s);
            }
        }
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(b) = default {
                if let Some(s) = b.state() {
                    out.push(s);
                }
            }
            for (_, sub) in arms {
                collect_arm_states(sub, out);
            }
        }
    }
}

fn eliminate_dead(table: &mut StateTable) {
    let mut reachable: HashSet<StateId> = HashSet::new();
    let mut queue: VecDeque<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();
    while let Some(id) = queue.pop_front() {
        if !reachable.insert(id) {
            continue;
        }
        let Some(state) = table.states.get(&id) else {
            continue;
        };
        for op in &state.ops {
            for target in op_targets(op) {
                if !reachable.contains(&target) {
                    queue.push_back(target);
                }
            }
        }
    }
    table.states.retain(|id, _| reachable.contains(id));
}

fn op_targets(op: &Op) -> Vec<StateId> {
    let mut out = Vec::new();
    collect_op_targets(op, &mut out);
    out
}

fn collect_op_targets(op: &Op, out: &mut Vec<StateId>) {
    match op {
        Op::PushRet(n) | Op::Jump(n) => out.push(*n),
        Op::Ret | Op::Enter(_) | Op::Exit(_) | Op::Expect { .. } => {}
        // `head` keeps the original loop-head state alive when the Star
        // op gets spliced into another state. `cont` is the post-loop
        // fall-through; `None` means tail call (the `Op::Ret` chain
        // shortcuts straight to the caller). `body` may be inlined —
        // recurse into its ops in that case so any state ids they
        // reference still count as reachable.
        Op::Star {
            body, cont, head, ..
        } => {
            collect_body_targets(body, out);
            if let Some(c) = cont {
                out.push(*c);
            }
            out.push(*head);
        }
        // Tail-flavoured Opt/Dispatch (`cont = None`) don't transition
        // through any continuation state — codegen emits `Ret` instead
        // of `Jump(cont)`. Excluding the missing target here lets
        // `eliminate_dead` drop the now-orphan trampoline state.
        Op::Opt { body, cont, .. } => {
            collect_body_targets(body, out);
            if let Some(c) = cont {
                out.push(*c);
            }
        }
        Op::Dispatch { tree, cont, .. } => {
            if let Some(c) = cont {
                out.push(*c);
            }
            collect_tree_targets(tree, out);
        }
    }
}

fn collect_body_targets(body: &Body, out: &mut Vec<StateId>) {
    match body {
        Body::State(s) => out.push(*s),
        // Inlined bodies don't reference the original state any more,
        // but their ops may transition to states we still need to keep
        // alive — walk them.
        Body::Inline(ops) => {
            for op in ops {
                collect_op_targets(op, out);
            }
        }
    }
}

fn collect_tree_targets(tree: &DispatchTree, out: &mut Vec<StateId>) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => collect_body_targets(b, out),
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(b) = default {
                collect_body_targets(b, out);
            }
            for (_, sub) in arms {
                collect_tree_targets(sub, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::grammar::parse_grammar;
    use crate::lowering::lexer_dfa::DfaState;
    use crate::lowering::{lower, State, StateTable};

    fn analyze_src(src: &str) -> crate::AnalyzedGrammar {
        let g = parse_grammar(src).expect("parse");
        let outcome = analyze(g);
        assert!(!outcome.has_errors(), "{:?}", outcome.diagnostics);
        outcome.grammar.expect("grammar")
    }

    fn empty_dfa() -> Vec<DfaState> {
        vec![DfaState {
            id: 0,
            arms: vec![],
            accept: None,
            self_loop: vec![],
        }]
    }

    fn make_state(id: StateId, ops: Vec<Op>) -> State {
        State {
            id,
            label: format!("s{}", id),
            ops,
        }
    }

    fn empty_table_with(states: BTreeMap<StateId, State>, entry: StateId) -> StateTable {
        StateTable {
            grammar_name: "x".into(),
            tokens: vec![],
            rule_kinds: vec![],
            first_sets: vec![],
            sync_sets: vec![],
            states,
            entry_states: vec![("main".into(), entry)],
            k: 1,
            queue_cap: 0,
            lexer_dfa: empty_dfa(),
        }
    }

    #[test]
    fn duplication_budget_is_positive() {
        assert!(DUPLICATION_BUDGET > 0);
    }

    #[test]
    fn tail_push_to_ret_only_state_is_stripped() {
        // 1 calls 5: pushes 6 (a pure-Ret trampoline) and jumps. After
        // elimination 1 should not push 6 anymore, and 6 should be gone.
        let mut states = BTreeMap::new();
        states.insert(
            1,
            make_state(1, vec![Op::PushRet(6), Op::Enter(0), Op::Jump(5)]),
        );
        states.insert(5, make_state(5, vec![Op::Exit(0), Op::Ret]));
        states.insert(6, make_state(6, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        fuse(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        // The PushRet(6) has been stripped.
        assert!(
            !s1_ops.iter().any(|op| matches!(op, Op::PushRet(6))),
            "{:?}",
            s1_ops
        );
        // State 6 was unreferenced after the strip, so DCE drops it.
        assert!(
            !table.states.contains_key(&6),
            "trampoline state 6 still present"
        );
    }

    #[test]
    fn tail_push_does_not_strip_when_target_is_an_entry() {
        // Entry states must remain callable even if their only role is Ret.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::PushRet(2), Op::Jump(3)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        // Mark 2 as a public entry.
        table.entry_states.push(("alt".into(), 2));
        fuse(&mut table);
        // PushRet(2) survives because 2 is an entry.
        assert!(
            table
                .states
                .get(&1)
                .unwrap()
                .ops
                .iter()
                .any(|op| matches!(op, Op::PushRet(2))),
            "{:?}",
            table.states.get(&1).unwrap().ops
        );
        assert!(table.states.contains_key(&2));
    }

    #[test]
    fn tail_push_does_not_strip_when_target_does_real_work() {
        // Op::Exit is observable and can't be skipped, so a [Exit, Ret]
        // state is NOT a pure trampoline.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::PushRet(2), Op::Jump(3)]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        fuse(&mut table);
        assert!(table
            .states
            .get(&1)
            .unwrap()
            .ops
            .iter()
            .any(|op| matches!(op, Op::PushRet(2))));
        assert!(table.states.contains_key(&2));
    }

    #[test]
    fn single_pred_opt_body_is_inlined() {
        // 1's only op is an Opt with body=5. State 5 has one external
        // reference (1's Opt), isn't an entry, and contains real work
        // (Enter+Expect+Ret). The inline pass moves its ops into 1's
        // Opt as `Body::Inline` and DCE drops 5.
        let opt = Op::Opt {
            first: 0,
            body: Body::State(5),
            cont: Some(2),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![opt]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        let body_ops = vec![
            Op::Enter(0),
            Op::Expect {
                kind: 1,
                token_name: "X".into(),
                sync: 0,
            },
            Op::Ret,
        ];
        states.insert(5, make_state(5, body_ops.clone()));
        let mut table = empty_table_with(states, 1);
        fuse(&mut table);
        let s1 = table.states.get(&1).unwrap();
        assert!(
            matches!(
                s1.ops.last(),
                Some(Op::Opt {
                    body: Body::Inline(_),
                    ..
                })
            ),
            "{:?}",
            s1.ops
        );
        if let Some(Op::Opt { body: Body::Inline(ops), .. }) = s1.ops.last() {
            assert_eq!(ops, &body_ops);
        }
        assert!(
            !table.states.contains_key(&5),
            "inlined state 5 should have been dropped by DCE"
        );
    }

    #[test]
    fn multi_pred_branch_target_is_not_inlined() {
        // State 5 is referenced as the body of *two* Opts (in 1 and 3).
        // Branchy ops aren't splice candidates, so the refcount stays
        // at 2 and the inline pass leaves both bodies as Body::State(5).
        let opt1 = Op::Opt { first: 0, body: Body::State(5), cont: Some(2) };
        let opt2 = Op::Opt { first: 0, body: Body::State(5), cont: Some(4) };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![opt1]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        states.insert(3, make_state(3, vec![opt2]));
        states.insert(4, make_state(4, vec![Op::Exit(1), Op::Ret]));
        states.insert(5, make_state(5, vec![Op::Enter(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 3));
        fuse(&mut table);
        assert!(table.states.contains_key(&5));
        let s1 = table.states.get(&1).unwrap();
        assert!(matches!(
            s1.ops.last(),
            Some(Op::Opt { body: Body::State(5), .. })
        ));
    }

    #[test]
    fn tail_push_rewrites_star_cont_to_none() {
        // 1 holds a Star with cont=2 and head=1; 2 is a pure-Ret
        // trampoline. Tail-call elimination should rewrite cont to
        // None so the loop's miss path emits `cur = ret()` directly.
        let star = Op::Star {
            first: 0,
            body: Body::State(50),
            cont: Some(2),
            head: 1,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![star]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(50, make_state(50, vec![Op::Exit(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
        fuse(&mut table);
        // cont became None.
        let s1 = table.states.get(&1).unwrap();
        assert!(
            matches!(
                s1.ops.last(),
                Some(Op::Star {
                    cont: None,
                    head: 1,
                    ..
                })
            ),
            "{:?}",
            s1.ops
        );
        // The trampoline state was unreferenced after the rewrite, so DCE drops it.
        assert!(
            !table.states.contains_key(&2),
            "trampoline state 2 still present"
        );
    }

    #[test]
    fn fuse_real_lowering_reaches_fixpoint_and_keeps_entries() {
        let ag = analyze_src("T = \"t\"; main = T;");
        let st = lower(&ag);
        for (_, id) in &st.entry_states {
            assert!(st.states.contains_key(id), "entry {} missing post-fuse", id);
        }
    }

    #[test]
    fn eliminate_dead_drops_states_no_entry_can_reach() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        fuse(&mut table);
        assert!(table.states.contains_key(&1));
        assert!(!table.states.contains_key(&99));
    }

    #[test]
    fn splice_chains_absorbs_jump_chain_into_first_state() {
        // 1 -> Jump(2); 2 -> Jump(3); 3 -> Ret.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Jump(3)]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        splice_chains(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(matches!(s1_ops.last(), Some(Op::Ret)), "{:?}", s1_ops);
    }

    #[test]
    fn single_ref_branchy_target_is_inlined() {
        // 1 -> Jump(2); 2 starts with Dispatch but is referenced only from 1.
        // The relaxed splicer inlines unconditionally.
        let dispatch = Op::Dispatch {
            tree: DispatchTree::Leaf(DispatchLeaf::Fallthrough),
            sync: 0,
            cont: Some(99),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![dispatch]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        splice_chains(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(
            matches!(s1_ops.last(), Some(Op::Dispatch { .. })),
            "{:?}",
            s1_ops
        );
    }

    #[test]
    fn multi_ref_branchy_target_is_left_alone() {
        // 1 -> Jump(2); 3 -> Jump(2); 2 starts with Dispatch.
        // Two predecessors means inlining would duplicate the dispatch —
        // splicer must bail on the branchy first op for both callers.
        let dispatch = || Op::Dispatch {
            tree: DispatchTree::Leaf(DispatchLeaf::Fallthrough),
            sync: 0,
            cont: Some(99),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![dispatch()]));
        states.insert(3, make_state(3, vec![Op::Jump(2)]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        // Mark 3 reachable too so eliminate_dead doesn't prune it before
        // we can inspect.
        table.entry_states.push(("alt".into(), 3));
        splice_chains(&mut table);
        assert!(matches!(
            table.states.get(&1).unwrap().ops.last(),
            Some(Op::Jump(2))
        ));
        assert!(matches!(
            table.states.get(&3).unwrap().ops.last(),
            Some(Op::Jump(2))
        ));
    }

    #[test]
    fn single_ref_star_target_is_inlined_and_head_preserved() {
        // 1 -> [Enter(0); Jump(2)]; 2 -> [Star{head: 2}]. State 2 has one
        // external Jump-ref (from 1) plus a self-ref via head, so it splices
        // into 1. The Star's `head` keeps pointing at 2, so 2 stays alive
        // (eliminate_dead can reach it via the spliced Star), and the loop
        // re-evaluation lands at 2 instead of re-running 1's prologue.
        // 99 isn't a pure-Ret here (we make it Exit + Ret) so the Star's
        // `cont` doesn't get tail-call-rewritten — that would distract
        // from what this test is checking.
        // 50 is the Star body. Make it a non-trivial state so branch
        // inlining doesn't fold it into the Star (this test is about
        // splice + head-preservation, not branch inlining).
        let star = Op::Star {
            first: 0,
            body: Body::State(50),
            cont: Some(99),
            head: 2,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Enter(0), Op::Jump(2)]));
        states.insert(2, make_state(2, vec![star]));
        // Two callers (a synthetic side-entry + the Star body) so
        // branch inlining sees 50 as multi-pred and leaves it alone.
        states.insert(50, make_state(50, vec![Op::Exit(0), Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Exit(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("body_alt".into(), 50));
        fuse(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        // Star spliced into state 1.
        assert!(
            matches!(s1_ops.last(), Some(Op::Star { head: 2, .. })),
            "{:?}",
            s1_ops
        );
        // State 2 remains because the spliced Star references it via head.
        assert!(table.states.contains_key(&2), "head target dropped");
    }

    #[test]
    fn entry_state_target_is_not_inlined_even_when_single_ref() {
        // 1 (entry) -> Jump(2); 2 (also entry) -> Ret. Even with one
        // textual Jump-reference, state 2 must remain because it is a
        // public entry point.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 2));
        // 2 is non-branchy so the multi-ref/branchy gate would still let
        // it splice — the entry-state guard is what saves it.
        // (Splicing into 1 is fine; the must is that 2 stays in the map.)
        fuse(&mut table);
        assert!(table.states.contains_key(&2), "entry state 2 was dropped");
    }

    #[test]
    fn splice_chains_breaks_self_loop() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(1)]));
        let mut table = empty_table_with(states, 1);
        splice_chains(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert_eq!(s1_ops.len(), 1);
    }

    #[test]
    fn duplicating_splice_respects_budget() {
        // Each intermediate state 2..=7 has [Enter(0), Jump(next)] so
        // every splice nets one extra op into state 1; state 8 ends the
        // chain with Ret. Each intermediate is also referenced by a
        // synthetic side-caller so total refs == 2 — splices count
        // against the duplication budget. With budget = 6 we expect 6
        // splices, leaving state 1's tail as Jump(8) (state 8's Ret is
        // not reached).
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        for id in 2..=7 {
            states.insert(id, make_state(id, vec![Op::Enter(0), Op::Jump(id + 1)]));
        }
        states.insert(8, make_state(8, vec![Op::Ret]));
        for (alt_id, target) in (100..).zip(2..=8) {
            states.insert(alt_id, make_state(alt_id, vec![Op::Jump(target)]));
        }
        let mut table = empty_table_with(states, 1);
        splice_chains(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        // 1 original Jump replaced by [Enter, Jump], then 5 more splices
        // each net +1 op. Total: 2 + 5 = 7 ops. Tail is still a Jump.
        assert_eq!(s1_ops.len(), 1 + DUPLICATION_BUDGET);
        assert!(matches!(s1_ops.last(), Some(Op::Jump(_))), "{:?}", s1_ops);
    }
}
