//! Post-layout optimization. Layout produces a state table with the
//! one-event-per-step invariant already satisfied (every body's tail
//! is a real terminator, never `[Ret]` in a separate state). This
//! module's passes are *purely* about size and dispatch-hop reduction;
//! the parser is correct without them.
//!
//! Four passes, each gated by a flag in [`LoweringOpts`]:
//!
//! * [`inline_jumps`] — when a state's `tail` is `Jump(N)`, replace
//!   it with `N`'s body (concatenating instrs, taking N's tail), as
//!   long as the result still satisfies [`is_valid_body`].
//!   Multi-predecessor targets get duplicated; the original stays
//!   alive only if some other reference remains.
//!
//! * [`inline_branch_bodies`] — a sub-body that's just
//!   `Body { instrs: [], tail: Jump(s) }` inside a `Tail::Star`,
//!   `Tail::Opt`, or dispatch arm gets replaced with `s`'s body
//!   directly, again under the [`is_valid_body`] gate.
//!
//! * [`fold_trampolines`] — drops `Instr::PushRet(s)` whose `s` is
//!   a `[Ret]`-only state, and rewrites `cont: Some(s) → None` for
//!   the same case. Inlining tends to expose these (a body that
//!   absorbed a tail of `Ret` becomes a fresh trampoline candidate).
//!
//! * [`eliminate_dead`] — drops every state no public entry can
//!   reach.
//!
//! The four run in a fixpoint loop because they feed each other:
//! inlining shrinks ref counts (which `eliminate_dead` then drops),
//! and dropping a state's last reference can turn neighboring states
//! into trampolines.

use std::collections::{BTreeMap, HashSet, VecDeque};

use crate::lowering::validate::is_valid_body;
use crate::lowering::{Body, DispatchLeaf, DispatchTree, Instr, LoweringOpts, StateId, StateTable, Tail};

/// Run enabled passes to a fixpoint. With every flag off the loop
/// converges in a single iteration (no change), which is fine: the
/// generated parser is correct without any optimizer pass running —
/// the runtime's `next_event` loop walks through 0-event state
/// bodies (loop heads, jump trampolines) until `step` returns an
/// event, so each optimizer pass is purely about reducing state
/// hops and table size.
pub fn optimize(table: &mut StateTable, opts: LoweringOpts) {
    loop {
        let snapshot = body_snapshot(table);
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
        if body_snapshot(table) == snapshot {
            break;
        }
    }
    // Final renumber: surviving state ids are sparse after DCE and
    // splicing (e.g. `1, 2, 3, 17, …`). Compact them to a dense
    // `1..N` range so backends emit tighter `match`/`switch` tables
    // and per-state arrays.
    compact_state_ids(table);
}

