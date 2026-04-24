//! FIRST(k)/FOLLOW computation and the helpers the grammar analysis builds
//! on (length-bounded concatenation, bounded star closure, etc.).

use std::collections::{BTreeMap, BTreeSet};

use super::EOF_MARKER;
use crate::grammar::ir::*;

/// A sequence of token names, bounded by the analysis' `k`. Tokens are
/// referred to by name here rather than numeric id because analysis runs
/// before lowering assigns ids.
pub type Seq = Vec<String>;
/// FIRST(k): the set of token sequences of length ≤ `k` that can start a
/// rule or expression. An empty sequence means "ε" (the expression is
/// nullable for this prefix).
pub type FirstSet = BTreeSet<Seq>;
/// Classic FOLLOW set: single tokens that may legally follow a rule.
/// `EOF_MARKER` stands in for end-of-input.
pub type FollowSet = BTreeSet<String>;

/// Length-bounded concatenation: `{ truncate(a ++ b, k) | a ∈ A, b ∈ B }`.
///
/// Any sequence in `A` already of length `k` is passed through unchanged —
/// `b` can add nothing to it since we only keep the first `k` tokens.
pub fn concat_k(a: &FirstSet, b: &FirstSet, k: usize) -> FirstSet {
    let mut out = FirstSet::new();
    for x in a {
        if x.len() >= k {
            out.insert(x.clone());
            continue;
        }
        for y in b {
            let mut s = x.clone();
            for t in y {
                if s.len() >= k {
                    break;
                }
                s.push(t.clone());
            }
            out.insert(s);
        }
    }
    out
}

/// Length-bounded Kleene star over FIRST sets. Seeded with ε and saturated
/// by repeatedly concatenating `inner`; the fixed point is finite because
/// sequences are capped at `k` tokens.
pub fn star_k(inner: &FirstSet, k: usize) -> FirstSet {
    let mut out: FirstSet = FirstSet::new();
    out.insert(Vec::new());
    loop {
        let added = concat_k(&out, inner, k);
        let prev = out.len();
        for s in added {
            out.insert(s);
        }
        if out.len() == prev {
            break;
        }
    }
    out
}

/// Compute FIRST(k) and nullability for every rule by fixed-point iteration.
///
/// Classic worklist-free saturation: on each pass we recompute every rule's
/// FIRST from the current approximation and stop once nothing changed. The
/// iteration always terminates because FIRST(k) is a finite lattice.
pub fn compute_first(
    g: &Grammar,
    k: usize,
) -> (BTreeMap<String, bool>, BTreeMap<String, FirstSet>) {
    let mut nullable: BTreeMap<String, bool> =
        g.rules.iter().map(|r| (r.name.clone(), false)).collect();
    let mut first: BTreeMap<String, FirstSet> = g
        .rules
        .iter()
        .map(|r| (r.name.clone(), FirstSet::new()))
        .collect();

    loop {
        let mut changed = false;
        for r in &g.rules {
            let e_first = first_of(&r.body, &nullable, &first, k);
            let mut cur = first.get(&r.name).cloned().unwrap_or_default();
            let cur_null = *nullable.get(&r.name).unwrap_or(&false);
            let mut new_null = cur_null || e_first.iter().any(|s| s.is_empty());
            for seq in &e_first {
                if cur.insert(seq.clone()) {
                    changed = true;
                }
            }
            if new_null != cur_null {
                changed = true;
                new_null = true;
                nullable.insert(r.name.clone(), new_null);
            }
            first.insert(r.name.clone(), cur);
        }
        if !changed {
            break;
        }
    }

    (nullable, first)
}

