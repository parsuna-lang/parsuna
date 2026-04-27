//! Lower an [`AnalyzedGrammar`] to a flat [`StateTable`] — the backend-agnostic
//! shape every target language gets compiled from.
//!
//! Lowering runs in three phases:
//! 1. [`build`]: translate rule bodies into a `Block`/`Op` IR with
//!    symbolic block ids and interned FIRST/SYNC sets.
//! 2. [`layout`]: flatten the blocks into numeric state ids, build the
//!    dispatch trees, and compile the lexer DFA.
//! 3. [`fuse`]: splice deterministic jump chains and drop unreachable
//!    states so the emitted tables stay small.

mod build;
pub mod fuse;
mod layout;
pub mod lexer_dfa;

use std::collections::BTreeMap;

use crate::analysis::AnalyzedGrammar;
use crate::grammar::ir::TokenPattern;

pub use lexer_dfa::{DfaState, DEAD, START};

/// Numeric id of a parser state in the final [`StateTable`]. Small dense
/// integers so targets can use them as switch labels.
pub type StateId = u32;
/// Id into [`StateTable::first_sets`] — FIRST sets are interned so two
/// sites using the same set share a single entry in the generated tables.
pub type FirstSetId = u32;
/// Id into [`StateTable::sync_sets`] — SYNC sets are interned the same way.
pub type SyncSetId = u32;

/// A single lookahead sequence: `k` or fewer token-kind ids.
///
/// Sequences are the unit of LL(k) prediction — a FIRST set is a set of
/// these, and dispatch-tree paths consume them one token at a time.
pub type LookaheadSeq = Vec<u16>;

/// An interned FIRST set: every lookahead sequence that can open the
/// dispatch site that owns this id. Sequences are stored as a sorted,
/// deduplicated `Vec` (rather than a `Set`) so backends iterate in a
/// stable order. The id is carried inline so iterating consumers don't
/// need `enumerate`.
#[derive(Clone, Debug)]
pub struct FirstSet {
    /// Pool index of this entry — equal to its position in
    /// [`StateTable::first_sets`].
    pub id: FirstSetId,
    /// The lookahead sequences that comprise this FIRST set.
    pub seqs: Vec<LookaheadSeq>,
    /// True if the generated code references this FIRST set at runtime —
    /// i.e. some `Op::Star` or `Op::Opt` site points at it AND the
    /// grammar's `k > 1` (LL(1) sites inline the FIRST set into a
    /// `match` arm pattern at codegen time, so the constant is never
    /// loaded). `Op::Dispatch` arms also point at FIRST sets but
    /// consume them at lowering time only — the resulting nested switch
    /// arms carry concrete token kinds, not pool ids. Backends emit a
    /// `FIRST_n` constant only when this flag is set.
    pub has_references: bool,
}

/// Pool of interned FIRST sets, indexed by [`FirstSetId`].
pub type FirstSetPool = Vec<FirstSet>;

/// An interned SYNC set: token-kind ids that an `Expect` can recover to.
/// Each entry is a single token, not a sequence. The id is carried
/// inline so iterating consumers don't need `enumerate`.
#[derive(Clone, Debug)]
pub struct SyncSet {
    /// Pool index of this entry — equal to its position in
    /// [`StateTable::sync_sets`].
    pub id: SyncSetId,
    /// Token-kind ids the parser will recover to.
    pub kinds: Vec<u16>,
}

/// Pool of interned SYNC sets, indexed by [`SyncSetId`].
pub type SyncSetPool = Vec<SyncSet>;

/// State id that means "the parser has terminated". Distinct from any real
/// state; the generator chooses the largest u32 so a plain switch over the
/// real state ids never collides.
pub const TERMINATED: StateId = u32::MAX;

