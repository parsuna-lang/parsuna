//! First lowering phase: rule bodies → symbolic `Block`/`Op` IR.
//!
//! The output of this phase is a [`Program`] whose blocks reference one
//! another by `BlockId` and reference rule entry points by name. It
//! interns FIRST and SYNC sets so downstream code can refer to them by a
//! small integer, which keeps the generated tables compact and lets two
//! sites share a single table entry when they happen to have the same set.
//!
//! `layout.rs` picks up from here to assign concrete state ids and resolve
//! every `Target` into a `StateId`.

use std::collections::{BTreeSet, HashMap};

use crate::analysis::{self, AnalyzedGrammar, EOF_MARKER};
use crate::grammar::ir::*;
use crate::lowering::{
    FirstSet, FirstSetId, FirstSetPool, LookaheadSeq, ModeActionInfo, SyncSet, SyncSetId,
    SyncSetPool, TokenInfo,
};

/// Index into [`Program::blocks`]. Used wherever a block needs to refer to
/// another one before state ids exist.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(
    /// Index into [`Program::blocks`].
    pub u32,
);

/// Control-transfer target used inside a [`Block`]'s ops.
///
/// `Rule(name)` is a call to a top-level rule; the actual block id is
/// looked up via [`Program::rule_entry`] during layout. `Block(id)` is a
/// direct reference to a local block (arm body, loop body, etc.).
#[derive(Clone, Debug)]
pub enum Target {
    /// Call by rule name. Resolved via [`Program::rule_entry`] during layout.
    Rule(String),
    /// Direct reference to a block in the same `Program`.
    Block(BlockId),
}

/// Op in the block-level IR — one level above the state-machine ops in
/// [`crate::lowering::Op`]. `Call` and `Target::Rule` still exist here
/// because we haven't resolved rule calls to block ids yet, and dispatch
/// arms are still a flat list rather than a tree.
#[derive(Clone, Debug)]
pub enum Op {
    /// Emit an `Enter` event for rule-kind `kind`.
    Enter {
        /// Rule-kind id (index into [`Program::rules`]).
        kind: u16,
    },
    /// Emit an `Exit` event for rule-kind `kind`.
    Exit {
        /// Rule-kind id (index into [`Program::rules`]).
        kind: u16,
    },
    /// Consume a token of `kind`; on mismatch, emit an error and recover
    /// to `sync`. `label` is the optional name from a `name:NAME` form
    /// in the grammar; the runtime stamps it on the resulting `Token`
    /// event so consumers can identify the position at runtime.
    Expect {
        /// Required token-kind id.
        kind: u16,
        /// Token name, retained only for the diagnostic message.
        token_name: String,
        /// SYNC-set id to recover to on mismatch.
        sync: SyncSetId,
        /// Optional label from a `name:NAME` grammar form.
        label: Option<String>,
    },
    /// Recursive call into another rule or block.
    Call {
        /// Callee — a sibling block or another rule by name.
        target: Target,
    },
    /// `?` branch — call `body` zero or one times.
    Opt {
        /// FIRST-set id the body opens with.
        first: FirstSetId,
        /// Body to call when the lookahead matches `first`.
        body: Target,
    },
    /// `*` loop — repeatedly call `body` while the lookahead matches.
    Star {
        /// FIRST-set id the body opens with.
        first: FirstSetId,
        /// Body of one iteration.
        body: Target,
    },
    /// `Alt` dispatch — pick one arm based on up to `k` tokens of
    /// lookahead, or recover via `sync`.
    Dispatch {
        /// `(FIRST-set id, arm body)` pairs, one per non-nullable arm.
        arms: Vec<(FirstSetId, Target)>,
        /// True if any arm is nullable — changes the default action from
        /// "error and recover" to "fall through".
        has_eps: bool,
        /// SYNC-set id to recover to on "unexpected token".
        sync: SyncSetId,
    },
}