/// Snapshot every state's body keyed by id. The fixpoint loop uses
/// equality of two snapshots to decide convergence; passes use a
/// snapshot to source "the body before any rewrites this iteration"
/// when they need to inline a target into a caller.
fn body_snapshot(table: &StateTable) -> BTreeMap<StateId, Body> {
    table
        .states
        .iter()
        .map(|(id, s)| (*id, s.body.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// inline_jumps
// ---------------------------------------------------------------------------

/// For each state, absorb trailing `Jump(N)` chains: when the body's
/// `tail` is `Jump(N)`, replace it with `N`'s body (extending instrs
/// with N's instrs and taking N's tail), as long as the result is a
/// valid body. Repeats per state until either the tail is no longer
/// a `Jump`, the chain loops back on itself, or the candidate would
/// violate the runtime invariant.
fn inline_jumps(table: &mut StateTable) {
    let snapshot = body_snapshot(table);
    for state in table.states.values_mut() {
        state.body = absorb_tail_jumps(&snapshot, state.id);
    }
}

fn absorb_tail_jumps(snapshot: &BTreeMap<StateId, Body>, start: StateId) -> Body {
    let mut body = snapshot.get(&start).cloned().unwrap_or_else(|| Body {
        instrs: Vec::new(),
        tail: Tail::Ret,
    });
    // Cycle guard: a body whose tail Jump points back to a state
    // we've already inlined would either spin or fail validation
    // mid-walk; bail at the first revisit.
    let mut visited: HashSet<StateId> = HashSet::new();
    visited.insert(start);
    loop {
        let target = match body.tail {
            Tail::Jump(t) => t,
            _ => break,
        };
        if !visited.insert(target) {
            break;
        }
        let Some(target_body) = snapshot.get(&target) else {
            break;
        };
        let mut candidate = body.clone();
        // Splice: drop the trailing Jump and append the target's
        // instrs + tail.
        candidate.instrs.extend(target_body.instrs.iter().cloned());
        candidate.tail = target_body.tail.clone();
        if !is_valid_body(&candidate) {
            break;
        }
        body = candidate;
    }
    body
}

// ---------------------------------------------------------------------------
// inline_branch_bodies
// ---------------------------------------------------------------------------

/// Replace any `Body { instrs: [], tail: Jump(s) }`-shaped sub-body
/// inside a `Tail::Star`, `Tail::Opt`, or `Tail::Dispatch` arm with a
/// copy of `s`'s body, when the resulting host is still valid.
/// Multi-predecessor targets get duplicated into each caller; the
/// original stays alive only if some other reference still points at
/// it (Tail::Star::head, an untouched jump-only body that didn't
/// validate, etc.).
fn inline_branch_bodies(table: &mut StateTable) {
    let snapshot = body_snapshot(table);
    let entry_ids: HashSet<StateId> = table.entry_states.iter().map(|(_, id)| *id).collect();
    let inlinable: HashSet<StateId> = snapshot
        .keys()
        .copied()
        .filter(|id| !entry_ids.contains(id))
        .collect();
    if inlinable.is_empty() {
        return;
    }
    // Per-state rewrite with revert-on-invalid: an inline that pushes
    // its host past 1 event (or violates the post-Expect rule) is
    // reverted, but other branchy ops in the host can still inline.
    // The fixpoint loop reaches a stable state.
    for state in table.states.values_mut() {
        let saved = state.body.clone();
        inline_in_tail(&mut state.body.tail, &snapshot, &inlinable);
        if !is_valid_body(&state.body) {
            state.body = saved;
        }
    }
}

fn inline_in_tail(
    tail: &mut Tail,
    snapshot: &BTreeMap<StateId, Body>,
    inlinable: &HashSet<StateId>,
) {
    match tail {
        Tail::Jump(_) | Tail::Ret => {}
        Tail::Star { body, .. } | Tail::Opt { body, .. } => {
            inline_jump_body(body, snapshot, inlinable);
            // Once the body is no longer just `[Jump(s)]`, recurse
            // into its tail to catch any branchy ops *inside* it
            // whose own body is still a `[Jump]`. Without this,
            // nested loops/opts would need a fixpoint iteration per
            // nesting depth and stop converging once the outer body
            // stops being a simple Jump.
            inline_in_tail(&mut body.tail, snapshot, inlinable);
        }
        Tail::Dispatch { tree, .. } => inline_in_tree(tree, snapshot, inlinable),
    }
}

fn inline_in_tree(
    tree: &mut DispatchTree,
    snapshot: &BTreeMap<StateId, Body>,
    inlinable: &HashSet<StateId>,
) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(body)) => {
            inline_jump_body(body, snapshot, inlinable);
            inline_in_tail(&mut body.tail, snapshot, inlinable);
        }
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                inline_jump_body(body, snapshot, inlinable);
                inline_in_tail(&mut body.tail, snapshot, inlinable);
            }
            for (_, sub) in arms {
                inline_in_tree(sub, snapshot, inlinable);
            }
        }
    }
}

/// If `body` is the trivial `Body { instrs: [], tail: Jump(s) }` shape,
/// replace it with a snapshot of `s`'s body. Bodies that have already
/// been inlined (anything richer than a single Jump) are left alone —
/// the next fixpoint iteration will catch any further inlining
/// opportunities after their callers update their snapshots.
fn inline_jump_body(
    body: &mut Body,
    snapshot: &BTreeMap<StateId, Body>,
    inlinable: &HashSet<StateId>,
) {
    let Some(s) = body.jump_target() else {
        return;
    };
    if !inlinable.contains(&s) {
        return;
    }
    if let Some(target) = snapshot.get(&s) {
        *body = target.clone();
    }
}

// ---------------------------------------------------------------------------
// fold_trampolines
// ---------------------------------------------------------------------------

