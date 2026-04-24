//! Semantic analysis of a parsed grammar.
//!
//! The main entry point is [`analyze`]: it validates the grammar, then
//! iteratively computes FIRST/FOLLOW for increasing `k` until every
//! alternative can be disambiguated by `k` tokens of lookahead (LL(k)).
//! If no finite `k` works within [`STUCK_LIMIT`] attempts without progress,
//! it reports the remaining conflicts as errors.

pub mod first_follow;
mod validate;

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Error;
use crate::grammar::ir::*;

pub use first_follow::{FirstSet, FollowSet, Seq};

/// Placeholder name used inside FOLLOW sets to mean "end of input". Picked
/// so no real grammar token can collide with it.
pub const EOF_MARKER: &str = "$EOF";

/// How many rounds of raising `k` we tolerate with no reduction in the
/// number of conflicting sequences before giving up on the grammar. Keeps
/// analysis from looping forever on genuinely ambiguous grammars.
const STUCK_LIMIT: usize = 3;

/// A validated grammar together with the FIRST/FOLLOW tables needed to
/// generate its parser.
///
/// `first` is FIRST(k) for the chosen `k` (sequences up to `k` tokens long).
/// `follow` is the classic FOLLOW set (single-token, used for recovery sync
/// points). `follow_k` is FIRST(k)-style follow used by the dispatch logic.
/// `nullable[name]` tracks whether a rule derives the empty string.
#[derive(Clone, Debug)]
pub struct AnalyzedGrammar {
    /// The grammar this analysis ran over. Preserved verbatim so
    /// downstream phases can reach the original token/rule definitions.
    pub grammar: Grammar,
    /// `FIRST(k)` per rule name: every length-≤-`k` token-name prefix that
    /// can start an occurrence of the rule. The empty sequence marks a
    /// nullable rule.
    pub first: BTreeMap<String, FirstSet>,
    /// Classic single-token FOLLOW per rule name, used as recovery sync
    /// points. `EOF_MARKER` stands in for end-of-input.
    pub follow: BTreeMap<String, FollowSet>,
    /// `FIRST(k)`-style FOLLOW per rule name: every length-≤-`k` sequence
    /// that can legally follow an occurrence of the rule. Used by the
    /// dispatch logic to extend each arm's prediction with context.
    pub follow_k: BTreeMap<String, FirstSet>,
    /// Per-rule nullability: `true` iff the rule can derive ε.
    pub nullable: BTreeMap<String, bool>,
    /// The smallest `k` for which the grammar is LL(k) — i.e. for which
    /// every alternative can be disambiguated by `k` tokens of lookahead.
    pub k: usize,
}

/// Validate `g` and compute the analysis tables at the smallest `k` that
/// resolves all LL conflicts.
///
/// Strategy: raise `k` one at a time. At each `k`, compute FIRST and
/// FIRST(k)-based follow, then scan every `Alt` and `Seq` node for arms
/// whose prediction sets intersect. If no conflicts remain we take that
/// `k`; otherwise we keep going, tracking whether the overall conflict
/// count is dropping. After [`STUCK_LIMIT`] rounds with no improvement we
/// give up — the grammar is ambiguous in a way that more lookahead cannot
/// fix, and we report the last round's conflicts as errors.
pub fn analyze(g: Grammar) -> Result<AnalyzedGrammar, Vec<Error>> {
    let mut issues = Vec::new();

    validate::run(&g, &mut issues);
    if !issues.is_empty() {
        return Err(issues);
    }

    let mut chosen: Option<(usize, BTreeMap<String, bool>, BTreeMap<String, FirstSet>)> = None;
    let mut last_conflicts: Vec<RawConflict> = Vec::new();
    let mut last_k;
    let mut min_size: Option<usize> = None;
    let mut stuck = 0usize;

    let mut k = 0usize;
    loop {
        k += 1;
        last_k = k;
        let (nullable, first) = first_follow::compute_first(&g, k);
        let follow_k = first_follow::compute_follow_k(&g, &first, &nullable, k);
        let conflicts = detect_conflicts(&g, &nullable, &first, &follow_k, k);
        if conflicts.is_empty() {
            chosen = Some((k, nullable, first));
            break;
        }
        let size: usize = conflicts.iter().map(|c| c.ambiguous.len()).sum();
        match min_size {
            Some(m) if size < m => {
                min_size = Some(size);
                stuck = 0;
            }
            Some(_) => stuck += 1,
            None => min_size = Some(size),
        }
        last_conflicts = conflicts;
        if stuck >= STUCK_LIMIT {
            break;
        }
    }

    let (k, nullable, first) = match chosen {
        Some(c) => c,
        None => {
            issues.push(Error::new(format!(
                "grammar is not LL(k) for any finite k: conflicts are stable \
                 at k = {} (stopped iterating after no progress over {} rounds)",
                last_k, STUCK_LIMIT
            )));
            for c in render_conflicts(&last_conflicts) {
                issues.push(c);
            }
            return Err(issues);
        }
    };

    let follow = first_follow::compute_follow(&g, &first, &nullable);
    let follow_k = first_follow::compute_follow_k(&g, &first, &nullable, k);
    Ok(AnalyzedGrammar {
        grammar: g,
        first,
        follow,
        follow_k,
        nullable,
        k,
    })
}