/// A sequence of ops with a label prefix used for diagnostic display.
///
/// `op_labels[i]` is a human-readable name for `ops[i]` (e.g.
/// `expr:call:atom`) that carries through to the emitted code as a comment
/// and into the debug dumps.
#[derive(Clone)]
pub struct Block {
    /// Human-readable prefix that every op label in this block starts with.
    /// Preserves a rule-hierarchical trail in debug dumps, e.g. `expr:alt1`.
    pub label_prefix: String,
    /// Ops in execution order.
    pub ops: Vec<Op>,
    /// Parallel to `ops`: `op_labels[i]` is the diagnostic label for
    /// `ops[i]`. Kept out-of-band so `Op` stays a plain sum type.
    pub op_labels: Vec<String>,
}

/// The output of the build phase: every block, the entry-block map, and
/// the interned FIRST/SYNC pools.
///
/// `rule_order` preserves grammar declaration order so that the layout
/// phase can walk rules in a deterministic, user-visible order.
/// `public_rule_names` is the subset that becomes part of the generated
/// API (fragments are kept in `rule_entry` but not here).
pub struct Program {
    /// All blocks — rule bodies plus every synthetic sub-block (alt arms,
    /// loop bodies, etc.). Referenced by `BlockId(index)`.
    pub blocks: Vec<Block>,
    /// Rule name → entry block. Covers both public rules and fragments.
    pub rule_entry: HashMap<String, BlockId>,
    /// Rules in grammar declaration order. Used so `layout` numbers
    /// states in a stable, human-readable order.
    pub rule_order: Vec<String>,
    /// Rules that should become public API (fragments excluded).
    pub public_rule_names: BTreeSet<String>,
    /// Token metadata mirroring the analyzed grammar, fragments resolved.
    pub tokens: Vec<TokenInfo>,
    /// Names of the public (non-fragment) rules. A rule's `RuleKind` id
    /// is its index here.
    pub rules: Vec<String>,
    /// Interned FIRST-set pool. Each entry is a `FirstSet` — a list of
    /// `LookaheadSeq`s (i.e. `Vec<Vec<u16>>`). Index by [`FirstSetId`].
    pub first_sets: FirstSetPool,
    /// Interned SYNC-set pool. Each entry is a flat `Vec<u16>` of token
    /// ids. Index by [`SyncSetId`].
    pub sync_sets: SyncSetPool,
    /// Lexer mode names indexed by id. `mode_names[0]` is always
    /// `"default"`; further entries come from `@mode(name)` annotations
    /// in declaration order.
    pub mode_names: Vec<String>,
}

/// Mutable state carried through the recursive descent over rule bodies.
///
/// `current_sync` is the SYNC-set id to stamp on any `Expect` ops we emit
/// inside the rule we are currently lowering — every rule computes its own
/// SYNC (derived from its FOLLOW set) up front so inner sites don't have
/// to recompute it. `current_symbol` is just the label prefix we use for
/// sub-blocks so their op labels read like `expr:alt0:expect:LPAREN`.
struct Builder<'a> {
    ag: &'a AnalyzedGrammar,
    blocks: Vec<Block>,
    first_sets: FirstSetPool,
    first_intern: HashMap<Vec<LookaheadSeq>, FirstSetId>,
    sync_sets: SyncSetPool,
    sync_intern: HashMap<Vec<u16>, SyncSetId>,
    current_sync: SyncSetId,
    current_symbol: String,
}