/// Everything a backend needs to emit a parser.
#[derive(Clone, Debug)]
pub struct StateTable {
    /// Grammar name, used by backends to pick file/package names.
    pub grammar_name: String,
    /// Non-fragment token metadata, indexed by `kind - 1`.
    pub tokens: Vec<TokenInfo>,
    /// Names of the non-fragment rules in declaration order. A rule's
    /// `RuleKind` id is its index here.
    pub rule_kinds: Vec<String>,
    /// Interned FIRST-set pool. Each entry is a `FirstSet` — a list of
    /// `LookaheadSeq`s (i.e. `Vec<Vec<u16>>`). Index by [`FirstSetId`].
    pub first_sets: FirstSetPool,
    /// Interned SYNC-set pool. Each entry is a `SyncSet` — a flat list of
    /// token ids (`Vec<u16>`). Index by [`SyncSetId`].
    pub sync_sets: SyncSetPool,
    /// Every parser state, keyed by id. A `BTreeMap` so backends walk the
    /// table in deterministic id order.
    pub states: BTreeMap<StateId, State>,
    /// Public entry points: one `(rule_name, start_state)` per non-fragment
    /// rule. Backends emit a `parse_<name>` for each.
    pub entry_states: Vec<(String, StateId)>,
    /// Lookahead required to disambiguate every alternative (LL(k)).
    pub k: usize,
    /// Static upper bound on the number of structural events one
    /// `drive` invocation can push onto the parser's queue between two
    /// yields, computed by [`max_event_burst`] after fuse runs. The
    /// dispatch loop only yields between state bodies (not between
    /// individual emits), so the bound is `max(events_per_state(s))`.
    /// Backends emit this as a `QUEUE_CAP` const that the runtime
    /// uses to pre-size the queue, so the success path never pays a
    /// grow-and-copy. Error recovery (`recover_to` consuming garbage
    /// until a sync token) can push more, in which case the queue
    /// grows from heap. Pending skip tokens between structural events
    /// also pile up dynamically and aren't included.
    pub queue_cap: usize,
    /// The compiled lexer DFA.
    pub lexer_dfa: Vec<DfaState>,
}

/// A single token as seen by the code generator: the name, the resolved
/// pattern (fragments inlined), the skip flag, and a stable numeric kind.
#[derive(Clone, Debug)]
pub struct TokenInfo {
    /// Grammar-declared token name.
    pub name: String,
    /// Token body with every `Ref` inlined, ready for the DFA builder.
    pub pattern: TokenPattern,
    /// True if the token is `[skip]`-annotated: matched but dropped from
    /// the structural stream.
    pub skip: bool,
    /// Dense, 1-based numeric id. `0` is reserved for EOF; lex failures
    /// are surfaced as `Option<TK>` (`None`) at the runtime boundary, so
    /// they consume no kind id.
    pub kind: u16,
}

