//! Post-layout optimization. Layout produces a state table with the
//! one-event-per-step invariant already satisfied (every block's last
//! op is in tail form, so no standalone `[Op::Ret]` states get
//! generated). This module's passes are *purely* about size and
//! dispatch-hop reduction; the parser is correct without them.
//!
//! Four passes, each gated by a flag in [`LoweringOpts`]:
//!
//! * [`inline_jumps`] — a state ending in `Jump(N)` absorbs `N`'s
//!   ops as long as the result still satisfies [`is_valid_body`].
//!   Multi-predecessor targets get duplicated; the original stays
//!   alive only if some other reference remains.
//!
//! * [`inline_branch_bodies`] — a body that's just `[Op::Jump(s)]`
//!   inside an `Op::Star` / `Op::Opt` / `Op::Dispatch` arm gets
//!   replaced with `s`'s ops, again under the [`is_valid_body`] gate.
//!
//! * [`fold_trampolines`] — drops `Op::PushRet(s)` whose `s` is
//!   `[Op::Ret]`-only, and rewrites `cont: Some(s) → None` for the
//!   same case. Inlining tends to expose these (a body that absorbed
//!   a tail of `Ret` becomes a fresh trampoline candidate).
//!
//! * [`eliminate_dead`] — drops every state no public entry can
//!   reach.
//!
//! The four run in a fixpoint loop because they feed each other:
//! inlining shrinks ref counts (which `eliminate_dead` then drops),
//! and dropping a state's last reference can turn neighbouring states
//! into trampolines.

use std::collections::{BTreeMap, HashSet, VecDeque};

use crate::lowering::validate::is_valid_body;
use crate::lowering::{Body, DispatchLeaf, DispatchTree, LoweringOpts, Op, StateId, StateTable};

/// Run enabled passes to a fixpoint. With every flag off the loop
/// converges in a single iteration (no change), which is fine: the
/// generated parser is correct without any optimizer pass running —
/// the runtime's `next_event` loop walks through 0-event state
/// bodies (loop heads, jump trampolines) until `step` returns an
/// event, so each optimizer pass is purely about reducing state
/// hops and table size.
pub fn optimize(table: &mut StateTable, opts: LoweringOpts) {
    loop {
        let snapshot = ops_snapshot(table);
        if opts.inline_jumps {
            inline_jumps(table);
        }
        if opts.fold_trampolines {
            fold_trampolines(table);
        }
        if opts.inline_branch_bodies {
            inline_branch_bodies(table);
        }
        if opts.eliminate_dead {
            eliminate_dead(table);
        }
        if ops_snapshot(table) == snapshot {
            break;
        }
    }
    // Final renumber: surviving state ids are sparse after DCE and
    // splicing (e.g. `1, 2, 3, 17, …`). Compact them to a dense
    // `1..N` range so backends emit tighter `match`/`switch` tables
    // and per-state arrays.
    compact_state_ids(table);
}