/// Lower every rule in `ag` to a [`Program`]. Walks the rules in grammar
/// order, wrapping public rules in `Enter`/`Exit` structural events and
/// inlining fragment rules raw (they produce no structural events).
pub fn build(ag: &AnalyzedGrammar) -> Program {
    let g = &ag.grammar;
    let rules = collect_rules(g);

    // Mode-name → numeric id. Default mode is always 0; further entries
    // come from `@mode(name)` pre-annotations in declaration order. The
    // map is built up-front so token mode_ids and mode_action targets
    // resolve with a single pass.
    let mode_ids = collect_mode_ids(g);

    // Assign dense kind ids starting at 1 — EOF (0) is reserved. Lex
    // failures are surfaced as `Option<TK>` (`None`) at the runtime
    // boundary, so they consume no kind id. Fragments are filtered out
    // because they never become a real token kind at run time.
    let tokens: Vec<TokenInfo> = g
        .tokens
        .values()
        .filter(|t| !t.is_fragment)
        .enumerate()
        .map(|(i, t)| TokenInfo {
            name: t.name.clone(),
            pattern: resolve_pattern(&t.pattern, g),
            skip: t.skip,
            kind: (i + 1) as u16,
            mode_ids: t
                .modes
                .iter()
                .map(|name| mode_ids[name.as_str()])
                .collect(),
            mode_actions: t
                .mode_actions
                .iter()
                .map(|a| match a {
                    ModeAction::Push(name) => {
                        let id = mode_ids.get(name.as_str()).copied().unwrap_or_else(|| {
                            // The grammar parser doesn't emit a synthetic
                            // mode-name table, so a `push(unknown)` would
                            // never resolve here. Bake in `0` (default) as
                            // a fallback so codegen still succeeds; any
                            // misuse should already have surfaced as an
                            // analysis-stage error.
                            0
                        });
                        ModeActionInfo::Push(id)
                    }
                    ModeAction::Pop => ModeActionInfo::Pop,
                })
                .collect(),
        })
        .collect();
    let mut b = Builder {
        ag,
        blocks: Vec::new(),
        first_sets: Vec::new(),
        first_intern: HashMap::new(),
        sync_sets: Vec::new(),
        sync_intern: HashMap::new(),
        current_sync: 0,
        current_symbol: String::new(),
    };

    let mut rule_entry: HashMap<String, BlockId> = HashMap::new();
    let mut rule_order: Vec<String> = Vec::with_capacity(g.rules.len());
    for rule in g.rules.values() {
        // Every rule gets its own SYNC set (its FOLLOW plus EOF). Interned
        // up-front and stashed in `current_sync` so every `Expect` emitted
        // inside this rule's body can refer to it without recomputing.
        let sync = compute_sync(ag, &rule.name);
        b.current_sync = b.intern_sync(sync);
        b.current_symbol = rule.name.clone();
        let bid = b.new_block(rule.name.clone());
        rule_entry.insert(rule.name.clone(), bid);
        rule_order.push(rule.name.clone());

        let rule_tail = b.ag.follow_k.get(&rule.name).cloned().unwrap_or_default();

        if !rule.is_fragment {
            // Non-fragment rules produce Enter/Exit events so consumers
            // see their subtree in the event stream. Fragments are
            // "transparent" — their body is emitted raw.
            let kind = rule_kind_id(&rules, &rule.name);
            let enter_label = format!("{}:enter", rule.name);
            let exit_label = format!("{}:exit", rule.name);
            b.push_op(bid, Op::Enter { kind }, enter_label);
            b.emit_expr(&rule.body, bid, &rule_tail);
            b.push_op(bid, Op::Exit { kind }, exit_label);
        } else {
            b.emit_expr(&rule.body, bid, &rule_tail);
        }
    }

    let public_rule_names: BTreeSet<String> = g
        .rules
        .values()
        .filter(|r| !r.is_fragment)
        .map(|r| r.name.clone())
        .collect();
    let Builder {
        blocks,
        first_sets,
        sync_sets,
        ..
    } = b;
    let mut mode_names: Vec<String> = vec!["default".to_string(); mode_ids.len()];
    for (name, id) in &mode_ids {
        mode_names[*id as usize] = name.clone();
    }
    Program {
        blocks,
        rule_entry,
        rule_order,
        public_rule_names,
        tokens,
        rules,
        first_sets,
        sync_sets,
        mode_names,
    }
}

