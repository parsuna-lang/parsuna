//! Flatten the block-level IR from `build` into numbered parser states.
//!
//! Each `Block` becomes a contiguous run of states (one per op, plus a
//! trailing `Ret`), and targets that were symbolic `BlockId`/`Rule(name)`
//! references in `build` are resolved to concrete `StateId`s. The
//! `Dispatch` op, which was a flat list of `(first_set_id, target)` arms
//! at the block level, is expanded into a [`crate::lowering::DispatchTree`]
//! here so the generated code can switch on lookahead token-by-token.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::analysis::AnalyzedGrammar;
use crate::lowering::build::{Block, BlockId, Op, Program, Target};
use crate::lowering::lexer_dfa::{self, DfaOpts};
use crate::lowering::{build_dispatch_tree, FirstSet, Op as StateOp, State, StateId, StateTable};

/// Assign state ids to every op in every block, resolve inter-block
/// targets, and compile the lexer DFA. Produces the [`StateTable`]
/// that downstream passes (`fuse`, backends) operate on. `dopts`
/// controls the lexer-DFA optimization passes; layout itself has no
/// per-pass knobs of its own.
pub fn layout(prog: Program, ag: &AnalyzedGrammar, dopts: DfaOpts) -> StateTable {
    // BFS from each public rule's entry to order the blocks. This keeps a
    // rule and everything it transitively reaches adjacent in the state
    // numbering, which makes debug dumps and generated switch tables
    // easier to read.
    let mut order: Vec<BlockId> = Vec::new();
    let mut seen: HashSet<BlockId> = HashSet::new();
    for name in &prog.rule_order {
        let root = prog.rule_entry[name];
        bfs(root, &prog.blocks, &mut order, &mut seen);
    }

    // Pre-assign entry state ids for each block so we can resolve
    // cross-block targets in the next pass without forward references.
    // Id 0 is reserved for the TERMINATED sentinel, so numbering starts at
    // 1. Each block reserves one slot per op plus one for the trailing
    // `Ret` state.
    let mut entry: HashMap<BlockId, StateId> = HashMap::new();
    let mut cursor: StateId = 1;
    for bid in &order {
        entry.insert(*bid, cursor);
        cursor += prog.blocks[bid.0 as usize].ops.len() as StateId + 1;
    }

    let mut states: std::collections::BTreeMap<StateId, State> = std::collections::BTreeMap::new();
    let mut next_id: StateId = 1;
    for bid in &order {
        let block = &prog.blocks[bid.0 as usize];
        for (i, op) in block.ops.iter().enumerate() {
            // Fall-through defaults to the next id — one op per state
            // means "after this op, go to the op below me" is just id + 1.
            // Ops that branch (Dispatch, Opt, Star) override this via
            // their own `next`/body targets.
            let here = next_id;
            let fall = here + 1;
            states.insert(
                here,
                State {
                    id: here,
                    label: block.op_labels[i].clone(),
                    ops: lower_op(op, here, fall, &entry, &prog.rule_entry, &prog.first_sets),
                },
            );
            next_id += 1;
        }

        // Synthetic trailing state: every block ends with a `Ret` so a
        // caller's `PushRet(fall)` followed by `Jump(block.entry)` lands
        // back at `fall` when the callee finishes.
        states.insert(
            next_id,
            State {
                id: next_id,
                label: format!("{}:ret", block.label_prefix),
                ops: vec![StateOp::Ret],
            },
        );
        next_id += 1;
    }

    // Expose entry points for public (non-fragment) rules only — fragments
    // are inlined call targets and have no standalone public API.
    let entry_states: Vec<(String, StateId)> = prog
        .rule_order
        .iter()
        .filter(|n| prog.public_rule_names.contains(n.as_str()))
        .map(|n| (n.clone(), entry[&prog.rule_entry[n]]))
        .collect();

    let lexer_dfa = lexer_dfa::compile_with_opts(&prog.tokens, dopts);

    StateTable {
        grammar_name: ag.grammar.name.clone(),
        tokens: prog.tokens,
        rule_kinds: prog.rules,
        first_sets: prog.first_sets,
        sync_sets: prog.sync_sets,
        states,
        entry_states,
        k: ag.k,
        // Filled in by lower() after fuse via max_event_burst.
        queue_cap: 0,
        lexer_dfa,
    }
}