/// Snapshot every state's ops keyed by id. The fixpoint loop uses
/// equality of two snapshots to decide convergence; passes use a
/// snapshot to source "the body before any rewrites this iteration"
/// when they need to inline a target into a caller.
fn ops_snapshot(table: &StateTable) -> BTreeMap<StateId, Vec<Op>> {
    table
        .states
        .iter()
        .map(|(id, s)| (*id, s.ops.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// inline_jumps
// ---------------------------------------------------------------------------

/// For each state, absorb trailing `Jump(N)` chains: replace the
/// body's tail `Jump(N)` with `N`'s ops, as long as the result is a
/// valid body. Repeats per state until either the tail is no longer
/// a `Jump`, the chain loops back on itself, or the candidate would
/// violate the runtime invariant.
fn inline_jumps(table: &mut StateTable) {
    let snapshot = ops_snapshot(table);
    for state in table.states.values_mut() {
        state.ops = absorb_tail_jumps(&snapshot, state.id);
    }
}

fn absorb_tail_jumps(
    snapshot: &BTreeMap<StateId, Vec<Op>>,
    start: StateId,
) -> Vec<Op> {
    let mut ops = snapshot.get(&start).cloned().unwrap_or_default();
    // Cycle guard: a body whose tail Jump points back to a state
    // we've already inlined would either spin or fail validation
    // mid-walk; bail at the first revisit.
    let mut visited: HashSet<StateId> = HashSet::new();
    visited.insert(start);
    loop {
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
        let mut candidate = ops.clone();
        candidate.pop();
        candidate.extend(target_ops.iter().cloned());
        if !is_valid_body(&candidate) {
            break;
        }
        ops = candidate;
    }
    ops
}

// ---------------------------------------------------------------------------
// inline_branch_bodies
// ---------------------------------------------------------------------------

/// Replace any `[Op::Jump(s)]`-shaped body inside an `Op::Star`,
/// `Op::Opt`, or `Op::Dispatch` arm with a copy of `s`'s ops, when
/// the resulting host body is still valid. Multi-predecessor targets
/// get duplicated into each caller; the original stays alive only if
/// some other reference still points at it (Op::Star::head, an
/// untouched `[Op::Jump]` body that didn't validate, etc.).
fn inline_branch_bodies(table: &mut StateTable) {
    let snapshot = ops_snapshot(table);
    let entry_ids: HashSet<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();
    let inlinable: HashSet<StateId> = snapshot
        .keys()
        .copied()
        .filter(|id| !entry_ids.contains(id))
        .collect();
    if inlinable.is_empty() {
        return;
    }
    // Per-op rewrite with revert-on-invalid: an inline that pushes
    // its host past 1 event (or violates the post-Expect rule) is
    // reverted, but other ops in the host can still inline. The
    // fixpoint loop reaches a stable state.
    for state in table.states.values_mut() {
        for i in 0..state.ops.len() {
            let saved = state.ops[i].clone();
            inline_in_op(&mut state.ops[i], &snapshot, &inlinable);
            if !is_valid_body(&state.ops) {
                state.ops[i] = saved;
            }
        }
    }
}

fn inline_in_op(
    op: &mut Op,
    snapshot: &BTreeMap<StateId, Vec<Op>>,
    inlinable: &HashSet<StateId>,
) {
    match op {
        Op::Star { body, .. } | Op::Opt { body, .. } => {
            inline_jump_body(body, snapshot, inlinable);
            // Once the body is no longer just `[Jump(s)]`, recurse to
            // catch any branchy ops *inside* it whose own body is
            // still a `[Jump]`. Without this, nested loops/opts would
            // need a fixpoint iteration per nesting depth and stop
            // converging once the outer body stops being a simple
            // Jump.
            for inner in body.iter_mut() {
                inline_in_op(inner, snapshot, inlinable);
            }
        }
        Op::Dispatch { tree, .. } => inline_in_tree(tree, snapshot, inlinable),
        _ => {}
    }
}

fn inline_in_tree(
    tree: &mut DispatchTree,
    snapshot: &BTreeMap<StateId, Vec<Op>>,
    inlinable: &HashSet<StateId>,
) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(body)) => {
            inline_jump_body(body, snapshot, inlinable);
            for inner in body.iter_mut() {
                inline_in_op(inner, snapshot, inlinable);
            }
        }
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                inline_jump_body(body, snapshot, inlinable);
                for inner in body.iter_mut() {
                    inline_in_op(inner, snapshot, inlinable);
                }
            }
            for (_, sub) in arms {
                inline_in_tree(sub, snapshot, inlinable);
            }
        }
    }
}

/// If `body` is the trivial `[Op::Jump(s)]` shape, replace it with a
/// snapshot of `s`'s ops. Bodies that have already been inlined
/// (anything other than a single Jump) are left alone — the next
/// fixpoint iteration will catch any further inlining opportunities
/// after their callers update their snapshots.
fn inline_jump_body(
    body: &mut Body,
    snapshot: &BTreeMap<StateId, Vec<Op>>,
    inlinable: &HashSet<StateId>,
) {
    let s = match body.as_slice() {
        [Op::Jump(s)] => *s,
        _ => return,
    };
    if !inlinable.contains(&s) {
        return;
    }
    *body = snapshot.get(&s).cloned().unwrap_or_default();
}

// ---------------------------------------------------------------------------
// fold_trampolines
// ---------------------------------------------------------------------------