/// Walk every token declaration and assign a numeric mode id to each
/// distinct `@mode(name)` annotation. Mode `"default"` (= unannotated)
/// always maps to id 0; further modes get ids 1.. in the order their
/// first occurrence appears in the grammar source. Mode names referenced
/// from `-> push(name)` actions are also recorded so action resolution
/// can find them, even if no token actually lives in the named mode.
fn collect_mode_ids(g: &Grammar) -> HashMap<String, u32> {
    let mut ids: HashMap<String, u32> = HashMap::new();
    ids.insert("default".to_string(), 0);
    let mut next: u32 = 1;
    for t in g.tokens.values() {
        for name in &t.modes {
            ids.entry(name.to_string()).or_insert_with(|| {
                let id = next;
                next += 1;
                id
            });
        }
        for a in &t.mode_actions {
            if let ModeAction::Push(name) = a {
                ids.entry(name.clone()).or_insert_with(|| {
                    let id = next;
                    next += 1;
                    id
                });
            }
        }
    }
    ids
}

impl Builder<'_> {
    /// Allocate a fresh empty block and return its id.
    fn new_block(&mut self, label_prefix: String) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block {
            label_prefix,
            ops: Vec::new(),
            op_labels: Vec::new(),
        });
        id
    }

    /// Append an op (with its matching diagnostic label) to a block.
    fn push_op(&mut self, bid: BlockId, op: Op, label: String) {
        let block = &mut self.blocks[bid.0 as usize];
        block.ops.push(op);
        block.op_labels.push(label);
    }

    /// Intern a FIRST set (a list of token-id sequences) and return its id.
    ///
    /// Sort+dedup first so two call sites that produce the same set via
    /// different orderings still hash to one entry — that turns N
    /// syntactically-similar grammar fragments into a single shared table
    /// row in the final output.
    fn intern_first(&mut self, mut seqs: Vec<LookaheadSeq>) -> FirstSetId {
        seqs.sort();
        seqs.dedup();
        if let Some(id) = self.first_intern.get(&seqs) {
            return *id;
        }
        let id = self.first_sets.len() as FirstSetId;
        self.first_intern.insert(seqs.clone(), id);
        self.first_sets.push(FirstSet {
            id,
            seqs,
            has_references: false,
        });
        id
    }

    /// Intern a SYNC set (a list of token ids). Same sort+dedup strategy
    /// as `intern_first`.
    fn intern_sync(&mut self, mut kinds: Vec<u16>) -> SyncSetId {
        kinds.sort();
        kinds.dedup();
        if let Some(id) = self.sync_intern.get(&kinds) {
            return *id;
        }
        let id = self.sync_sets.len() as SyncSetId;
        self.sync_intern.insert(kinds.clone(), id);
        self.sync_sets.push(SyncSet { id, kinds });
        id
    }

    fn first_of_expr(&self, e: &Expr) -> analysis::FirstSet {
        analysis::first_follow::first_of(e, &self.ag.nullable, &self.ag.first, self.ag.k)
    }

    /// Intern a FIRST set after converting its string-name sequences into
    /// numeric token ids and dropping the ε sequence — dispatches use
    /// nullability as a separate bit (`has_eps`) rather than a sentinel
    /// inside the set.
    fn first_ids(&mut self, first: &analysis::FirstSet) -> FirstSetId {
        let seqs: Vec<LookaheadSeq> = first
            .iter()
            .filter(|seq| !seq.is_empty())
            .map(|seq| seq.iter().map(|n| token_kind(self.ag, n)).collect::<LookaheadSeq>())
            .collect();
        self.intern_first(seqs)
    }

    /// Build a sub-block for a piece of grammar (an alternative arm, a
    /// `*`-body, etc.), emit `body_expr` into it, and return the block id.
    ///
    /// Swaps `current_symbol` for the duration so the sub-block's ops get
    /// hierarchical labels (e.g. `expr:alt0:call:atom` rather than
    /// `expr:call:atom`) which is much easier to read in dumps.
    fn build_sub(
        &mut self,
        sub_suffix: &str,
        body_expr: &Expr,
        tail: &analysis::FirstSet,
    ) -> BlockId {
        let sub_label = format!("{}:{}", self.current_symbol, sub_suffix);
        let sub = self.new_block(sub_label.clone());
        let saved = std::mem::replace(&mut self.current_symbol, sub_label);
        self.emit_expr(body_expr, sub, tail);
        self.current_symbol = saved;
        sub
    }

    /// Lower one grammar expression into ops appended to `cur`.
    ///
    /// `tail` is the FIRST(k) of "everything that follows this expression
    /// in the enclosing context". It's needed to compute accurate
    /// prediction sets for nullable arms and loop bodies — if an arm can
    /// match ε, what actually distinguishes it is the tokens that would
    /// follow, not the arm itself.
    fn emit_expr(&mut self, e: &Expr, cur: BlockId, tail: &analysis::FirstSet) {
        let k = self.ag.k;
        match e {
            Expr::Empty => {}
            Expr::Token(name) => {
                let kind = token_kind(self.ag, name);
                let dbg_label = format!("{}:expect:{}", self.current_symbol, name);
                self.push_op(
                    cur,
                    Op::Expect {
                        kind,
                        token_name: name.clone(),
                        sync: self.current_sync,
                        label: None,
                    },
                    dbg_label,
                );
            }
            Expr::Rule(name) => {
                let label = format!("{}:call:{}", self.current_symbol, name);
                self.push_op(
                    cur,
                    Op::Call {
                        target: Target::Rule(name.clone()),
                    },
                    label,
                );
            }
            Expr::Seq(xs) => {
                // Compute the tail for every position right-to-left: the
                // tail after xs[i] is FIRST(xs[i+1..]) concatenated with
                // the outer tail. Each child is then emitted with the
                // correct forward-looking context.
                let n = xs.len();
                let mut succ: Vec<analysis::FirstSet> = vec![tail.clone(); n + 1];
                for i in (0..n).rev() {
                    let fx = self.first_of_expr(&xs[i]);
                    succ[i] = analysis::first_follow::concat_k(&fx, &succ[i + 1], k);
                }
                for i in 0..n {
                    self.emit_expr(&xs[i], cur, &succ[i + 1]);
                }
            }
            Expr::Alt(xs) => {
                let firsts: Vec<analysis::FirstSet> =
                    xs.iter().map(|x| self.first_of_expr(x)).collect();

                // `has_eps` is hoisted out of the per-arm FIRST sets
                // because the dispatch node needs it as a single bit —
                // "is any arm nullable?". Nullable arms are handled via a
                // fall-through default, not by putting ε in a FIRST set.
                let has_eps = firsts.iter().any(|f| f.iter().any(|seq| seq.is_empty()));
                let mut arms: Vec<(FirstSetId, Target)> = Vec::new();
                for (idx, (arm_expr, first)) in xs.iter().zip(firsts.iter()).enumerate() {
                    // A wholly nullable arm (only ε in its FIRST) is
                    // already covered by `has_eps` fall-through — no need
                    // to dispatch into it.
                    if first.iter().all(|seq| seq.is_empty()) {
                        continue;
                    }

                    // Extend FIRST with the surrounding tail so nullable
                    // pieces of the arm are disambiguated by what follows
                    // the whole alternation.
                    let predict = analysis::first_follow::concat_k(first, tail, k);
                    let (non_eps, _) = analysis::first_follow::split_nullable(&predict);
                    let fid = self.first_ids(&non_eps);

                    let sub = self.build_sub(&format!("alt{}", idx), arm_expr, tail);
                    arms.push((fid, Target::Block(sub)));
                }
                let label = format!("{}:dispatch", self.current_symbol);
                self.push_op(
                    cur,
                    Op::Dispatch {
                        arms,
                        has_eps,
                        sync: self.current_sync,
                    },
                    label,
                );
            }
            Expr::Opt(x) => {
                let first = self.first_of_expr(x);
                let fid = self.first_ids(&first);

                let sub = self.build_sub("opt-body", x, tail);
                let label = format!("{}:opt", self.current_symbol);
                self.push_op(
                    cur,
                    Op::Opt {
                        first: fid,
                        body: Target::Block(sub),
                    },
                    label,
                );
            }
            Expr::Star(x) => {
                let first = self.first_of_expr(x);
                let fid = self.first_ids(&first);

                // Inside a star, each body iteration can be followed by
                // another iteration *or* by the outer tail — the body's
                // tail therefore needs to include FIRST(body) prepended.
                let body_tail = analysis::first_follow::concat_k(&first, tail, k);
                let sub = self.build_sub("star-body", x, &body_tail);
                let label = format!("{}:star", self.current_symbol);
                self.push_op(
                    cur,
                    Op::Star {
                        first: fid,
                        body: Target::Block(sub),
                    },
                    label,
                );
            }
            Expr::Plus(x) => {
                // `x+` = `x x*`: emit one guaranteed call, then the same
                // body as a `*` loop. Sharing the sub-block between the
                // call and the loop means the body is lowered once.
                let first = self.first_of_expr(x);
                let fid = self.first_ids(&first);
                let body_tail = analysis::first_follow::concat_k(&first, tail, k);
                let sub = self.build_sub("plus-body", x, &body_tail);
                let call_label = format!("{}:plus-first", self.current_symbol);
                self.push_op(
                    cur,
                    Op::Call {
                        target: Target::Block(sub),
                    },
                    call_label,
                );
                let star_label = format!("{}:plus-star", self.current_symbol);
                self.push_op(
                    cur,
                    Op::Star {
                        first: fid,
                        body: Target::Block(sub),
                    },
                    star_label,
                );
            }
            Expr::Label(name, body) => match body.as_ref() {
                Expr::Token(tok_name) => {
                    let kind = token_kind(self.ag, tok_name);
                    let dbg_label = format!(
                        "{}:expect:{}@{}",
                        self.current_symbol, tok_name, name
                    );
                    self.push_op(
                        cur,
                        Op::Expect {
                            kind,
                            token_name: tok_name.clone(),
                            sync: self.current_sync,
                            label: Some(name.clone()),
                        },
                        dbg_label,
                    );
                }
                _ => {
                    // Defensive: validate.rs rejects labels on anything
                    // other than a token reference. If something slips
                    // through we strip the label and recurse so the
                    // grammar still lowers — the missing label just
                    // doesn't surface in the event stream.
                    self.emit_expr(body, cur, tail);
                }
            },
        }
    }
}

