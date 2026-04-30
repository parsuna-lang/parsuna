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
use crate::lowering::{
    build_dispatch_tree, Body, FirstSet, Instr, ModeInfo, State, StateId, StateTable, Tail,
};

/// Assign state ids to every op in every block, resolve inter-block
/// targets, and compile the lexer DFA. Produces the [`StateTable`]
/// that downstream passes (`optimize`, backends) operate on. `dopts`
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
    // 1. Each block reserves exactly `ops.len()` slots — its last op is
    // emitted in *tail form* (Ret instead of Jump-to-trailing-ret-state,
    // tail-call instead of PushRet+Jump, `cont: None` for branchy ops),
    // so no separate trailing `[Ret]` state is needed. The runtime
    // requires every drive call to emit one event, and a standalone
    // `[Ret]`-only state reached with an empty stack would emit nothing
    // before hitting TERMINATED — baking the tail form into layout
    // makes that state shape impossible to construct, regardless of
    // which optimizer passes run later. Empty blocks still need one
    // slot to carry their entry id; we emit a single `[Ret]` for them.
    let mut entry: HashMap<BlockId, StateId> = HashMap::new();
    let mut cursor: StateId = 1;
    for bid in &order {
        entry.insert(*bid, cursor);
        let n = prog.blocks[bid.0 as usize].ops.len().max(1);
        cursor += n as StateId;
    }

    let mut states: std::collections::BTreeMap<StateId, State> = std::collections::BTreeMap::new();
    let mut next_id: StateId = 1;
    for bid in &order {
        let block = &prog.blocks[bid.0 as usize];
        if block.ops.is_empty() {
            // An empty block lowers to a single Ret. Reachable only via
            // a caller's `PushRet(...) ; Jump(here)`, so the Ret pops
            // back to the caller's continuation — never to TERMINATED
            // unless the caller deliberately set up that path.
            states.insert(
                next_id,
                State {
                    id: next_id,
                    label: format!("{}:empty", block.label_prefix),
                    body: Body {
                        instrs: Vec::new(),
                        tail: Tail::Ret,
                    },
                },
            );
            next_id += 1;
            continue;
        }
        let total = block.ops.len();
        for (i, op) in block.ops.iter().enumerate() {
            // Fall-through defaults to the next id — one op per state
            // means "after this op, go to the op below me" is just id + 1.
            // Ops that branch (Dispatch, Opt, Star) override this via
            // their own `next`/body targets. The last op of the block
            // takes its tail form, so its `fall` is unused.
            let here = next_id;
            let fall = here + 1;
            let is_tail = i + 1 == total;
            states.insert(
                here,
                State {
                    id: here,
                    label: block.op_labels[i].clone(),
                    body: lower_op(
                        op,
                        here,
                        fall,
                        is_tail,
                        &entry,
                        &prog.rule_entry,
                        &prog.first_sets,
                    ),
                },
            );
            next_id += 1;
        }
    }

    // Expose entry points for public (non-fragment) rules only — fragments
    // are inlined call targets and have no standalone public API.
    let entry_states: Vec<(String, StateId)> = prog
        .rule_order
        .iter()
        .filter(|n| prog.public_rule_names.contains(n.as_str()))
        .map(|n| (n.clone(), entry[&prog.rule_entry[n]]))
        .collect();

    // One DFA per declared lexer mode. A token belongs to its mode_id;
    // the per-mode token vec is what the DFA builder sees. We hold on
    // to the original `kind` ids — mode-local DFAs still emit the same
    // global TokenKind ids, so dispatch can look up the matched kind
    // without an indirection.
    let modes: Vec<ModeInfo> = prog
        .mode_names
        .iter()
        .enumerate()
        .map(|(id, name)| {
            let id = id as u32;
            let mode_tokens: Vec<_> = prog
                .tokens
                .iter()
                .filter(|t| t.mode_id == id)
                .cloned()
                .collect();
            let dfa = lexer_dfa::compile_with_opts(&mode_tokens, dopts);
            ModeInfo {
                id,
                name: name.clone(),
                dfa,
            }
        })
        .collect();

    StateTable {
        grammar_name: ag.grammar.name.clone(),
        tokens: prog.tokens,
        rule_kinds: prog.rules,
        first_sets: prog.first_sets,
        sync_sets: prog.sync_sets,
        states,
        entry_states,
        k: ag.k,
        modes,
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

/// Translate one block-level `Op` into the [`Body`] of a single state.
///
/// Straight-line ops (Enter/Exit/Expect/Call) emit their action as an
/// `Instr` (or sometimes an empty `instrs` list with a tail `Jump`),
/// then a `Tail` that's `Jump(fall)` for non-tail ops or `Ret` for the
/// block's last op. Branchy ops (Opt/Star/Dispatch) become the body's
/// tail directly — they encode their continuation via their own `cont`
/// field rather than a trailing jump.
fn lower_op(
    op: &Op,
    here: StateId,
    fall: StateId,
    is_tail: bool,
    entry: &HashMap<BlockId, StateId>,
    rules: &HashMap<String, BlockId>,
    first_sets: &[FirstSet],
) -> Body {
    // The "post-op terminator" — what closes the body for non-branchy
    // ops. For non-tail ops this is `Jump(fall)` (fall-through to the
    // next state in the block). For the block's last op this is `Ret`,
    // so the op's drive() call also pops the caller's continuation in
    // the same step — the runtime never lands on a standalone
    // `[Ret]`-only state with an empty stack.
    let after = if is_tail {
        Tail::Ret
    } else {
        Tail::Jump(fall)
    };
    // Branchy ops encode the post-op transition via their `cont` field
    // — `Some(fall)` is push-and-jump, `None` is tail call (the
    // body's `Ret` pops the *caller's* continuation directly).
    let cont = if is_tail { None } else { Some(fall) };
    match op {
        Op::Enter { kind } => Body {
            instrs: vec![Instr::Enter(*kind)],
            tail: after,
        },
        Op::Exit { kind } => Body {
            instrs: vec![Instr::Exit(*kind)],
            tail: after,
        },
        Op::Expect {
            kind,
            token_name,
            sync,
        } => Body {
            instrs: vec![Instr::Expect {
                kind: *kind,
                token_name: token_name.clone(),
                sync: *sync,
            }],
            tail: after,
        },
        Op::Call { target } => {
            // Call = save fall-through as the return state, then jump
            // to the callee's entry. When the callee's trailing Ret
            // runs, it pops `fall` and resumes here. As a tail call
            // (last op of block) we skip the push: the callee's Ret
            // pops *our* caller's continuation directly.
            let target_state = resolve(target, entry, rules);
            if is_tail {
                Body {
                    instrs: Vec::new(),
                    tail: Tail::Jump(target_state),
                }
            } else {
                Body {
                    instrs: vec![Instr::PushRet(fall)],
                    tail: Tail::Jump(target_state),
                }
            }
        }
        Op::Opt { first, body } => Body {
            instrs: Vec::new(),
            tail: Tail::Opt {
                first: *first,
                body: Box::new(Body::jump(resolve(body, entry, rules))),
                cont,
            },
        },
        Op::Star { first, body } => Body {
            instrs: Vec::new(),
            tail: Tail::Star {
                first: *first,
                body: Box::new(Body::jump(resolve(body, entry, rules))),
                cont,
                // Loop-head defaults to the state we're being placed in.
                // If the optimizer later splices this Star elsewhere, the
                // original `here` state stays alive (it's referenced via
                // `head`) so the body's Ret has somewhere to land.
                head: here,
            },
        },
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
            Body {
                instrs: Vec::new(),
                tail: Tail::Dispatch {
                    tree,
                    sync: *sync,
                    cont,
                },
            }
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
    use crate::lowering::{Instr, Tail};

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
    fn block_last_op_uses_tail_form() {
        // Layout no longer emits a standalone trailing `[Ret]` state per
        // block — the block's last op carries its own terminator (a
        // direct `Ret`, a tail-call `Jump`, or `cont: None` for branchy
        // ops). The state at `entry_id + ops.len() - 1` is the block's
        // last op, and its body's tail should be `Ret` (for the rules
        // in this grammar, every body ends with `Exit`, which lowers to
        // `Body { instrs: [Exit], tail: Ret }`).
        let (ag, st) = lay("T = \"t\"; a = T; main = a T;");
        let prog = build(&ag);
        for (rule_name, entry_id) in &st.entry_states {
            let bid = prog.rule_entry[rule_name];
            let block = &prog.blocks[bid.0 as usize];
            let last_id = *entry_id + (block.ops.len() - 1) as StateId;
            let last = st.states.get(&last_id).expect("last state of block");
            assert!(
                matches!(last.body.tail, Tail::Ret),
                "block last op didn't tail in Ret: {:?}",
                last.body
            );
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
        // The Expect state's body should be { instrs: [Expect], tail: Jump(next) }.
        let (_, st) = lay("T = \"t\"; main = T;");
        let entry_id = st.entry_states[0].1;
        let entry = st.states.get(&entry_id).expect("entry state");
        // The Enter wrapper is first; find an Expect Body anywhere with a Jump tail.
        let saw_expect_then_jump = st.states.values().any(|s| {
            s.body.instrs.len() == 1
                && matches!(s.body.instrs[0], Instr::Expect { .. })
                && matches!(s.body.tail, Tail::Jump(_))
        });
        assert!(
            saw_expect_then_jump,
            "no Expect followed by Jump anywhere; entry={:?}",
            entry.body
        );
    }

    #[test]
    fn lexer_dfa_compiled_alongside_state_table() {
        let (_, st) = lay("T = \"t\"; main = T;");
        // DFA always has at least the dead state plus a real start.
        assert!(!st.lexer_dfa().is_empty());
        assert_eq!(st.lexer_dfa()[0].id, crate::lowering::lexer_dfa::START);
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