/// Drop trampoline references — `Instr::PushRet(s)` and
/// `cont: Some(s)` where `s`'s body is exactly `Ret`. Two shapes get
/// cleaned up:
///
/// * **PushRet to a Ret state** — the push pairs with a future
///   `Ret` that immediately re-pops. Drop the PushRet outright; the
///   eventual Ret then pops *our* caller's continuation, which is
///   what we wanted in the first place.
///
/// * **`cont: Some(s)` where s = `Ret`** — `Tail::Star`, `Tail::Opt`,
///   `Tail::Dispatch` rewrite to `cont: None`. Backends emit `cur =
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
        .filter(|(id, s)| {
            !entry_ids.contains(id) && s.body.instrs.is_empty() && matches!(s.body.tail, Tail::Ret)
        })
        .map(|(id, _)| *id)
        .collect();
    if return_only.is_empty() {
        return;
    }
    for state in table.states.values_mut() {
        state
            .body
            .instrs
            .retain(|op| !matches!(op, Instr::PushRet(t) if return_only.contains(t)));
        let cont = match &mut state.body.tail {
            Tail::Opt { cont, .. } | Tail::Dispatch { cont, .. } | Tail::Star { cont, .. } => {
                Some(cont)
            }
            _ => None,
        };
        if let Some(cont) = cont {
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
        let mut targets = Vec::new();
        collect_body_targets(&state.body, &mut targets);
        for target in targets {
            if !reachable.contains(&target) {
                queue.push_back(target);
            }
        }
    }
    table.states.retain(|id, _| reachable.contains(id));
}

fn collect_body_targets(body: &Body, out: &mut Vec<StateId>) {
    for instr in &body.instrs {
        if let Instr::PushRet(n) = instr {
            out.push(*n);
        }
    }
    collect_tail_targets(&body.tail, out);
}

