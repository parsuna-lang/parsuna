//! Post-layout clean-ups: splice deterministic jump chains into their
//! predecessors, then drop any state no entry point can reach.
//!
//! Splicing keeps generated code smaller and faster by turning
//! `A -> B -> C -> D` (all unconditional jumps) into a single `A -> D`
//! fall-through, as long as none of the intermediate states does
//! anything branchy.

use std::collections::{BTreeMap, HashSet, VecDeque};

use crate::lowering::{DispatchLeaf, DispatchTree, Op, State, StateId, StateTable};

/// Cap on how many successor states a single splice will absorb. Bounds
/// how large a generated state can get and protects against pathological
/// chains exploding one state's code size.
pub const DEFAULT_MAX_DEPTH: usize = 6;

/// Splice straight-line jump chains and drop unreachable states.
pub fn fuse(table: &mut StateTable) {
    splice_chains(&mut table.states);
    eliminate_dead(table);
}

fn splice_chains(states: &mut BTreeMap<StateId, State>) {
    let snapshot: BTreeMap<StateId, Vec<Op>> =
        states.iter().map(|(id, s)| (*id, s.ops.clone())).collect();
    for state in states.values_mut() {
        state.ops = fuse_ops(&snapshot, state.id, DEFAULT_MAX_DEPTH);
    }
}

fn fuse_ops(snapshot: &BTreeMap<StateId, Vec<Op>>, start_id: StateId, max_depth: usize) -> Vec<Op> {
    let mut ops = snapshot.get(&start_id).cloned().unwrap_or_default();
    // Guard against a jump chain that loops back on itself: once we have
    // already absorbed a state, re-absorbing it would inline its code
    // twice (and, eventually, infinitely).
    let mut visited: HashSet<StateId> = HashSet::new();
    visited.insert(start_id);
    let mut absorbed = 0usize;
    while absorbed < max_depth {
        // Only chains ending in `Jump` are splice candidates — anything
        // else (Ret, Expect, etc.) already has semantic effect we must
        // preserve at this state boundary.
        let target = match ops.last() {
            Some(Op::Jump(t)) => *t,
            _ => break,
        };
        if !visited.insert(target) {
            break;
        }
        let target_ops = match snapshot.get(&target) {
            Some(ops) => ops,
            None => break,
        };
        // Branchy first-ops are jump targets for other predecessors too;
        // splicing them in would both duplicate logic and lose a shared
        // entry point. Stop the chain here.
        match target_ops.first() {
            Some(Op::Star { .. }) | Some(Op::Opt { .. }) | Some(Op::Dispatch { .. }) => break,
            None => break,
            _ => {}
        }
        ops.pop();
        ops.extend(target_ops.iter().cloned());
        absorbed += 1;
    }
    ops
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
    match op {
        Op::PushRet(n) | Op::Jump(n) => vec![*n],
        Op::Ret | Op::Enter(_) | Op::Exit(_) | Op::Expect { .. } => Vec::new(),
        Op::Star { body, next, .. } | Op::Opt { body, next, .. } => vec![*body, *next],
        Op::Dispatch { tree, next, .. } => {
            let mut out = vec![*next];
            collect_tree_targets(tree, &mut out);
            out
        }
    }
}

fn collect_tree_targets(tree: &DispatchTree, out: &mut Vec<StateId>) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(t)) => out.push(*t),
        DispatchTree::Leaf(_) => {}
        DispatchTree::Switch { arms, default, .. } => {
            if let DispatchLeaf::Arm(t) = default {
                out.push(*t);
            }
            for (_, sub) in arms {
                collect_tree_targets(sub, out);
            }
        }
    }
}
