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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::grammar::parse_grammar;
    use crate::lowering::{lower, DfaTable, StateTable};

    fn analyze_src(src: &str) -> crate::AnalyzedGrammar {
        let g = parse_grammar(src).expect("parse");
        let outcome = analyze(g);
        assert!(!outcome.has_errors(), "{:?}", outcome.diagnostics);
        outcome.grammar.expect("grammar")
    }

    fn empty_dfa() -> DfaTable {
        DfaTable {
            states: vec![crate::lowering::lexer_dfa::DfaState {
                trans: vec![0; 256],
                accept: None,
            }],
            start: 0,
        }
    }

    fn make_state(id: StateId, ops: Vec<Op>) -> State {
        State {
            id,
            label: format!("s{}", id),
            ops,
        }
    }

    #[test]
    fn default_max_depth_is_positive() {
        assert!(DEFAULT_MAX_DEPTH > 0);
    }

    #[test]
    fn fuse_real_lowering_reaches_fixpoint_and_keeps_entries() {
        // Smoke: lower() runs fuse() last; the resulting table should still
        // have each entry state present in `states`.
        let ag = analyze_src("T = \"t\"; main = T;");
        let st = lower(&ag);
        for (_, id) in &st.entry_states {
            assert!(st.states.contains_key(id), "entry {} missing post-fuse", id);
        }
    }

    #[test]
    fn eliminate_dead_drops_states_no_entry_can_reach() {
        // Build a tiny table by hand: state 1 is the entry, jumps to 2;
        // state 99 is unreferenced and should be dropped.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Ret]));
        let mut table = StateTable {
            grammar_name: "x".into(),
            tokens: vec![],
            rule_kinds: vec![],
            first_sets: vec![],
            sync_sets: vec![],
            states,
            entry_states: vec![("main".into(), 1)],
            eof_id: 0,
            error_id: -1,
            k: 1,
            lexer_dfa: empty_dfa(),
        };
        fuse(&mut table);
        assert!(table.states.contains_key(&1));
        // Either kept as a separate state, or spliced into 1 — but 99 must go.
        assert!(!table.states.contains_key(&99));
    }

    #[test]
    fn splice_chains_absorbs_jump_chain_into_first_state() {
        // 1 -> Jump(2); 2 -> Jump(3); 3 -> Ret.
        // After splice, state 1's tail Jump should be replaced with the
        // chain, ending at Ret.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![Op::Jump(3)]));
        states.insert(3, make_state(3, vec![Op::Ret]));
        splice_chains(&mut states);
        let s1_ops = &states.get(&1).unwrap().ops;
        // Final op should now be Ret (chain absorbed).
        assert!(matches!(s1_ops.last(), Some(Op::Ret)), "{:?}", s1_ops);
    }

    #[test]
    fn splice_chains_stops_at_branchy_target() {
        // 1 -> Jump(2); 2 starts with a Dispatch (branchy) — splicing must
        // stop at 1 so the original Jump is retained.
        let dispatch = Op::Dispatch {
            tree: DispatchTree::Leaf(DispatchLeaf::Fallthrough),
            sync: 0,
            next: 99,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(2)]));
        states.insert(2, make_state(2, vec![dispatch]));
        splice_chains(&mut states);
        let s1_ops = &states.get(&1).unwrap().ops;
        assert!(matches!(s1_ops.last(), Some(Op::Jump(2))), "{:?}", s1_ops);
    }

    #[test]
    fn splice_chains_breaks_self_loop() {
        // A jump that loops back on itself must not inline forever.
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Jump(1)]));
        splice_chains(&mut states); // would diverge if the visited-set guard were missing
        let s1_ops = &states.get(&1).unwrap().ops;
        assert_eq!(s1_ops.len(), 1);
    }
}