fn collect_tail_targets(tail: &Tail, out: &mut Vec<StateId>) {
    match tail {
        Tail::Jump(n) => out.push(*n),
        Tail::Ret => {}
        Tail::Star {
            body, cont, head, ..
        } => {
            collect_body_targets(body, out);
            if let Some(c) = cont {
                out.push(*c);
            }
            // `head` is the loop-head state. After splicing a Star
            // into another state, head still points at the original
            // — keep it alive so loop-iteration-ret lands somewhere.
            out.push(*head);
        }
        Tail::Opt { body, cont, .. } => {
            collect_body_targets(body, out);
            if let Some(c) = cont {
                out.push(*c);
            }
        }
        Tail::Dispatch { tree, cont, .. } => {
            if let Some(c) = cont {
                out.push(*c);
            }
            collect_tree_targets(tree, out);
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

// ---------------------------------------------------------------------------
// compact_state_ids
// ---------------------------------------------------------------------------

/// Renumber surviving states to a dense `1..N` range. After splicing
/// and DCE the original layout-time ids are usually sparse — this
/// pass is a pure renumber that lets backends emit tighter
/// `match`/`switch` tables and per-state arrays.
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
        remap_body(&mut state.body, &remap);
        table.states.insert(new_id, state);
    }
    for (_, id) in table.entry_states.iter_mut() {
        *id = remap[id];
    }
}

fn remap_body(body: &mut Body, remap: &BTreeMap<StateId, StateId>) {
    for instr in body.instrs.iter_mut() {
        if let Instr::PushRet(s) = instr {
            *s = remap[s];
        }
    }
    remap_tail(&mut body.tail, remap);
}

fn remap_tail(tail: &mut Tail, remap: &BTreeMap<StateId, StateId>) {
    match tail {
        Tail::Jump(s) => *s = remap[s],
        Tail::Ret => {}
        Tail::Star {
            body, cont, head, ..
        } => {
            remap_body(body, remap);
            if let Some(c) = cont {
                *c = remap[c];
            }
            *head = remap[head];
        }
        Tail::Opt { body, cont, .. } => {
            remap_body(body, remap);
            if let Some(c) = cont {
                *c = remap[c];
            }
        }
        Tail::Dispatch { tree, cont, .. } => {
            if let Some(c) = cont {
                *c = remap[c];
            }
            remap_tree(tree, remap);
        }
    }
}

fn remap_tree(tree: &mut DispatchTree, remap: &BTreeMap<StateId, StateId>) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(body)) => remap_body(body, remap),
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(body) = default {
                remap_body(body, remap);
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
        }]
    }

    fn make_state(id: StateId, instrs: Vec<Instr>, tail: Tail) -> State {
        State {
            id,
            label: format!("s{}", id),
            body: Body { instrs, tail },
        }
    }

    fn empty_table_with(states: BTreeMap<StateId, State>, entry: StateId) -> StateTable {
        StateTable {
            grammar_name: "x".into(),
            tokens: vec![],
            rule_kinds: vec![],
            labels: vec![],
            first_sets: vec![],
            sync_sets: vec![],
            states,
            entry_states: vec![("main".into(), entry)],
            k: 1,
            modes: vec![crate::lowering::ModeInfo {
                id: 0,
                name: "default".into(),
                dfa: empty_dfa(),
            }],
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
            make_state(1, vec![Instr::PushRet(6), Instr::Enter(0)], Tail::Jump(5)),
        );
        states.insert(5, make_state(5, vec![Instr::Exit(0)], Tail::Ret));
        states.insert(6, make_state(6, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        assert!(
            !s1.instrs.iter().any(|op| matches!(op, Instr::PushRet(6))),
            "{:?}",
            s1
        );
        assert!(
            !table.states.contains_key(&6),
            "trampoline state 6 still present"
        );
    }

    #[test]
    fn pushret_does_not_strip_when_target_is_an_entry() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Instr::PushRet(2)], Tail::Jump(3)));
        states.insert(2, make_state(2, vec![], Tail::Ret));
        states.insert(3, make_state(3, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 2));
        optimize_default(&mut table);
        assert!(table
            .states
            .get(&1)
            .unwrap()
            .body
            .instrs
            .iter()
            .any(|op| matches!(op, Instr::PushRet(2))));
        assert!(table.states.contains_key(&2));
    }

    #[test]
    fn pushret_does_not_strip_when_target_does_real_work() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Instr::PushRet(2)], Tail::Jump(3)));
        states.insert(2, make_state(2, vec![Instr::Exit(0)], Tail::Ret));
        states.insert(3, make_state(3, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        assert!(table
            .states
            .get(&1)
            .unwrap()
            .body
            .instrs
            .iter()
            .any(|op| matches!(op, Instr::PushRet(2))));
        assert!(table.states.contains_key(&2));
    }

    #[test]
    fn single_pred_opt_body_is_inlined() {
        // 1's body is empty + a tail Opt with body=Jump(5). State 5's
        // body is `[Expect, Ret]`. Inlining replaces the Opt's body
        // with 5's body and DCE drops 5.
        let opt = Tail::Opt {
            first: 0,
            body: Box::new(Body::jump(5)),
            cont: Some(2),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], opt));
        states.insert(2, make_state(2, vec![Instr::Exit(0)], Tail::Ret));
        let target_body = Body {
            instrs: vec![Instr::Expect {
                kind: 1,
                token_name: "X".into(),
                sync: 0,
                label: None,
            }],
            tail: Tail::Ret,
        };
        states.insert(
            5,
            State {
                id: 5,
                label: "s5".into(),
                body: target_body.clone(),
            },
        );
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        let s1 = table.states.get(&1).unwrap();
        let body = match &s1.body.tail {
            Tail::Opt { body, .. } => body.clone(),
            other => panic!("expected Opt, got {:?}", other),
        };
        assert_eq!(*body, target_body);
        assert!(!table.states.contains_key(&5));
    }

    #[test]
    fn multi_pred_branch_target_is_duplicated_into_each_caller() {
        let opt1 = Tail::Opt {
            first: 0,
            body: Box::new(Body::jump(5)),
            cont: Some(2),
        };
        let opt2 = Tail::Opt {
            first: 0,
            body: Box::new(Body::jump(5)),
            cont: Some(4),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], opt1));
        states.insert(2, make_state(2, vec![Instr::Exit(0)], Tail::Ret));
        states.insert(3, make_state(3, vec![], opt2));
        states.insert(4, make_state(4, vec![Instr::Exit(1)], Tail::Ret));
        let target_body = Body {
            instrs: vec![Instr::Enter(0)],
            tail: Tail::Ret,
        };
        states.insert(
            5,
            State {
                id: 5,
                label: "s5".into(),
                body: target_body.clone(),
            },
        );
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 3));
        optimize_default(&mut table);
        for src in [1, 3] {
            let s = table.states.get(&src).unwrap();
            let body = match &s.body.tail {
                Tail::Opt { body, .. } => body.clone(),
                other => panic!("state {} did not get an Opt: {:?}", src, other),
            };
            assert_eq!(*body, target_body, "state {} body: {:?}", src, s.body);
        }
        assert!(!table.states.contains_key(&5));
    }

    #[test]
    fn star_cont_to_ret_only_is_rewritten_to_none() {
        let star = Tail::Star {
            first: 0,
            body: Box::new(Body::jump(50)),
            cont: Some(2),
            head: 1,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], star));
        states.insert(2, make_state(2, vec![], Tail::Ret));
        states.insert(50, make_state(50, vec![Instr::Exit(0)], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        let s1 = table.states.get(&1).unwrap();
        assert!(
            matches!(
                s1.body.tail,
                Tail::Star {
                    cont: None,
                    head: 1,
                    ..
                }
            ),
            "{:?}",
            s1.body
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
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], Tail::Ret));
        states.insert(99, make_state(99, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        optimize_default(&mut table);
        assert!(table.states.contains_key(&1));
        assert!(!table.states.contains_key(&99));
    }

    #[test]
    fn jump_chain_is_absorbed_into_first_state() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], Tail::Jump(3)));
        states.insert(3, make_state(3, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        assert!(matches!(s1.tail, Tail::Ret), "{:?}", s1);
    }

    #[test]
    fn jump_chain_breaks_on_self_loop() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], Tail::Jump(1)));
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        assert!(matches!(s1.tail, Tail::Jump(1)));
        assert!(s1.instrs.is_empty());
    }

    #[test]
    fn jump_chain_absorbs_branchy_target() {
        // 1 -> Jump(2); 2's body's tail is Dispatch. inline_jumps
        // absorbs the branchy target into 1's tail.
        let dispatch = Tail::Dispatch {
            tree: DispatchTree::Leaf(DispatchLeaf::Fallthrough),
            sync: 0,
            cont: Some(99),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], dispatch));
        states.insert(99, make_state(99, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        assert!(matches!(s1.tail, Tail::Dispatch { .. }), "{:?}", s1);
    }

    #[test]
    fn jump_chain_duplicates_branchy_target_into_every_caller() {
        // 1 -> Jump(2); 3 -> Jump(2); 2's tail is Dispatch. Both 1 and 3
        // absorb a copy of the dispatch — aggressive policy duplicates
        // instead of leaving the shared target alone.
        let dispatch = || Tail::Dispatch {
            tree: DispatchTree::Leaf(DispatchLeaf::Fallthrough),
            sync: 0,
            cont: Some(99),
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], dispatch()));
        states.insert(3, make_state(3, vec![], Tail::Jump(2)));
        states.insert(99, make_state(99, vec![], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("alt".into(), 3));
        inline_jumps(&mut table);
        for src in [1, 3] {
            let s = &table.states.get(&src).unwrap().body;
            assert!(
                matches!(s.tail, Tail::Dispatch { .. }),
                "state {} did not absorb dispatch: {:?}",
                src,
                s
            );
        }
    }

    #[test]
    fn star_target_inlined_and_head_state_preserved() {
        // 1 -> [Enter, Jump(2)]; 2's tail is a Star with head=2.
        // inline_jumps absorbs 2's Star into 1. The Star's `head`
        // keeps pointing at 2, so 2 stays alive (eliminate_dead reaches
        // it via the spliced Star) and the loop re-evaluation lands at 2
        // instead of re-running 1's prologue.
        let star = Tail::Star {
            first: 0,
            body: Box::new(Body::jump(50)),
            cont: Some(99),
            head: 2,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Instr::Enter(0)], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], star));
        // 50 is the Star body. Make it a non-trivial state so branch
        // inlining doesn't fold it into the Star (this test is about
        // splice + head-preservation, not branch inlining).
        states.insert(50, make_state(50, vec![Instr::Exit(0)], Tail::Ret));
        // Make 99 do real work so its `cont` isn't tail-rewritten —
        // we want to see the Star in state 1's tail unchanged.
        states.insert(99, make_state(99, vec![Instr::Exit(0)], Tail::Ret));
        let mut table = empty_table_with(states, 1);
        table.entry_states.push(("body_alt".into(), 50));
        optimize_default(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        assert!(
            matches!(s1.tail, Tail::Star { head: 2, .. }),
            "{:?}",
            s1
        );
        assert!(table.states.contains_key(&2), "head target dropped");
    }

    #[test]
    fn entry_state_is_not_inlined_even_when_single_ref() {
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        states.insert(2, make_state(2, vec![], Tail::Ret));
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
        states.insert(1, make_state(1, vec![], Tail::Jump(2)));
        for id in 2..=7 {
            states.insert(
                id,
                make_state(id, vec![Instr::PushRet(99)], Tail::Jump(id + 1)),
            );
        }
        states.insert(8, make_state(8, vec![], Tail::Ret));
        states.insert(99, make_state(99, vec![], Tail::Ret));
        for (alt_id, target) in (100..).zip(2..=8) {
            states.insert(alt_id, make_state(alt_id, vec![], Tail::Jump(target)));
        }
        let mut table = empty_table_with(states, 1);
        inline_jumps(&mut table);
        let s1 = &table.states.get(&1).unwrap().body;
        let push_rets = s1
            .instrs
            .iter()
            .filter(|op| matches!(op, Instr::PushRet(_)))
            .count();
        assert_eq!(push_rets, 6, "{:?}", s1);
        assert!(matches!(s1.tail, Tail::Ret), "{:?}", s1);
    }
}