/// One instruction in a parser state.
///
/// States are small straight-line programs built from these ops; control
/// flow within a state is explicit (`Jump`, `PushRet`/`Ret`) and inter-state
/// flow is fall-through to the next numeric id.
///
/// `PartialEq` is derived so the [`fuse`](crate::lowering::fuse) fixpoint
/// loop can detect when an iteration produced no change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Emit an `Enter` event for this rule-kind id.
    Enter(u16),
    /// Emit an `Exit` event for this rule-kind id.
    Exit(u16),
    /// Consume a token of `kind`; on mismatch, raise an error and recover
    /// to the given SYNC set.
    Expect {
        /// Required token-kind id.
        kind: u16,
        /// Token name, baked in purely for the diagnostic message.
        token_name: String,
        /// SYNC set to recover to on mismatch.
        sync: SyncSetId,
    },
    /// Push a return state. Followed by a `Jump` into a callee; the
    /// callee's trailing `Ret` pops this to resume.
    PushRet(StateId),
    /// Unconditional jump to another state id.
    Jump(StateId),
    /// Return to the state id on top of the return stack, or `TERMINATED`
    /// if the stack is empty.
    Ret,
    /// `*` loop: if lookahead matches `first`, call `body` and re-enter
    /// `head` (the loop-condition state); otherwise fall through to the
    /// continuation.
    Star {
        /// FIRST-set id the body opens with.
        first: FirstSetId,
        /// Body of one iteration. Initially [`Body::State`]; the fuse
        /// branch-inlining pass may move the target state's ops into a
        /// [`Body::Inline`] when the target's only external reference
        /// was this `body`. See [`Body`].
        body: Body,
        /// What to do when the loop exits (lookahead misses `first`).
        /// `Some(state)` is the original lowering shape — `cur = state`.
        /// `None` is a tail call — `cur = ret()` directly. The
        /// [`fuse`](crate::lowering::fuse) tail-call elimination pass
        /// rewrites `Some(s)` to `None` when `s` is a pure-`Ret`
        /// trampoline; by the time the loop misses, every iteration has
        /// already pushed *and* popped its `head` frame, so the stack
        /// is back to whatever was there when the loop was entered.
        ///
        /// Note that `head` itself is never tail-call-eligible: it's
        /// the state hosting this `Op::Star`, so it always has at
        /// least one op (this one).
        cont: Option<StateId>,
        /// State to return to after `body` finishes — the loop-head.
        /// Initially the state that contains this Star, but stays
        /// pointing at the original loop-head if the Star op is later
        /// inlined into another state by the fuse pass.
        head: StateId,
    },
    /// `?` branch: if lookahead matches `first`, call `body` once,
    /// otherwise skip the body. The continuation is either a state to
    /// jump to after `body` returns (push-and-jump) or "tail call".
    Opt {
        /// FIRST-set id the body opens with.
        first: FirstSetId,
        /// Body to call when taken. See [`Body`].
        body: Body,
        /// Continuation. `Some(state)` is the original lowering shape
        /// — `push_ret(state); cur = body` on match, `cur = state` on
        /// miss. `None` is a tail call: `cur = body` on match (the
        /// body's trailing `Ret` returns to *our* caller), and `cur =
        /// ret()` on miss. The [`fuse`](crate::lowering::fuse)
        /// tail-call elimination pass rewrites `Some(s)` to `None`
        /// when `s` is a pure-`Ret` trampoline.
        cont: Option<StateId>,
    },
    /// `Alt` dispatch: pick one arm based on up to `k` tokens of
    /// lookahead, or recover via `sync` on a miss.
    Dispatch {
        /// Decision tree over the lookahead.
        tree: DispatchTree,
        /// SYNC set to recover to on "unexpected token".
        sync: SyncSetId,
        /// Continuation once an arm's body returns (also used for
        /// `Fallthrough` and post-recovery `Error` paths). Same
        /// encoding as [`Op::Opt::cont`]: `Some(state)` is
        /// push-and-jump, `None` is a tail call (no push, return
        /// directly to caller).
        cont: Option<StateId>,
    },
}

/// The "where to call into" half of a branchy op (`Op::Opt`,
/// `Op::Star`, [`DispatchLeaf::Arm`]).
///
/// Layout always emits [`Body::State`] — a normal state-id transition
/// the dispatch loop bounces through. The fuse branch-inlining pass
/// may rewrite that to [`Body::Inline`] when the target state had only
/// one external reference (this op): the target's ops move *into* the
/// op, and the now-orphan target state is dropped by `eliminate_dead`.
///
/// The codegens pattern-match: `State(s)` emits `cur = s;`,
/// `Inline(ops)` recursively emits the inlined ops at the call site,
/// saving the dispatch hop into a state that only one place ever
/// reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Transfer control to a separate state (and let the dispatch
    /// loop pick it up on the next iteration).
    State(StateId),
    /// Run these ops directly inside the calling branch arm. The
    /// final op is the inlined state's terminator (`Ret`/`Jump`/
    /// another branchy op) and is responsible for the post-body
    /// transition.
    Inline(Vec<Op>),
}

impl Body {
    /// True iff the body is still a [`Body::State`] — i.e. not yet
    /// inlined. Convenience for the few sites that just want to know
    /// whether a state-id is in play.
    pub fn is_state(&self) -> bool {
        matches!(self, Body::State(_))
    }
    /// The state id this body refers to, if it hasn't been inlined.
    pub fn state(&self) -> Option<StateId> {
        match self {
            Body::State(s) => Some(*s),
            Body::Inline(_) => None,
        }
    }
}