/// Names of the public (non-fragment) rules, in declaration order.
/// Fragments are excluded because they don't become `RuleKind` variants.
fn collect_rules(g: &Grammar) -> Vec<String> {
    // `g.rules` is an `IndexMap` keyed by name; iterating values yields
    // them in insertion (i.e. declaration) order. Filter out fragments
    // — they're inlined at lex time and don't need a `RuleKind` slot.
    g.rules
        .values()
        .filter(|r| !r.is_fragment)
        .map(|r| r.name.clone())
        .collect()
}

/// SYNC set for a rule: every real token in its FOLLOW plus EOF. `EOF`
/// always sits at the end so the recovery loop will stop if nothing else
/// catches it, rather than running off the input.
fn compute_sync(ag: &AnalyzedGrammar, rule_name: &str) -> Vec<u16> {
    let follow = ag.follow.get(rule_name).cloned().unwrap_or_default();
    let mut names: Vec<&str> = follow
        .iter()
        .filter(|t| t.as_str() != EOF_MARKER)
        .map(|t| t.as_str())
        .collect();
    names.sort();
    names.dedup();
    let mut ids: Vec<u16> = names.iter().map(|n| token_kind(ag, n)).collect();
    ids.push(parsuna_rt::TOKEN_EOF);
    ids
}

