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

use crate::lowering::{DispatchLeaf, DispatchTree, Op, StateId, StateTable};

/// Max chain depth to absorb when each step duplicates the target's
/// ops (i.e. the target has multiple predecessors). Bounds generated
/// file size for chains that would otherwise expand multiplicatively.
pub const DUPLICATION_BUDGET: usize = 6;

/// Splice straight-line jump chains, eliminate tail-call trampolines,
/// and drop unreachable states.
///
/// Tail-call elimination runs after the chain splicer because splicing
/// creates the situation TCE optimises: a state that ends in a "call"
/// pattern (`PushRet(B); ... <call>`) where `B` is now a pure-`Ret`
/// state because `splice_chains` collapsed everything else out of it.
pub fn fuse(table: &mut StateTable) {
    splice_chains(table);
    eliminate_tail_pushes(table);
    eliminate_dead(table);
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
        // `head` keeps the original loop-head state alive when the Star
        // op gets spliced into another state. `cont` is the post-loop
        // fall-through; `None` means tail call (the `Op::Ret` chain
        // shortcuts straight to the caller).
        Op::Star {
            body, cont, head, ..
        } => match cont {
            Some(c) => vec![*body, *c, *head],
            None => vec![*body, *head],
        },
        // Tail-flavoured Opt/Dispatch (`cont = None`) don't transition
        // through any continuation state — codegen emits `Ret` instead
        // of `Jump(cont)`. Excluding the missing target here lets
        // `eliminate_dead` drop the now-orphan trampoline state.
        Op::Opt { body, cont, .. } => match cont {
            Some(c) => vec![*body, *c],
            None => vec![*body],
        },
        Op::Dispatch { tree, cont, .. } => {
            let mut out = match cont {
                Some(c) => vec![*c],
                None => Vec::new(),
            };
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
    fn tail_push_rewrites_star_cont_to_none() {
        // 1 holds a Star with cont=2 and head=1; 2 is a pure-Ret
        // trampoline. Tail-call elimination should rewrite cont to
        // None so the loop's miss path emits `cur = ret()` directly.
        let star = Op::Star {
            first: 0,
            body: 50,
            cont: Some(2),
            head: 1,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![star]));
        states.insert(2, make_state(2, vec![Op::Ret]));
        states.insert(50, make_state(50, vec![Op::Ret]));
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
        let star = Op::Star {
            first: 0,
            body: 50,
            cont: Some(99),
            head: 2,
        };
        let mut states = BTreeMap::new();
        states.insert(1, make_state(1, vec![Op::Enter(0), Op::Jump(2)]));
        states.insert(2, make_state(2, vec![star]));
        states.insert(50, make_state(50, vec![Op::Ret]));
        states.insert(99, make_state(99, vec![Op::Exit(0), Op::Ret]));
        let mut table = empty_table_with(states, 1);
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