/// Terminal action at a leaf of a [`DispatchTree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchLeaf {
    /// Take the arm whose body starts at this body (push the
    /// dispatch's `next` as the return target first).
    Arm(Body),
    /// No arm matched, but the dispatch is nullable — continue at the
    /// dispatch's `next` without emitting an error.
    Fallthrough,
    /// No arm matched and the dispatch is not nullable — report an
    /// "unexpected token" error and recover via the dispatch's `sync`
    /// set, then continue at `next`.
    Error,
}

/// An alternative-dispatch decision tree over up to `k` lookahead tokens.
///
/// Each `Switch` inspects `look(depth).kind` and branches into sub-trees.
/// The flat shape maps cleanly onto nested `switch`/`match` statements in
/// every target language. Built by [`build_dispatch_tree`] from a set of
/// `(FIRST-set-id, target-state)` pairs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchTree {
    /// A terminal action: commit to an arm, fall through, or error.
    Leaf(DispatchLeaf),
    /// Inspect the lookahead at `depth` and branch.
    Switch {
        /// Which lookahead slot to inspect (`0` = current token, `1` =
        /// the one after that, up to `k-1`).
        depth: u8,
        /// `(token kind, sub-tree)` pairs for each matched lookahead.
        /// Kept sorted by kind so a backend can compile to a jump table.
        arms: Vec<(u16, DispatchTree)>,
        /// Action when no arm matches at this depth.
        default: DispatchLeaf,
    },
}

/// Build a dispatch tree from the `(first_set_id, target)` pairs of an
/// `Alt` and whether any arm is nullable.
///
/// `has_eps = true` changes the outer default from `Error` to
/// `Fallthrough`: a nullable alt means "none of the arms matched, but
/// that's OK — drop through".
pub fn build_dispatch_tree(
    arms: &[(FirstSetId, StateId)],
    first_sets: &[FirstSet],
    has_eps: bool,
) -> DispatchTree {
    let outer_default = if has_eps {
        DispatchLeaf::Fallthrough
    } else {
        DispatchLeaf::Error
    };
    let mut entries: Vec<(Vec<u16>, StateId)> = Vec::new();
    for (fid, target) in arms {
        for seq in &first_sets[*fid as usize].seqs {
            entries.push((seq.clone(), *target));
        }
    }
    build_trie(&entries, 0, outer_default)
}

fn build_trie(
    entries: &[(Vec<u16>, StateId)],
    depth: usize,
    outer_default: DispatchLeaf,
) -> DispatchTree {
    // An entry whose sequence length equals the current depth terminates
    // here: the first `depth` tokens fully identified the arm, so anything
    // that doesn't match a deeper prefix should still take this branch.
    // That's why we capture it as `node_default` for the sub-tree rather
    // than inheriting the outer one.
    let mut surviving: Vec<(Vec<u16>, StateId)> = Vec::new();
    let mut terminal: Option<DispatchLeaf> = None;
    for entry in entries {
        if entry.0.len() == depth {
            terminal = Some(DispatchLeaf::Arm(Body::State(entry.1)));
            break;
        }
        surviving.push(entry.clone());
    }

    let node_default = terminal.unwrap_or(outer_default);

    if surviving.is_empty() {
        return DispatchTree::Leaf(node_default);
    }

    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<u16, Vec<(Vec<u16>, StateId)>> = BTreeMap::new();
    for entry in &surviving {
        by_kind
            .entry(entry.0[depth])
            .or_default()
            .push(entry.clone());
    }

    let arms: Vec<(u16, DispatchTree)> = by_kind
        .into_iter()
        .map(|(k, subentries)| (k, build_trie(&subentries, depth + 1, node_default.clone())))
        .collect();

    DispatchTree::Switch {
        depth: depth as u8,
        arms,
        default: node_default,
    }
}

/// A numbered state holding one straight-line program of `Op`s.
///
/// `label` is a human-readable tag (rule name plus what the state is doing,
/// e.g. `expr:call:atom`) used in the debug dumps and as a comment in
/// generated code.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The state's id — matches its key in [`StateTable::states`].
    pub id: StateId,
    /// Human-readable tag (e.g. `expr:alt0:call:atom`). Used for debug
    /// dumps and emitted as a comment next to the `case` in generated code.
    pub label: String,
    /// Straight-line ops executed when the parser enters this state.
    pub ops: Vec<Op>,
}