/// FIRST(k) of a single expression, given the rule-level tables already
/// computed by [`compute_first`].
pub fn first_of(
    e: &Expr,
    nullable: &BTreeMap<String, bool>,
    rule_first: &BTreeMap<String, FirstSet>,
    k: usize,
) -> FirstSet {
    let mut out = FirstSet::new();
    match e {
        Expr::Empty => {
            out.insert(Vec::new());
        }
        Expr::Token(n) => {
            if k == 0 {
                out.insert(Vec::new());
            } else {
                out.insert(vec![n.clone()]);
            }
        }
        Expr::Rule(n) => {
            if let Some(s) = rule_first.get(n) {
                out.extend(s.iter().cloned());
            }
            if *nullable.get(n).unwrap_or(&false) {
                out.insert(Vec::new());
            }
        }
        Expr::Seq(xs) => {
            // Prefix-accumulate FIRST across the sequence. Short-circuit
            // once every accumulated sequence is already `k` long —
            // subsequent elements cannot alter the result.
            let mut acc: FirstSet = FirstSet::new();
            acc.insert(Vec::new());
            for x in xs {
                let fx = first_of(x, nullable, rule_first, k);
                acc = concat_k(&acc, &fx, k);

                if acc.iter().all(|s| s.len() >= k) {
                    break;
                }
            }
            out = acc;
        }
        Expr::Alt(xs) => {
            for x in xs {
                out.extend(first_of(x, nullable, rule_first, k).into_iter());
            }
        }
        Expr::Opt(x) => {
            out.extend(first_of(x, nullable, rule_first, k).into_iter());
            out.insert(Vec::new());
        }
        Expr::Star(x) => {
            let inner = first_of(x, nullable, rule_first, k);
            out = star_k(&inner, k);
        }
        Expr::Plus(x) => {
            let inner = first_of(x, nullable, rule_first, k);
            let star = star_k(&inner, k);
            out = concat_k(&inner, &star, k);
        }
    }
    out
}

/// Split a FIRST set into its non-empty sequences and a boolean indicating
/// whether ε was a member. Handy when a caller wants to treat nullability
/// as a separate bit from the prediction set.
pub fn split_nullable(s: &FirstSet) -> (FirstSet, bool) {
    let mut out = FirstSet::new();
    let mut null = false;
    for seq in s {
        if seq.is_empty() {
            null = true;
        } else {
            out.insert(seq.clone());
        }
    }
    (out, null)
}

/// Compute FOLLOW (single-token) for every rule. The result is seeded with
/// `EOF_MARKER` for every rule (any rule could be the outermost one we are
/// about to emit) and then saturated by walking each rule's body.
pub fn compute_follow(
    g: &Grammar,
    first: &BTreeMap<String, FirstSet>,
    nullable: &BTreeMap<String, bool>,
) -> BTreeMap<String, FollowSet> {
    let mut follow: BTreeMap<String, FollowSet> = g
        .rules
        .iter()
        .map(|r| {
            let mut s = BTreeSet::new();
            s.insert(EOF_MARKER.to_string());
            (r.name.clone(), s)
        })
        .collect();

    loop {
        let mut changed = false;
        for r in &g.rules {
            walk_follow(&r.body, &r.name, nullable, first, &mut follow, &mut changed);
        }
        if !changed {
            break;
        }
    }
    follow
}

fn walk_follow(
    e: &Expr,
    host: &str,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    follow: &mut BTreeMap<String, FollowSet>,
    changed: &mut bool,
) {
    match e {
        Expr::Empty | Expr::Token(_) => {}
        Expr::Rule(name) => {
            if let Some(hf) = follow.get(host).cloned() {
                let target = follow.entry(name.clone()).or_default();
                for t in &hf {
                    if target.insert(t.clone()) {
                        *changed = true;
                    }
                }
            }
        }
        Expr::Seq(xs) => {
            for (i, x) in xs.iter().enumerate() {
                if let Expr::Rule(name) = x {
                    let tail = &xs[i + 1..];
                    let tf = first_of_seq_1(tail, nullable, first);
                    let tail_nullable = tf.contains("");
                    {
                        let target = follow.entry(name.clone()).or_default();
                        for t in tf.iter().filter(|t| !t.is_empty()) {
                            if target.insert(t.clone()) {
                                *changed = true;
                            }
                        }
                    }
                    if tail_nullable {
                        let host_follow = follow.get(host).cloned().unwrap_or_default();
                        let target = follow.entry(name.clone()).or_default();
                        for t in &host_follow {
                            if target.insert(t.clone()) {
                                *changed = true;
                            }
                        }
                    }
                }
                walk_follow(x, host, nullable, first, follow, changed);
            }
        }
        Expr::Alt(xs) => {
            for x in xs {
                walk_follow(x, host, nullable, first, follow, changed);
            }
        }
        Expr::Opt(x) => walk_follow(x, host, nullable, first, follow, changed),
        Expr::Star(x) | Expr::Plus(x) => {
            walk_follow(x, host, nullable, first, follow, changed);
            let fx = first_of(x, nullable, first, 1);
            let trailing = collect_trailing_rules(x, nullable);
            let host_follow = follow.get(host).cloned().unwrap_or_default();
            for r in trailing {
                let target = follow.entry(r).or_default();
                for seq in fx.iter().filter(|s| !s.is_empty()) {
                    if target.insert(seq[0].clone()) {
                        *changed = true;
                    }
                }
                for t in &host_follow {
                    if target.insert(t.clone()) {
                        *changed = true;
                    }
                }
            }
        }
    }
}