/// Drop trampoline references — `Op::PushRet(s)` and
/// `cont: Some(s)` where `s`'s body is exactly `[Op::Ret]`. Two
/// shapes get cleaned up:
///
/// * **PushRet to a Ret state** — the push pairs with a future
///   `Ret` that immediately re-pops. Drop the PushRet outright; the
///   eventual Ret then pops *our* caller's continuation, which is
///   what we wanted in the first place.
///
/// * **`cont: Some(s)` where s = `[Ret]`** — `Op::Star`, `Op::Opt`,
///   `Op::Dispatch` rewrite to `cont: None`. Backends emit `cur =
///   ret()` directly on the miss/fall-through path instead of bouncing
///   through the trampoline.
///
/// Entry states are never treated as trampolines: they need to remain
/// callable by id from outside the dispatch loop. After this pass the
/// trampoline state often has zero references and `eliminate_dead`
/// drops it.
fn fold_trampolines(table: &mut StateTable) {
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
        state.ops.retain(|op| match op {
            Op::PushRet(t) => !return_only.contains(t),
            _ => true,
        });
        for op in state.ops.iter_mut() {
            let cont = match op {
                Op::Opt { cont, .. } | Op::Dispatch { cont, .. } | Op::Star { cont, .. } => cont,
                _ => continue,
            };
            if let Some(t) = *cont {
                if return_only.contains(&t) {
                    *cont = None;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// eliminate_dead
// ---------------------------------------------------------------------------

/// Drop every state no public entry can reach. Walks the call graph
/// from the entry-state roots, then retains only the visited set.
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
        Op::Star {
            body, cont, head, ..
        } => {
            for op in body {
                collect_op_targets(op, out);
            }
            if let Some(c) = cont {
                out.push(*c);
            }
            // `head` is the loop-head state. After splicing a Star
            // into another state, head still points at the original
            // — keep it alive so loop-iteration-ret lands somewhere.
            out.push(*head);
        }
        Op::Opt { body, cont, .. } => {
            for op in body {
                collect_op_targets(op, out);
            }
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

fn collect_tree_targets(tree: &DispatchTree, out: &mut Vec<StateId>) {
    let walk = |body: &Body, out: &mut Vec<StateId>| {
        for op in body {
            collect_op_targets(op, out);
        }
    };
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(b)) => walk(b, out),
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(b) = default {
                walk(b, out);
            }
            for (_, sub) in arms {
                collect_tree_targets(sub, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// compact_state_ids
// ---------------------------------------------------------------------------

/// Renumber surviving states to a dense `1..N` range. After splicing
/// and DCE the original layout-time ids are usually sparse — this
/// pass is a pure renumber that lets backends emit tighter
/// `match`/`switch` tables and per-state arrays.
///
/// Walks the table in current key order (which is `BTreeMap`-sorted,
/// so the renumbering is deterministic) and applies the resulting
/// `old → new` map to every `StateId` in the table: the keys
/// themselves, every state's `id` field, every `Op`'s target fields
/// (`Jump`, `PushRet`, `Star::head`, `Star`/`Opt`/`Dispatch::cont`,
/// dispatch-arm bodies), and every entry in `entry_states`.
fn compact_state_ids(table: &mut StateTable) {
    let remap: BTreeMap<StateId, StateId> = table
        .states
        .keys()
        .copied()
        .zip(1u32..)
        .collect();
    if remap.iter().all(|(old, new)| old == new) {
        return;
    }
    let old_states = std::mem::take(&mut table.states);
    for (old_id, mut state) in old_states {
        let new_id = remap[&old_id];
        state.id = new_id;
        remap_ops(&mut state.ops, &remap);
        table.states.insert(new_id, state);
    }
    for (_, id) in table.entry_states.iter_mut() {
        *id = remap[id];
    }
}

fn remap_ops(ops: &mut [Op], remap: &BTreeMap<StateId, StateId>) {
    for op in ops {
        remap_op(op, remap);
    }
}

fn remap_op(op: &mut Op, remap: &BTreeMap<StateId, StateId>) {
    match op {
        Op::PushRet(s) | Op::Jump(s) => *s = remap[s],
        Op::Ret | Op::Enter(_) | Op::Exit(_) | Op::Expect { .. } => {}
        Op::Star {
            body, cont, head, ..
        } => {
            remap_ops(body, remap);
            if let Some(c) = cont {
                *c = remap[c];
            }
            *head = remap[head];
        }
        Op::Opt { body, cont, .. } => {
            remap_ops(body, remap);
            if let Some(c) = cont {
                *c = remap[c];
            }
        }
        Op::Dispatch { tree, cont, .. } => {
            if let Some(c) = cont {
                *c = remap[c];
            }
            remap_tree(tree, remap);
        }
    }
}

fn remap_tree(tree: &mut DispatchTree, remap: &BTreeMap<StateId, StateId>) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(body)) => remap_ops(body, remap),
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                remap_ops(body, remap);
            }
            for (_, sub) in arms {
                remap_tree(sub, remap);
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
    use std::collections::BTreeMap;

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
            lexer_dfa: empty_dfa(),
        }
    }

    fn optimize_default(table: &mut StateTable) {
        optimize(table, LoweringOpts::default());
    }

    #[test]
    fn pushret_to_ret_only_state_is_stripped() {
        let mut states = BTreeMap::new();
        states.insert(
            1,
            make_state(1, vec![Op::PushRet(6), Op::Enter(0), Op::Jump(5)]),
        );
        states.insert(5, make_state(5, vec![Op::Exit(0), Op::Ret]));
        states.insert(6, make_state(6, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(
            !s1_ops.iter().any(|op| matches!(op, Op::PushRet(6))),
            "{:?}",
            s1_ops
        );
        assert!(
            !table.states.contains_key(&6),
            "trampoline state 6 still present"
        );
    }

    #[test]
    fn pushret_does_not_strip_when_target_is_an_entry() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::PushRet(2), Op::Jump(3)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 2));
        optimize_default(&mut table);
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
    fn pushret_does_not_strip_when_target_does_real_work() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::PushRet(2), Op::Jump(3)]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
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
        // 1's only op is an Opt with body=[Jump(5)]. State 5 has one
        // external reference, isn't an entry, and its ops fit inside
        // the host as a 1-event body. Inlining moves 5's ops into the
        // Opt body and DCE drops 5.
        let opt = Op::Opt {
            first: 0,
            body: vec![Op::Jump(5)],
            cont: Some(2),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![opt]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        let body_ops = vec![
            Op::Expect {
                kind: 1,
                token_name: "X".into(),
                sync: 0,
            },
            Op::Ret,
        ];
        states.insert(5, make_state(5, body_ops.clone()));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        let s1 = table.states.get(&1).unwrap();
        let body = match s1.ops.last() {
            Some(Op::Opt { body, .. }) => body.clone(),
            other => panic!("expected Opt, got {:?}", other),
        };
        assert_eq!(body, body_ops);
        assert!(!table.states.contains_key(&5));
    }

    #[test]
    fn multi_pred_branch_target_is_duplicated_into_each_caller() {
        let opt1 = Op::Opt {
            first: 0,
            body: vec![Op::Jump(5)],
            cont: Some(2),
        };
        let opt2 = Op::Opt {
            first: 0,
            body: vec![Op::Jump(5)],
            cont: Some(4),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![opt1]));
        states.insert(2, make_state(2, vec![Op::Exit(0), Op::Ret]));
        states.insert(3, make_state(3, vec![opt2]));
        states.insert(4, make_state(4, vec![Op::Exit(1), Op::Ret]));
        let body_ops = vec![Op::Enter(0), Op::Ret];
        states.insert(5, make_state(5, body_ops.clone()));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 3));
        optimize_default(&mut table);
        for src in [1, 3] {
            let s = table.states.get(&src).unwrap();
            let body = match s.ops.last() {
                Some(Op::Opt { body, .. }) => body.clone(),
                other => panic!("state {} did not get an Opt: {:?}", src, other),
            };
            assert_eq!(body, body_ops, "state {} body: {:?}", src, s.ops);
        }
        assert!(!table.states.contains_key(&5));
    }

    #[test]
    fn star_cont_to_ret_only_is_rewritten_to_none() {
        let star = Op::Star {
            first: 0,
            body: vec![Op::Jump(50)],
            cont: Some(2),
            head: 1,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![star]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(50, make_state(50, vec![Op::Exit(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
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
        assert!(!table.states.contains_key(&2));
    }

    #[test]
    fn optimize_real_lowering_keeps_entries() {
        let ag = analyze_src("T = \"t\"; main = T;");
        let st = lower(&ag);
        for (_, id) in &st.entry_states {
            assert!(st.states.contains_key(id), "entry {} missing post-optimize", id);
        }
    }

    #[test]
    fn eliminate_dead_drops_states_no_entry_can_reach() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        assert!(table.states.contains_key(&1));
        assert!(!table.states.contains_key(&99));
    }

    #[test]
    fn jump_chain_is_absorbed_into_first_state() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Jump(3)]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(matches!(s1_ops.last(), Some(Op::Ret)), "{:?}", s1_ops);
    }

    #[test]
    fn jump_chain_breaks_on_self_loop() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(1)]));
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert_eq!(s1_ops.len(), 1);
    }

    #[test]
    fn jump_chain_absorbs_branchy_target() {
        // 1 -> Jump(2); 2 starts with Dispatch. Aggressive inline_jumps
        // absorbs the branchy target into 1's tail (no single-pred /
        // non-branchy gate any more).
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
        inline_jumps(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(
            matches!(s1_ops.last(), Some(Op::Dispatch { .. })),
            "{:?}",
            s1_ops
        );
    }

    #[test]
    fn jump_chain_duplicates_branchy_target_into_every_caller() {
        // 1 -> Jump(2); 3 -> Jump(2); 2 starts with Dispatch. Both 1
        // and 3 absorb a copy of the dispatch — aggressive policy
        // duplicates instead of leaving the shared target alone.
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
        table.entry_states.push(("alt".into(), 3));
        inline_jumps(&mut table);
        for src in [1, 3] {
            let s = table.states.get(&src).unwrap();
            assert!(
                matches!(s.ops.last(), Some(Op::Dispatch { .. })),
                "state {} did not absorb dispatch: {:?}",
                src,
                s.ops
            );
        }
    }

    #[test]
    fn star_target_inlined_and_head_state_preserved() {
        // 1 -> [Enter, Jump(2)]; 2 -> [Star{head: 2}]. Aggressive
        // splice absorbs 2's Star into 1. The Star's `head` keeps
        // pointing at 2, so 2 stays alive (eliminate_dead reaches it
        // via the spliced Star) and the loop re-evaluation lands at 2
        // instead of re-running 1's prologue.
        let star = Op::Star {
            first: 0,
            body: vec![Op::Jump(50)],
            cont: Some(99),
            head: 2,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Enter(0), Op::Jump(2)]));
        states.insert(2, make_state(2, vec![star]));
        // 50 is the Star body. Make it a non-trivial state so branch
        // inlining doesn't fold it into the Star (this test is about
        // splice + head-preservation, not branch inlining).
        states.insert(50, make_state(50, vec![Op::Exit(0), Op::Ret]));
        // Make 99 do real work so its `cont` isn't tail-rewritten —
        // we want to see the Star in state 1's tail unchanged.
        states.insert(99, make_state(99, vec![Op::Exit(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("body_alt".into(), 50));
        optimize_default(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        assert!(
            matches!(s1_ops.last(), Some(Op::Star { head: 2, .. })),
            "{:?}",
            s1_ops
        );
        assert!(table.states.contains_key(&2), "head target dropped");
    }

    #[test]
    fn entry_state_is_not_inlined_even_when_single_ref() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 2));
        optimize_default(&mut table);
        assert!(table.states.contains_key(&2), "entry state 2 was dropped");
    }

    #[test]
    fn multi_pred_jump_chain_is_fully_absorbed() {
        // No duplication budget: every intermediate gets pulled into
        // state 1, even though each is referenced from a side caller.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        for id in 2..=7 {
            states.insert(id, make_state(id, vec![Op::PushRet(99), Op::Jump(id + 1)]));
        }
        states.insert(8, make_state(8, vec![Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        for (alt_id, target) in (100..).zip(2..=8) {
            states.insert(alt_id, make_state(alt_id, vec![Op::Jump(target)]));
        }
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1_ops = &table.states.get(&1).unwrap().ops;
        let push_rets = s1_ops
            .iter()
            .filter(|op| matches!(op, Op::PushRet(_)))
            .count();
        assert_eq!(push_rets, 6, "{:?}", s1_ops);
        assert!(matches!(s1_ops.last(), Some(Op::Ret)), "{:?}", s1_ops);
    }
}