/// Lower an analyzed grammar into a [`StateTable`]: build → layout → fuse.
pub fn lower(ag: &AnalyzedGrammar) -> StateTable {
    let program = build::build(ag);
    let mut table = layout::layout(program, ag);
    fuse::fuse(&mut table);
    table.queue_cap = max_event_burst(&table);
    mark_first_set_references(&mut table);
    table
}

/// Compute the maximum number of structural events that one state
/// body can push onto the parser's queue when executed end-to-end.
///
/// The dispatch loop only yields between state bodies, so this is
/// the per-yield queue burst on the success path. With branch
/// inlining ([`fuse::inline_branch_targets`]) inlined `Body::Inline`
/// ops execute as part of the calling body and therefore contribute
/// to that body's burst — the recursion handles them.
///
/// `Body::State` transitions count as 0 here: the called state runs
/// in a *different* dispatch iteration and contributes to its own
/// burst, not ours. Skip tokens accumulating between structural
/// events also aren't included — they're input-dependent.
pub fn max_event_burst(table: &StateTable) -> usize {
    table
        .states
        .values()
        .map(|s| events_in_ops(&s.ops))
        .max()
        .unwrap_or(0)
}

fn events_in_ops(ops: &[Op]) -> usize {
    ops.iter().map(events_in_op).sum()
}

fn events_in_op(op: &Op) -> usize {
    match op {
        Op::Enter(_) | Op::Exit(_) | Op::Expect { .. } => 1,
        Op::PushRet(_) | Op::Jump(_) | Op::Ret => 0,
        Op::Star { body, .. } | Op::Opt { body, .. } => events_in_body(body),
        Op::Dispatch { tree, .. } => max_arm_events(tree),
    }
}

fn events_in_body(body: &Body) -> usize {
    match body {
        Body::State(_) => 0,
        Body::Inline(ops) => events_in_ops(ops),
    }
}

fn max_arm_events(tree: &DispatchTree) -> usize {
    fn arm(leaf: &DispatchLeaf) -> usize {
        match leaf {
            DispatchLeaf::Arm(b) => events_in_body(b),
            // Fallthrough/Error transition out of the dispatch state —
            // 0 events emitted in *this* state's body.
            DispatchLeaf::Fallthrough | DispatchLeaf::Error => 0,
        }
    }
    match tree {
        DispatchTree::Leaf(l) => arm(l),
        DispatchTree::Switch { arms, default, .. } => arm(default).max(
            arms.iter()
                .map(|(_, sub)| max_arm_events(sub))
                .max()
                .unwrap_or(0),
        ),
    }
}

/// Set [`FirstSet::has_references`] on every entry the runtime will
/// actually consult. See the field's doc comment for the precise rule.
fn mark_first_set_references(table: &mut StateTable) {
    if table.k == 1 {
        return;
    }
    let mut referenced = std::collections::BTreeSet::new();
    for state in table.states.values() {
        for op in &state.ops {
            match op {
                Op::Star { first, .. } | Op::Opt { first, .. } => {
                    referenced.insert(*first);
                }
                _ => {}
            }
        }
    }
    for f in table.first_sets.iter_mut() {
        f.has_references = referenced.contains(&f.id);
    }
}

impl State {
    /// Render the state as a single comment line: label plus the ops
    /// joined with `;`. Used by the debug dumper and by some backends when
    /// they want to annotate generated `case` arms.
    pub fn comment(&self) -> String {
        let body = if self.ops.is_empty() {
            "<empty>".to_string()
        } else {
            self.ops
                .iter()
                .map(format_op)
                .collect::<Vec<_>>()
                .join(" ; ")
        };
        format!("{}  {}", self.label, body)
    }
}