/// BFS the block graph from `start`, appending blocks to `order` in
/// breadth-first discovery order. Shared with the ordering pass above so
/// `seen` carries across rule roots and every block is visited exactly
/// once.
fn bfs(start: BlockId, blocks: &[Block], order: &mut Vec<BlockId>, seen: &mut HashSet<BlockId>) {
    let mut queue: VecDeque<BlockId> = VecDeque::new();
    queue.push_back(start);
    while let Some(bid) = queue.pop_front() {
        if !seen.insert(bid) {
            continue;
        }
        order.push(bid);
        for op in &blocks[bid.0 as usize].ops {
            visit_block_targets(op, &mut |child| {
                if !seen.contains(&child) {
                    queue.push_back(child);
                }
            });
        }
    }
}

/// Call `f` for every `BlockId` that `op` can transfer control to. Only
/// `Target::Block` values count — `Target::Rule` is resolved through
/// `rule_entry` later, so following those here would just revisit rule
/// roots we already seeded the BFS with.
fn visit_block_targets(op: &Op, f: &mut dyn FnMut(BlockId)) {
    let mut visit = |t: &Target| {
        if let Target::Block(b) = t {
            f(*b);
        }
    };
    match op {
        Op::Enter { .. } | Op::Exit { .. } | Op::Expect { .. } => {}
        Op::Call { target } => visit(target),
        Op::Opt { body, .. } | Op::Star { body, .. } => visit(body),
        Op::Dispatch { arms, .. } => {
            for (_, t) in arms {
                visit(t);
            }
        }
    }
}

/// Translate one block-level `Op` into the `StateOp` sequence of a single
/// state.
///
/// Straight-line ops (Enter/Exit/Expect/Call) emit their action followed
/// by an explicit `Jump(fall)` so the state always has an exit; the
/// branchy ops (Opt/Star/Dispatch) encode `next` directly and don't need
/// a trailing jump.
fn lower_op(
    op: &Op,
    here: StateId,
    fall: StateId,
    entry: &HashMap<BlockId, StateId>,
    rules: &HashMap<String, BlockId>,
    first_sets: &[FirstSet],
) -> Vec<StateOp> {
    match op {
        Op::Enter { kind } => vec![StateOp::Enter(*kind), StateOp::Jump(fall)],
        Op::Exit { kind } => vec![StateOp::Exit(*kind), StateOp::Jump(fall)],
        Op::Expect {
            kind,
            token_name,
            sync,
        } => vec![
            StateOp::Expect {
                kind: *kind,
                token_name: token_name.clone(),
                sync: *sync,
            },
            StateOp::Jump(fall),
        ],
        Op::Call { target } => vec![
            // Call = save fall-through as the return state, then jump to
            // the callee's entry. When the callee hits its trailing Ret,
            // it pops `fall` and resumes here.
            StateOp::PushRet(fall),
            StateOp::Jump(resolve(target, entry, rules)),
        ],
        Op::Opt { first, body } => vec![StateOp::Opt {
            first: *first,
            body: super::Body::State(resolve(body, entry, rules)),
            // Layout always emits the push-and-jump shape. The fuse
            // tail-call pass rewrites this to `None` when `fall`
            // turns out to be a pure-`Ret` trampoline.
            cont: Some(fall),
        }],
        Op::Star { first, body } => vec![StateOp::Star {
            first: *first,
            body: super::Body::State(resolve(body, entry, rules)),
            cont: Some(fall),
            // Loop-head defaults to the state we're being placed in.
            // If the fuse pass later splices this Star elsewhere, the
            // original `here` state stays alive (it's referenced via
            // `head`) so the body's Ret has somewhere to land.
            head: here,
        }],
        Op::Dispatch {
            arms,
            has_eps,
            sync,
        } => {
            // Dispatch arms carry `FirstSetId`s into the shared pool.
            // `build_dispatch_tree` expands them into an LL(k) trie the
            // backend can emit as nested switches.
            let resolved: Vec<(u32, StateId)> = arms
                .iter()
                .map(|(f, t)| (*f, resolve(t, entry, rules)))
                .collect();
            let tree = build_dispatch_tree(&resolved, first_sets, *has_eps);
            vec![StateOp::Dispatch {
                tree,
                sync: *sync,
                cont: Some(fall),
            }]
        }
    }
}

