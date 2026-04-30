//! FIRST(k)/FOLLOW computation, plus the iterative-deepening driver that
//! picks the smallest `k` for which the grammar has no LL conflicts.
//!
//! [`solve_lookahead`] is the entry point used by [`super::analyze`]; the
//! rest are helpers it (and the lints) build on (length-bounded
//! concatenation, bounded star closure, conflict detection).

use std::collections::{BTreeMap, BTreeSet};

use super::EOF_MARKER;
use crate::diagnostic::Diagnostic;
use crate::grammar::ir::*;
use crate::Span;

/// How many rounds of raising `k` we tolerate with no reduction in the
/// number of conflicting sequences before giving up on the grammar. Keeps
/// the iteration in [`solve_lookahead`] from looping forever on genuinely
/// ambiguous grammars.
const STUCK_LIMIT: usize = 3;

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
fn star_k(inner: &FirstSet, k: usize) -> FirstSet {
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
fn compute_first(
    g: &Grammar,
    k: usize,
) -> (BTreeMap<String, bool>, BTreeMap<String, FirstSet>) {
    let mut nullable: BTreeMap<String, bool> =
        g.rules.values().map(|r| (r.name.clone(), false)).collect();
    let mut first: BTreeMap<String, FirstSet> = g
        .rules
        .values()
        .map(|r| (r.name.clone(), FirstSet::new()))
        .collect();

    loop {
        let mut changed = false;
        for r in g.rules.values() {
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
        .values()
        .map(|r| {
            let mut s = BTreeSet::new();
            s.insert(EOF_MARKER.to_string());
            (r.name.clone(), s)
        })
        .collect();

    loop {
        let mut changed = false;
        for r in g.rules.values() {
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
        .values()
        .map(|r| {
            let mut s = FirstSet::new();
            s.insert(vec![EOF_MARKER.to_string()]);
            (r.name.clone(), s)
        })
        .collect();

    loop {
        let mut changed = false;
        for r in g.rules.values() {
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

/// Pick the smallest `k` for which the grammar is LL(k): raise `k` one at a
/// time, recompute FIRST/FOLLOW, scan for conflicts, and stop when none
/// remain. If no finite `k` works, push descriptive diagnostics into
/// `diags` and return `Err(())`.
///
/// Termination: gives up after [`STUCK_LIMIT`] rounds in which the conflict
/// count failed to decrease — past that point more lookahead is unlikely to
/// resolve anything.
pub fn solve_lookahead(
    g: &Grammar,
    diags: &mut Vec<Diagnostic>,
) -> Result<(usize, BTreeMap<String, bool>, BTreeMap<String, FirstSet>), ()> {
    let mut chosen: Option<(usize, BTreeMap<String, bool>, BTreeMap<String, FirstSet>)> = None;
    let mut last_conflicts: Vec<RawConflict> = Vec::new();
    let mut last_k;
    let mut min_size: Option<usize> = None;
    let mut stuck = 0usize;

    let mut k = 0usize;
    loop {
        k += 1;
        last_k = k;
        let (nullable, first) = compute_first(g, k);
        let follow_k = compute_follow_k(g, &first, &nullable, k);
        let conflicts = detect_conflicts(g, &nullable, &first, &follow_k, k);
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

    chosen.ok_or_else(|| {
        diags.push(Diagnostic::error(format!(
            "grammar is not LL(k) for any finite k: conflicts are stable \
             at k = {} (stopped iterating after no progress over {} rounds)",
            last_k, STUCK_LIMIT
        )));
        diags.extend(render_conflicts(&last_conflicts));
    })
}

#[derive(Clone, Debug)]
struct RawConflict {
    rule_name: String,
    rule_span: Span,
    arm_i: usize,
    arm_j: usize,
    ambiguous: BTreeSet<Seq>,
}

fn render_conflicts(raws: &[RawConflict]) -> Vec<Diagnostic> {
    raws.iter()
        .map(|c| {
            let sample: Vec<String> = c.ambiguous.iter().take(3).map(|s| format_seq(s)).collect();
            Diagnostic::error(format!(
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
    for r in g.rules.values() {
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
    rule_span: Span,
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
                let fx = first_of(&xs[i], nullable, first, k);
                succ[i] = concat_k(&fx, &succ[i + 1], k);
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
                    let f = first_of(x, nullable, first, k);
                    let predict = concat_k(&f, tail, k);
                    let (non_eps, _) = split_nullable(&predict);
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
            let fx = first_of(x, nullable, first, k);
            let star = star_k(&fx, k);
            let body_tail = concat_k(&star, tail, k);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(xs: &[&str]) -> Seq {
        xs.iter().map(|s| (*s).into()).collect()
    }

    fn fset(seqs: &[&[&str]]) -> FirstSet {
        seqs.iter().map(|s| seq(s)).collect()
    }

    fn rule(name: &str, body: Expr) -> RuleDef {
        RuleDef {
            name: name.into(),
            body,
            is_fragment: false,
            span: Span::default(),
        }
    }

    fn tok(name: &str) -> TokenDef {
        TokenDef {
            name: name.into(),
            pattern: TokenPattern::Empty,
            skip: false,
            is_fragment: false,
            mode: None,
            mode_actions: Vec::new(),
            span: Span::default(),
        }
    }

    #[test]
    fn concat_k_truncates_at_k_tokens() {
        let a = fset(&[&["A"]]);
        let b = fset(&[&["B", "C"]]);
        let merged = concat_k(&a, &b, 2);
        assert_eq!(merged, fset(&[&["A", "B"]]));

        let merged3 = concat_k(&a, &b, 3);
        assert_eq!(merged3, fset(&[&["A", "B", "C"]]));
    }

    #[test]
    fn concat_k_passes_through_long_left_sequences() {
        let a = fset(&[&["A", "B"]]);
        let b = fset(&[&["C"]]);
        let merged = concat_k(&a, &b, 2);
        assert_eq!(merged, fset(&[&["A", "B"]]));
    }

    #[test]
    fn concat_k_propagates_epsilon_on_left() {
        let a = fset(&[&[]]);
        let b = fset(&[&["A"], &["B"]]);
        let merged = concat_k(&a, &b, 1);
        assert_eq!(merged, fset(&[&["A"], &["B"]]));
    }

    #[test]
    fn star_k_includes_epsilon_and_iterates_to_bound() {
        let inner = fset(&[&["A"]]);
        let s = star_k(&inner, 3);
        assert_eq!(s, fset(&[&[], &["A"], &["A", "A"], &["A", "A", "A"]]));
    }

    #[test]
    fn split_nullable_separates_epsilon() {
        let s = fset(&[&[], &["A"], &["B", "C"]]);
        let (non_eps, null) = split_nullable(&s);
        assert!(null);
        assert_eq!(non_eps, fset(&[&["A"], &["B", "C"]]));

        let s2 = fset(&[&["X"]]);
        let (non_eps2, null2) = split_nullable(&s2);
        assert!(!null2);
        assert_eq!(non_eps2, fset(&[&["X"]]));
    }

    #[test]
    fn first_of_token_at_k1_is_singleton() {
        let nullable = BTreeMap::new();
        let first = BTreeMap::new();
        let f = first_of(&Expr::Token("A".into()), &nullable, &first, 1);
        assert_eq!(f, fset(&[&["A"]]));
    }

    #[test]
    fn first_of_token_at_k0_is_epsilon() {
        let nullable = BTreeMap::new();
        let first = BTreeMap::new();
        let f = first_of(&Expr::Token("A".into()), &nullable, &first, 0);
        assert_eq!(f, fset(&[&[]]));
    }

    #[test]
    fn first_of_opt_includes_epsilon() {
        let nullable = BTreeMap::new();
        let first = BTreeMap::new();
        let f = first_of(
            &Expr::Opt(Box::new(Expr::Token("A".into()))),
            &nullable,
            &first,
            1,
        );
        assert_eq!(f, fset(&[&[], &["A"]]));
    }

    #[test]
    fn first_of_plus_does_not_include_epsilon() {
        let nullable = BTreeMap::new();
        let first = BTreeMap::new();
        let f = first_of(
            &Expr::Plus(Box::new(Expr::Token("A".into()))),
            &nullable,
            &first,
            1,
        );
        assert_eq!(f, fset(&[&["A"]]));
    }

    #[test]
    fn compute_first_marks_nullable_rule() {
        // r = A?  → r is nullable; FIRST(r) = {ε, [A]}
        let mut g = Grammar::default();
        g.add_token(tok("A"));
        g.add_rule(rule(
            "r",
            Expr::Opt(Box::new(Expr::Token("A".into()))),
        ));
        let (nullable, first) = compute_first(&g, 1);
        assert_eq!(nullable.get("r"), Some(&true));
        assert_eq!(first.get("r").unwrap(), &fset(&[&[], &["A"]]));
    }

    #[test]
    fn compute_first_propagates_through_rule_reference() {
        // r = s; s = A;  → FIRST(r) = {[A]}
        let mut g = Grammar::default();
        g.add_token(tok("A"));
        g.add_rule(rule("r", Expr::Rule("s".into())));
        g.add_rule(rule("s", Expr::Token("A".into())));
        let (nullable, first) = compute_first(&g, 1);
        assert_eq!(nullable.get("r"), Some(&false));
        assert_eq!(first.get("r").unwrap(), &fset(&[&["A"]]));
    }

    #[test]
    fn compute_follow_includes_eof_for_every_rule() {
        let mut g = Grammar::default();
        g.add_token(tok("A"));
        g.add_rule(rule("r", Expr::Token("A".into())));
        let (nullable, first) = compute_first(&g, 1);
        let follow = compute_follow(&g, &first, &nullable);
        assert!(follow.get("r").unwrap().contains(EOF_MARKER));
    }

    #[test]
    fn compute_follow_passes_first_of_tail_to_inner_rule() {
        // outer = inner B; inner = A;  → FOLLOW(inner) ⊇ {B}
        let mut g = Grammar::default();
        g.add_token(tok("A"));
        g.add_token(tok("B"));
        g.add_rule(rule("inner", Expr::Token("A".into())));
        g.add_rule(rule(
            "outer",
            Expr::Seq(vec![Expr::Rule("inner".into()), Expr::Token("B".into())]),
        ));
        let (nullable, first) = compute_first(&g, 1);
        let follow = compute_follow(&g, &first, &nullable);
        let inner_follow = follow.get("inner").unwrap();
        assert!(inner_follow.contains("B"));
    }

    #[test]
    fn compute_follow_k_lookahead_sequences() {
        // outer = inner B C; inner = A;  → FOLLOW_2(inner) includes [B, C]
        let mut g = Grammar::default();
        g.add_token(tok("A"));
        g.add_token(tok("B"));
        g.add_token(tok("C"));
        g.add_rule(rule("inner", Expr::Token("A".into())));
        g.add_rule(rule(
            "outer",
            Expr::Seq(vec![
                Expr::Rule("inner".into()),
                Expr::Token("B".into()),
                Expr::Token("C".into()),
            ]),
        ));
        let (nullable, first) = compute_first(&g, 2);
        let follow = compute_follow_k(&g, &first, &nullable, 2);
        let inner_follow = follow.get("inner").unwrap();
        assert!(inner_follow.contains(&seq(&["B", "C"])));
    }
}