#[derive(Clone, Debug)]
struct RawConflict {
    rule_name: String,
    rule_span: crate::span::Span,
    arm_i: usize,
    arm_j: usize,
    ambiguous: BTreeSet<Seq>,
}

fn render_conflicts(raws: &[RawConflict]) -> Vec<Error> {
    raws.iter()
        .map(|c| {
            let sample: Vec<String> = c.ambiguous.iter().take(3).map(|s| format_seq(s)).collect();
            Error::new(format!(
                "rule `{}`: alternatives {} and {} both predict on {}{}",
                c.rule_name,
                c.arm_i + 1,
                c.arm_j + 1,
                sample.join(", "),
                if c.ambiguous.len() > 3 { ", …" } else { "" }
            ))
            .at(c.rule_span)
        })
        .collect()
}

fn detect_conflicts(
    g: &Grammar,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    follow_k: &BTreeMap<String, FirstSet>,
    k: usize,
) -> Vec<RawConflict> {
    let mut out = Vec::new();
    for r in &g.rules {
        let tail = follow_k.get(&r.name).cloned().unwrap_or_default();
        walk_check_conflicts(
            &r.body, &tail, &r.name, r.span, nullable, first, k, &mut out,
        );
    }
    out
}

fn walk_check_conflicts(
    e: &Expr,
    tail: &FirstSet,
    rule_name: &str,
    rule_span: crate::span::Span,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    k: usize,
    out: &mut Vec<RawConflict>,
) {
    match e {
        Expr::Empty | Expr::Token(_) | Expr::Rule(_) => {}
        Expr::Seq(xs) => {
            // Compute the "tail after position i" (succ[i+1] = tail after
            // xs[i]) by folding right-to-left: each element's lookahead is
            // its own FIRST concatenated with the tail that follows it.
            let n = xs.len();
            let mut succ: Vec<FirstSet> = vec![tail.clone(); n + 1];
            for i in (0..n).rev() {
                let fx = first_follow::first_of(&xs[i], nullable, first, k);
                succ[i] = first_follow::concat_k(&fx, &succ[i + 1], k);
            }
            for i in 0..n {
                walk_check_conflicts(
                    &xs[i],
                    &succ[i + 1],
                    rule_name,
                    rule_span,
                    nullable,
                    first,
                    k,
                    out,
                );
            }
        }
        Expr::Alt(xs) => {
            // Predict set for each arm: FIRST(arm) extended by the enclosing
            // tail. Nullable arms contribute their ε via the tail, so we
            // strip ε here and compare on the non-empty prefixes.
            let arms: Vec<(usize, FirstSet)> = xs
                .iter()
                .enumerate()
                .map(|(i, x)| {
                    let f = first_follow::first_of(x, nullable, first, k);
                    let predict = first_follow::concat_k(&f, tail, k);
                    let (non_eps, _) = first_follow::split_nullable(&predict);
                    (i, non_eps)
                })
                .collect();
            // Conflict detection: any prefix relation between two arms'
            // prediction sequences is ambiguous — if arm i predicts [a b]
            // and arm j predicts [a], seeing [a c] is fine for j but a
            // pure [a b …] input could match either.
            for i in 0..arms.len() {
                for j in (i + 1)..arms.len() {
                    let mut ambiguous: BTreeSet<Seq> = BTreeSet::new();
                    for a in &arms[i].1 {
                        for b in &arms[j].1 {
                            if a.starts_with(b.as_slice()) {
                                ambiguous.insert(b.clone());
                            } else if b.starts_with(a.as_slice()) {
                                ambiguous.insert(a.clone());
                            }
                        }
                    }
                    if !ambiguous.is_empty() {
                        out.push(RawConflict {
                            rule_name: rule_name.to_string(),
                            rule_span,
                            arm_i: arms[i].0,
                            arm_j: arms[j].0,
                            ambiguous,
                        });
                    }
                }
            }
            for x in xs {
                walk_check_conflicts(x, tail, rule_name, rule_span, nullable, first, k, out);
            }
        }
        Expr::Opt(x) => {
            walk_check_conflicts(x, tail, rule_name, rule_span, nullable, first, k, out);
        }
        Expr::Star(x) | Expr::Plus(x) => {
            // Inside a repetition, each iteration can be followed by
            // another iteration or by the outer tail — hence star_k
            // concatenated with tail rather than just tail.
            let fx = first_follow::first_of(x, nullable, first, k);
            let star = first_follow::star_k(&fx, k);
            let body_tail = first_follow::concat_k(&star, tail, k);
            walk_check_conflicts(x, &body_tail, rule_name, rule_span, nullable, first, k, out);
        }
    }
}

fn format_seq(seq: &Seq) -> String {
    if seq.is_empty() {
        "ε".to_string()
    } else {
        format!("[{}]", seq.join(" "))
    }
}