/// Look up the concrete `StateId` that a symbolic target maps to: either a
/// local block (resolved via `entry`) or a call by rule name (via
/// `rules` → block id → `entry`).
fn resolve(
    t: &Target,
    entry: &HashMap<BlockId, StateId>,
    rules: &HashMap<String, BlockId>,
) -> StateId {
    match t {
        Target::Block(b) => entry[b],
        Target::Rule(n) => entry[&rules[n]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::grammar::parse_grammar;
    use crate::lowering::build::build;
    use crate::lowering::Op as StateOp;

    fn analyze_src(src: &str) -> AnalyzedGrammar {
        let g = parse_grammar(src).expect("parse");
        let outcome = analyze(g);
        assert!(!outcome.has_errors(), "{:?}", outcome.diagnostics);
        outcome.grammar.expect("grammar")
    }

    fn lay(src: &str) -> (AnalyzedGrammar, StateTable) {
        let ag = analyze_src(src);
        let prog = build(&ag);
        let st = layout(prog, &ag, DfaOpts::default());
        (ag, st)
    }

    #[test]
    fn state_ids_are_dense_starting_from_one() {
        let (_, st) = lay("T = \"t\"; main = T;");
        let ids: Vec<StateId> = st.states.keys().copied().collect();
        // BTreeMap iterates in sorted order; ids should start at 1 and be contiguous.
        assert!(!ids.is_empty());
        assert_eq!(ids[0], 1);
        for w in ids.windows(2) {
            assert_eq!(w[1], w[0] + 1, "non-dense: {:?}", ids);
        }
    }

    #[test]
    fn every_block_ends_in_a_ret_state() {
        // For each entry state, walking the block linearly, the last state of
        // that block must be a Ret.
        let (ag, st) = lay("T = \"t\"; a = T; main = a T;");
        let prog = build(&ag);
        // Layout doesn't expose block boundaries directly, but we can check
        // each entry: from entry id, count `prog.blocks[bid].ops.len() + 1`
        // states forward; the last must be a single Ret.
        for (rule_name, entry_id) in &st.entry_states {
            let bid = prog.rule_entry[rule_name];
            let block = &prog.blocks[bid.0 as usize];
            let last_id = *entry_id + block.ops.len() as StateId; // entry + N ops → trailing ret slot
            let last = st.states.get(&last_id).expect("trailing state");
            assert_eq!(last.ops.len(), 1);
            assert!(matches!(last.ops[0], StateOp::Ret), "{:?}", last.ops);
        }
    }

    #[test]
    fn entry_states_only_for_public_rules() {
        let (_, st) = lay("T = \"t\"; _helper = T; main = T _helper;");
        let names: Vec<&str> = st.entry_states.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(!names.contains(&"_helper"));
    }

    #[test]
    fn straight_line_op_emits_action_followed_by_jump_to_fallthrough() {
        // `main = T;` — the body is a single Expect then the block's Ret.
        // The Expect state should have ops [Expect, Jump(next)].
        let (_, st) = lay("T = \"t\"; main = T;");
        let entry_id = st.entry_states[0].1;
        let entry = st.states.get(&entry_id).expect("entry state");
        // The Enter wrapper is first; find an Expect anywhere in the block.
        let saw_expect_then_jump = st.states.values().any(|s| {
            s.ops.len() == 2
                && matches!(s.ops[0], StateOp::Expect { .. })
                && matches!(s.ops[1], StateOp::Jump(_))
        });
        assert!(
            saw_expect_then_jump,
            "no Expect followed by Jump anywhere; entry={:?}",
            entry.ops
        );
    }

    #[test]
    fn lexer_dfa_compiled_alongside_state_table() {
        let (_, st) = lay("T = \"t\"; main = T;");
        // DFA always has at least the dead state plus a real start.
        assert!(!st.lexer_dfa.is_empty());
        assert_eq!(st.lexer_dfa[0].id, crate::lowering::lexer_dfa::START);
    }

    #[test]
    fn grammar_name_carried_into_state_table() {
        let mut g = parse_grammar("T = \"t\"; main = T;").expect("parse");
        g.name = "custom_name".into();
        let outcome = analyze(g);
        let ag = outcome.grammar.unwrap();
        let prog = build(&ag);
        let st = layout(prog, &ag, DfaOpts::default());
        assert_eq!(st.grammar_name, "custom_name");
    }

    #[test]
    fn k_value_carried_from_analyzed_grammar() {
        let (ag, st) = lay("T = \"t\"; main = T;");
        assert_eq!(st.k, ag.k);
    }
}