fn format_op(op: &Op) -> String {
    match op {
        Op::Enter(k) => format!("Enter({})", k),
        Op::Exit(k) => format!("Exit({})", k),
        Op::Expect {
            kind, token_name, ..
        } => format!("Expect({} /*{}*/)", kind, token_name),
        Op::PushRet(r) => format!("PushRet({})", r),
        Op::Jump(n) => format!("Jump({})", n),
        Op::Ret => "Ret".to_string(),
        Op::Star { body, .. } => format!("Star -> {}", body_label(body)),
        Op::Opt { body, .. } => format!("Opt -> {}", body_label(body)),
        Op::Dispatch { tree, .. } => format!("Dispatch[{}]", dispatch_tree_shape(tree)),
    }
}

fn body_label(body: &Body) -> String {
    match body {
        Body::State(s) => s.to_string(),
        Body::Inline(_) => "<inlined>".into(),
    }
}

fn dispatch_tree_shape(tree: &DispatchTree) -> String {
    let (leaves, depth) = tree_metrics(tree);
    format!("{} leaves, depth {}", leaves, depth)
}

fn tree_metrics(tree: &DispatchTree) -> (usize, usize) {
    match tree {
        DispatchTree::Leaf(_) => (1, 0),
        DispatchTree::Switch { arms, .. } => {
            let (mut leaf_count, mut max_depth) = (1, 1);
            for (_, sub) in arms {
                let (l, d) = tree_metrics(sub);
                leaf_count += l;
                if d + 1 > max_depth {
                    max_depth = d + 1;
                }
            }
            (leaf_count, max_depth)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::grammar::parse_grammar;

    fn analyze_src(src: &str) -> crate::AnalyzedGrammar {
        let g = parse_grammar(src).expect("parse");
        let outcome = analyze(g);
        assert!(!outcome.has_errors(), "{:?}", outcome.diagnostics);
        outcome.grammar.expect("grammar")
    }

    #[test]
    fn lower_minimal_grammar_produces_states_and_entry() {
        let ag = analyze_src("T = \"t\"; main = T;");
        let st = lower(&ag);
        assert_eq!(st.grammar_name, ag.grammar.name);
        assert_eq!(st.tokens.len(), 1);
        assert_eq!(st.tokens[0].name, "T");
        assert_eq!(st.tokens[0].kind, 1);
        assert!(!st.states.is_empty());
        assert_eq!(st.entry_states.len(), 1);
        assert_eq!(st.entry_states[0].0, "main");
        assert_eq!(st.k, 1);
    }

    #[test]
    fn lower_drops_fragment_tokens_from_kind_table() {
        let ag = analyze_src("_D = '0'..'9'; NUM = _D+; main = NUM;");
        let st = lower(&ag);
        assert!(!st.tokens.iter().any(|t| t.name == "_D"));
        assert!(st.tokens.iter().any(|t| t.name == "NUM"));
    }

    #[test]
    fn lower_drops_fragment_rules_from_rule_kinds() {
        let ag = analyze_src("T = \"t\"; _helper = T; main = T _helper;");
        let st = lower(&ag);
        assert!(!st.rule_kinds.iter().any(|n| n == "_helper"));
        assert!(st.rule_kinds.iter().any(|n| n == "main"));
    }

    #[test]
    fn lexer_dfa_is_built_for_each_token() {
        let ag = analyze_src("A = \"a\"; B = \"b\"; main = A B;");
        let st = lower(&ag);
        // 1 dead + start + at least one accept per token = ≥ 4 states
        // start + at least one accept state per token (no dead in the vec).
        assert!(st.lexer_dfa.len() >= 3);
        assert_eq!(st.lexer_dfa[0].id, crate::lowering::lexer_dfa::START);
    }

    #[test]
    fn skip_tokens_get_kind_but_arent_referenced_by_rules() {
        let ag = analyze_src("WS = \" \"+ [skip]; T = \"t\"; main = T;");
        let st = lower(&ag);
        let ws = st.tokens.iter().find(|t| t.name == "WS").expect("WS");
        assert!(ws.skip);
    }

    #[test]
    fn entry_states_one_per_non_fragment_rule() {
        let ag = analyze_src("T = \"t\"; main = T; other = T;");
        let st = lower(&ag);
        assert_eq!(st.entry_states.len(), 2);
        let names: Vec<&str> = st.entry_states.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"other"));
    }
}