/// Look up the numeric kind id of a token by name. Fragments are excluded
/// from the numbering (they never appear at run time), so the id is 1-based
/// over the non-fragment tokens.
fn token_kind(ag: &AnalyzedGrammar, name: &str) -> u16 {
    ag.grammar
        .tokens
        .values()
        .filter(|t| !t.is_fragment)
        .position(|t| t.name == name)
        .map(|i| (i + 1) as u16)
        .unwrap_or(0)
}

/// Inline every `TokenPattern::Ref` into the referenced token's body.
/// Fragments have already been validated to exist and be acyclic, so the
/// recursion always terminates.
fn resolve_pattern(p: &TokenPattern, g: &Grammar) -> TokenPattern {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => p.clone(),
        // Self-contained — `chars` and `strings` are inline literals.
        TokenPattern::NegLook { .. } => p.clone(),
        TokenPattern::Ref(n) => match g.tokens.get(n) {
            Some(td) => resolve_pattern(&td.pattern, g),
            None => TokenPattern::Empty,
        },
        TokenPattern::Seq(xs) => {
            TokenPattern::Seq(xs.iter().map(|x| resolve_pattern(x, g)).collect())
        }
        TokenPattern::Alt(xs) => {
            TokenPattern::Alt(xs.iter().map(|x| resolve_pattern(x, g)).collect())
        }
        TokenPattern::Opt(x) => TokenPattern::Opt(Box::new(resolve_pattern(x, g))),
        TokenPattern::Star(x) => TokenPattern::Star(Box::new(resolve_pattern(x, g))),
        TokenPattern::Plus(x) => TokenPattern::Plus(Box::new(resolve_pattern(x, g))),
    }
}