fn first_of_seq_1(
    xs: &[Expr],
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut all_nullable = true;
    for x in xs {
        let f = first_of(x, nullable, first, 1);
        let has_eps = f.iter().any(|s| s.is_empty());
        for seq in &f {
            if !seq.is_empty() {
                out.insert(seq[0].clone());
            }
        }
        if !has_eps {
            all_nullable = false;
            break;
        }
    }
    if all_nullable {
        out.insert(String::new());
    }
    out
}

fn collect_trailing_rules(e: &Expr, nullable: &BTreeMap<String, bool>) -> Vec<String> {
    let mut out = Vec::new();
    match e {
        Expr::Empty | Expr::Token(_) => {}
        Expr::Rule(n) => out.push(n.clone()),
        Expr::Seq(xs) => {
            for x in xs.iter().rev() {
                let mut sub = collect_trailing_rules(x, nullable);
                let is_null = expr_nullable(x, nullable);
                out.append(&mut sub);
                if !is_null {
                    break;
                }
            }
        }
        Expr::Alt(xs) => {
            for x in xs {
                out.append(&mut collect_trailing_rules(x, nullable));
            }
        }
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => {
            out.append(&mut collect_trailing_rules(x, nullable))
        }
    }
    out
}

/// FOLLOW(k): like [`compute_follow`] but producing sequences of up to `k`
/// tokens, suitable for the LL(k) dispatch trees.
pub fn compute_follow_k(
    g: &Grammar,
    first: &BTreeMap<String, FirstSet>,
    nullable: &BTreeMap<String, bool>,
    k: usize,
) -> BTreeMap<String, FirstSet> {
    let mut follow: BTreeMap<String, FirstSet> = g
        .rules
        .iter()
        .map(|r| {
            let mut s = FirstSet::new();
            s.insert(vec![EOF_MARKER.to_string()]);
            (r.name.clone(), s)
        })
        .collect();

    loop {
        let mut changed = false;
        for r in &g.rules {
            let host_follow = follow.get(&r.name).cloned().unwrap_or_default();
            walk_follow_k(
                &r.body,
                &host_follow,
                nullable,
                first,
                &mut follow,
                &mut changed,
                k,
            );
        }
        if !changed {
            break;
        }
    }
    follow
}

fn walk_follow_k(
    e: &Expr,
    tail: &FirstSet,
    nullable: &BTreeMap<String, bool>,
    first: &BTreeMap<String, FirstSet>,
    follow: &mut BTreeMap<String, FirstSet>,
    changed: &mut bool,
    k: usize,
) {
    match e {
        Expr::Empty | Expr::Token(_) => {}
        Expr::Rule(name) => {
            let target = follow.entry(name.clone()).or_default();
            for seq in tail {
                if target.insert(seq.clone()) {
                    *changed = true;
                }
            }
        }
        Expr::Seq(xs) => {
            let n = xs.len();
            let mut succ: Vec<FirstSet> = vec![tail.clone(); n + 1];
            for i in (0..n).rev() {
                let fx = first_of(&xs[i], nullable, first, k);
                succ[i] = concat_k(&fx, &succ[i + 1], k);
            }
            for i in 0..n {
                walk_follow_k(&xs[i], &succ[i + 1], nullable, first, follow, changed, k);
            }
        }
        Expr::Alt(xs) => {
            for x in xs {
                walk_follow_k(x, tail, nullable, first, follow, changed, k);
            }
        }
        Expr::Opt(x) => {
            walk_follow_k(x, tail, nullable, first, follow, changed, k);
        }
        Expr::Star(x) | Expr::Plus(x) => {
            // Inside a repetition the body can be followed by more
            // iterations or by the outer tail — hence star_k then tail.
            let fx = first_of(x, nullable, first, k);
            let star = star_k(&fx, k);
            let body_tail = concat_k(&star, tail, k);
            walk_follow_k(x, &body_tail, nullable, first, follow, changed, k);
        }
    }
}

fn expr_nullable(e: &Expr, nullable: &BTreeMap<String, bool>) -> bool {
    match e {
        Expr::Empty => true,
        Expr::Token(_) => false,
        Expr::Rule(n) => *nullable.get(n).unwrap_or(&false),
        Expr::Seq(xs) => xs.iter().all(|x| expr_nullable(x, nullable)),
        Expr::Alt(xs) => xs.iter().any(|x| expr_nullable(x, nullable)),
        Expr::Opt(_) | Expr::Star(_) => true,
        Expr::Plus(x) => expr_nullable(x, nullable),
    }
}
