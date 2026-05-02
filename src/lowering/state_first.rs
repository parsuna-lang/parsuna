//! Per-state FIRST analysis.
//!
//! Given a finalised [`StateTable`], compute, for every state, the set of
//! token kinds that the state's body would accept as `look[0]` if drive
//! mode entered it next. This is the static "what can come next here?"
//! lookup the dispatch insertion-recovery codegen needs.
//!
//! The analysis is a fixed-point: each state's FIRST is derived from the
//! FIRSTs of the states it can hand control to without consuming a token
//! (Jump/Ret targets, Star/Opt continuations on the nullable arm, the
//! cont state of a fall-through Dispatch). We seed every entry as empty
//! and re-evaluate until no set grows.
//!
//! Termination is straightforward: there are finitely many states and
//! finitely many kinds, sets only grow, so the fixed point is reached in
//! at most `O(|states| × |kinds|)` rounds.

use std::collections::{BTreeMap, BTreeSet};

use crate::lowering::{
    DispatchLeaf, DispatchTree, Instr, StateId, StateTable, Tail,
};

/// Compute FIRST(state) for every state in `st`, returned as a map
/// keyed by state id. Each set is the kinds of `look[0]` that the
/// state could legitimately accept without an error event — i.e. the
/// set of single-token continuations starting at that state.
pub fn compute(st: &StateTable) -> BTreeMap<StateId, BTreeSet<u16>> {
    let mut firsts: BTreeMap<StateId, BTreeSet<u16>> =
        st.states.keys().copied().map(|id| (id, BTreeSet::new())).collect();

    loop {
        let mut changed = false;
        for (id, state) in &st.states {
            let new = first_for_state(state, st, &firsts);
            let target = firsts.get_mut(id).unwrap();
            for k in &new {
                if target.insert(*k) {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    firsts
}

fn first_for_state(
    state: &crate::lowering::State,
    st: &StateTable,
    firsts: &BTreeMap<StateId, BTreeSet<u16>>,
) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    // The first lookahead-consuming op in the body wins: any
    // Enter/Exit/PushRet before it doesn't read lookahead, but an
    // Expect fixes `look[0]` to its kind. Per the runtime invariant
    // (see fuse.rs::is_valid_state_body) nothing else can follow an
    // Expect within the same body, so the Expect's kind is the
    // state's FIRST and we're done.
    for instr in &state.body.instrs {
        match instr {
            Instr::Expect { kind, .. } => {
                out.insert(*kind);
                return out;
            }
            Instr::Enter(_) | Instr::Exit(_) | Instr::PushRet(_) => {}
        }
    }

    // No Expect: the tail decides.
    follow_tail(&state.body.tail, &state.rule, st, firsts, &mut out);
    out
}

fn follow_tail(
    tail: &Tail,
    rule: &str,
    st: &StateTable,
    firsts: &BTreeMap<StateId, BTreeSet<u16>>,
    out: &mut BTreeSet<u16>,
) {
    match tail {
        Tail::Jump(n) => {
            extend_with_state_first(out, *n, firsts);
        }
        Tail::Ret => {
            extend_with_rule_follow(out, rule, st);
        }
        Tail::Star { first, cont, .. } => {
            // The star body is taken when its FIRST set matches; the
            // star is nullable, so the continuation (cont state, or a
            // tail-call return — which collapses to the host rule's
            // FOLLOW) is also a valid look[0].
            extend_with_first_set(out, *first, st);
            match cont {
                Some(c) => extend_with_state_first(out, *c, firsts),
                None => extend_with_rule_follow(out, rule, st),
            }
        }
        Tail::Opt { first, cont, .. } => {
            extend_with_first_set(out, *first, st);
            match cont {
                Some(c) => extend_with_state_first(out, *c, firsts),
                None => extend_with_rule_follow(out, rule, st),
            }
        }
        Tail::Dispatch { tree, cont, .. } => {
            let mut has_fallthrough = false;
            collect_dispatch_first(tree, out, &mut has_fallthrough);
            if has_fallthrough {
                match cont {
                    Some(c) => extend_with_state_first(out, *c, firsts),
                    None => extend_with_rule_follow(out, rule, st),
                }
            }
        }
    }
}

fn collect_dispatch_first(
    tree: &DispatchTree,
    out: &mut BTreeSet<u16>,
    has_fallthrough: &mut bool,
) {
    match tree {
        DispatchTree::Leaf(DispatchLeaf::Arm(_)) => {
            // Arm at depth 0: the kind that walked us here is in
            // `out` already (the parent Switch added it). Nothing
            // extra to collect at this leaf.
        }
        DispatchTree::Leaf(DispatchLeaf::Fallthrough) => {
            *has_fallthrough = true;
        }
        DispatchTree::Leaf(DispatchLeaf::Error) => {}
        DispatchTree::Switch { depth: 0, arms, default } => {
            for (k, sub) in arms {
                out.insert(*k);
                // Sub-tree may still pick up a deeper-layer
                // Fallthrough; recurse.
                collect_dispatch_first(sub, &mut BTreeSet::new(), has_fallthrough);
            }
            // The default applies when look[0] missed every arm —
            // its disposition decides whether the dispatch is
            // nullable, but it doesn't add extra kinds to FIRST
            // (Fallthrough/Error contribute no fixed token).
            if let DispatchLeaf::Fallthrough = default {
                *has_fallthrough = true;
            }
        }
        DispatchTree::Switch { arms, default, .. } => {
            // Switch on a deeper lookahead slot than `look[0]` —
            // every arm reachable from here was reached by the
            // outer Switch's `look[0]` decision, so there's nothing
            // new to add at depth>0. We still need to surface a
            // Fallthrough leaf — it propagates "the dispatch as a
            // whole is nullable" up to the caller.
            for (_, sub) in arms {
                collect_dispatch_first(sub, &mut BTreeSet::new(), has_fallthrough);
            }
            if let DispatchLeaf::Fallthrough = default {
                *has_fallthrough = true;
            }
        }
    }
}

fn extend_with_state_first(
    out: &mut BTreeSet<u16>,
    state: StateId,
    firsts: &BTreeMap<StateId, BTreeSet<u16>>,
) {
    if let Some(s) = firsts.get(&state) {
        for k in s {
            out.insert(*k);
        }
    }
}

fn extend_with_rule_follow(out: &mut BTreeSet<u16>, rule: &str, st: &StateTable) {
    if rule.is_empty() {
        return;
    }
    let Some(sid) = st.rule_sync.get(rule) else {
        return;
    };
    let Some(set) = st.sync_sets.get(*sid as usize) else {
        return;
    };
    for k in &set.kinds {
        out.insert(*k);
    }
}

fn extend_with_first_set(out: &mut BTreeSet<u16>, fid: crate::lowering::FirstSetId, st: &StateTable) {
    let Some(fs) = st.first_sets.get(fid as usize) else {
        return;
    };
    for seq in &fs.seqs {
        if let Some(k) = seq.first() {
            out.insert(*k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowering::{Body, ModeInfo, State};

    fn empty_dfa() -> Vec<crate::lowering::DfaState> {
        vec![crate::lowering::DfaState {
            id: 0,
            arms: vec![],
            accept: None,
        }]
    }

    fn empty_table(
        states: BTreeMap<StateId, State>,
        rule_sync: BTreeMap<String, crate::lowering::SyncSetId>,
        sync_sets: Vec<crate::lowering::SyncSet>,
        first_sets: Vec<crate::lowering::FirstSet>,
    ) -> StateTable {
        StateTable {
            grammar_name: "x".into(),
            tokens: vec![],
            rule_kinds: vec![],
            labels: vec![],
            first_sets,
            sync_sets,
            rule_sync,
            states,
            entry_states: vec![],
            k: 1,
            modes: vec![ModeInfo {
                id: 0,
                name: "default".into(),
                dfa: empty_dfa(),
            }],
        }
    }

    #[test]
    fn expect_in_instrs_pins_first_to_its_kind() {
        let mut states = BTreeMap::new();
        states.insert(
            1,
            State {
                id: 1,
                label: "s1".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![
                        Instr::Enter(0),
                        Instr::Expect {
                            kind: 7,
                            token_name: "X".into(),
                            sync: 0,
                            label: None,
                        },
                    ],
                    tail: Tail::Jump(2),
                },
            },
        );
        states.insert(
            2,
            State {
                id: 2,
                label: "s2".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![Instr::Exit(0)],
                    tail: Tail::Ret,
                },
            },
        );
        let table = empty_table(states, BTreeMap::new(), vec![], vec![]);
        let firsts = compute(&table);
        assert_eq!(
            firsts.get(&1).unwrap().iter().copied().collect::<Vec<_>>(),
            vec![7]
        );
    }

    #[test]
    fn ret_state_first_is_rule_follow() {
        // s1: [Exit]; Ret. Rule "r" has FOLLOW = {3, 5}.
        let mut states = BTreeMap::new();
        states.insert(
            1,
            State {
                id: 1,
                label: "s1".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![Instr::Exit(0)],
                    tail: Tail::Ret,
                },
            },
        );
        let mut rule_sync = BTreeMap::new();
        rule_sync.insert("r".into(), 0);
        let sync_sets = vec![crate::lowering::SyncSet {
            id: 0,
            kinds: vec![3, 5],
        }];
        let table = empty_table(states, rule_sync, sync_sets, vec![]);
        let firsts = compute(&table);
        assert_eq!(
            firsts.get(&1).unwrap().iter().copied().collect::<Vec<_>>(),
            vec![3, 5]
        );
    }

    #[test]
    fn star_first_unions_body_and_continuation() {
        // s1: []; Star { first: [9], cont: Some(2) }.
        // s2: [Expect 4]; Ret.
        let mut states = BTreeMap::new();
        states.insert(
            1,
            State {
                id: 1,
                label: "s1".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![],
                    tail: Tail::Star {
                        first: 0,
                        body: Box::new(Body::jump(3)),
                        cont: Some(2),
                        head: 1,
                    },
                },
            },
        );
        states.insert(
            2,
            State {
                id: 2,
                label: "s2".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![Instr::Expect {
                        kind: 4,
                        token_name: "Y".into(),
                        sync: 0,
                        label: None,
                    }],
                    tail: Tail::Ret,
                },
            },
        );
        states.insert(
            3,
            State {
                id: 3,
                label: "s3".into(),
                rule: "r".into(),
                body: Body {
                    instrs: vec![Instr::Expect {
                        kind: 9,
                        token_name: "Z".into(),
                        sync: 0,
                        label: None,
                    }],
                    tail: Tail::Ret,
                },
            },
        );
        let first_sets = vec![crate::lowering::FirstSet {
            id: 0,
            seqs: vec![vec![9]],
            has_references: false,
        }];
        let table = empty_table(states, BTreeMap::new(), vec![], first_sets);
        let firsts = compute(&table);
        let s1 = firsts.get(&1).unwrap();
        assert!(s1.contains(&9), "star body kind missing: {:?}", s1);
        assert!(s1.contains(&4), "star cont kind missing: {:?}", s1);
    }
}