/// Numeric id of a rule, as its position in `rules` (public rules only).
fn rule_kind_id(rules: &[String], name: &str) -> u16 {
    rules
        .iter()
        .position(|n| n == name)
        .map(|i| i as u16)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::grammar::parse_grammar;

    fn analyze_src(src: &str) -> AnalyzedGrammar {
        let g = parse_grammar(src).expect("parse");
        let outcome = analyze(g);
        assert!(!outcome.has_errors(), "{:?}", outcome.diagnostics);
        outcome.grammar.expect("grammar")
    }

    fn pattern_has_no_refs(p: &TokenPattern) -> bool {
        match p {
            TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => true,
            TokenPattern::NegLook { .. } => true,
            TokenPattern::Ref(_) => false,
            TokenPattern::Seq(xs) | TokenPattern::Alt(xs) => {
                xs.iter().all(pattern_has_no_refs)
            }
            TokenPattern::Opt(x) | TokenPattern::Star(x) | TokenPattern::Plus(x) => {
                pattern_has_no_refs(x)
            }
        }
    }

    #[test]
    fn build_assigns_token_kinds_starting_at_one_and_skipping_fragments() {
        let ag = analyze_src(
            "_DIGIT = '0'..'9'; NUM = _DIGIT+; T = \"t\"; main = NUM T;",
        );
        let prog = build(&ag);
        // Fragment _DIGIT must be excluded from the kind table.
        assert!(prog.tokens.iter().all(|t| !t.name.starts_with('_')));
        let kinds: Vec<u16> = prog.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, vec![1, 2]);
    }

    #[test]
    fn build_resolves_token_pattern_refs_inline() {
        let ag = analyze_src(
            "_DIGIT = '0'..'9'; NUM = _DIGIT+; main = NUM;",
        );
        let prog = build(&ag);
        let num = prog.tokens.iter().find(|t| t.name == "NUM").unwrap();
        // The Plus body should now be a Class — no more Ref.
        assert!(
            pattern_has_no_refs(&num.pattern),
            "ref not inlined: {:?}",
            num.pattern
        );
    }

    #[test]
    fn build_records_rule_order_in_grammar_order() {
        let ag = analyze_src("T = \"t\"; first = T; second = T; third = T;");
        let prog = build(&ag);
        assert_eq!(prog.rule_order, vec!["first", "second", "third"]);
    }

    #[test]
    fn build_public_rule_names_excludes_fragments() {
        let ag = analyze_src(
            "T = \"t\"; _helper = T; main = T _helper;",
        );
        let prog = build(&ag);
        assert!(prog.public_rule_names.contains("main"));
        assert!(!prog.public_rule_names.contains("_helper"));
        // But fragment still needs an entry block (it's called from `main`).
        assert!(prog.rule_entry.contains_key("_helper"));
    }

    #[test]
    fn build_assigns_block_to_every_rule_entry() {
        let ag = analyze_src("T = \"t\"; a = T; b = T; main = a b;");
        let prog = build(&ag);
        for name in &prog.rule_order {
            let bid = prog.rule_entry.get(name).expect("rule has entry");
            assert!(
                (bid.0 as usize) < prog.blocks.len(),
                "rule `{}` entry out of bounds",
                name
            );
        }
    }

    #[test]
    fn build_interns_first_sets_across_rules() {
        // Two rules each have a `T?` — both Opts open with the same FIRST set
        // ([T]), so the pool should intern them rather than store two copies.
        let ag = analyze_src("T = \"t\"; a = T?; b = T?; main = a b;");
        let prog = build(&ag);
        // We have at least two Opt sites both with FIRST = {[T]}; if interning
        // is doing its job, the pool size is strictly less than the number of
        // sites times 2.
        let opt_sites = prog
            .blocks
            .iter()
            .flat_map(|b| b.ops.iter())
            .filter(|op| matches!(op, Op::Opt { .. }))
            .count();
        assert!(opt_sites >= 2, "expected ≥2 Opt sites, got {}", opt_sites);
        assert!(
            prog.first_sets.len() < opt_sites * 2,
            "FIRST pool not interned: {} entries for {} sites",
            prog.first_sets.len(),
            opt_sites
        );
    }

    #[test]
    fn compute_sync_includes_eof_marker_at_end() {
        let ag = analyze_src("T = \"t\"; main = T;");
        let sync = compute_sync(&ag, "main");
        assert!(!sync.is_empty());
        assert_eq!(*sync.last().unwrap(), parsuna_rt::TOKEN_EOF);
    }

    #[test]
    fn token_kind_helper_returns_one_based_position_excluding_fragments() {
        let ag = analyze_src(
            "_DIGIT = '0'..'9'; A = \"a\"; B = \"b\"; main = A B;",
        );
        assert_eq!(token_kind(&ag, "A"), 1);
        assert_eq!(token_kind(&ag, "B"), 2);
        // Fragments have no runtime kind.
        assert_eq!(token_kind(&ag, "_DIGIT"), 0);
        assert_eq!(token_kind(&ag, "MISSING"), 0);
    }

    #[test]
    fn rule_kind_id_helper_returns_position() {
        let rules = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(rule_kind_id(&rules, "a"), 0);
        assert_eq!(rule_kind_id(&rules, "c"), 2);
        assert_eq!(rule_kind_id(&rules, "missing"), 0);
    }

    #[test]
    fn resolve_pattern_inlines_chain_of_refs() {
        let mut g = Grammar::default();
        let mut leaf = TokenDef {
            name: "_LEAF".into(),
            pattern: TokenPattern::Literal("x".into()),
            skip: false,
            is_fragment: true,
            modes: vec!["default".to_string()],
            mode_actions: Vec::new(),
            span: Default::default(),
        };
        leaf.is_fragment = true;
        g.add_token(leaf);
        let mut mid = TokenDef {
            name: "_MID".into(),
            pattern: TokenPattern::Ref("_LEAF".into()),
            skip: false,
            is_fragment: true,
            modes: vec!["default".to_string()],
            mode_actions: Vec::new(),
            span: Default::default(),
        };
        mid.is_fragment = true;
        g.add_token(mid);
        let resolved = resolve_pattern(&TokenPattern::Ref("_MID".into()), &g);
        assert!(matches!(resolved, TokenPattern::Literal(ref s) if s == "x"));
    }
}
