//! Compile a set of [`TokenPattern`]s into a deterministic byte-level DFA.
//!
//! Thompson-style NFA construction followed by interval-based subset
//! construction. Both NFA edges and DFA arms carry inclusive byte ranges
//! `(u8, u8)`, so wide character classes (`.`, negations, `'a'..'z'`) cost
//! one edge / one arm regardless of how many bytes they cover. Subset
//! construction uses a sweep-line over endpoints to find the maximal
//! sub-intervals on which the active set of NFA targets is constant.
//!
//! [`DEAD`] is a runtime sentinel meaning "stop"; it never appears as a
//! DFA state in the [`compile`] output. The start state's id is always
//! [`START`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::grammar::ir::{CharClass, ClassItem, TokenPattern};
use crate::lowering::TokenInfo;

/// Reserved DFA state: the dead sink the runtime exits on. Never appears
/// as an entry in the [`compile`] output.
pub const DEAD: u32 = 0;

/// Start state of every compiled lexer DFA. State 0 is reserved for the
/// dead sink, so the start always lands at id 1.
pub const START: u32 = 1;

/// One DFA state: its own state id, byte transitions already grouped into
/// [`ByteArm`]s, and an optional accepted token kind.
#[derive(Clone, Debug)]
pub struct DfaState {
    /// State id — equal to this entry's index in the `Vec<DfaState>`.
    /// Carried inline so backends can iterate without calling `enumerate`.
    pub id: u32,
    /// Byte transitions, grouped so bytes sharing a target collapse into
    /// one arm and contiguous bytes within an arm collapse into ranges.
    pub arms: Vec<ByteArm>,
    /// Token-kind id accepted in this state, or `None` if it is not an
    /// accept state. Filled by taking the minimum (= highest priority) of
    /// the NFA states collapsed into this DFA state.
    pub accept: Option<i16>,
}

/// One `match`/`switch` arm in a generated DFA state: every byte that
/// transitions to `target`, packed into inclusive ranges.
#[derive(Clone, Debug)]
pub struct ByteArm {
    /// Destination DFA state id for every byte in `ranges`.
    pub target: u32,
    /// If `target` is an accept state, the token-kind id it accepts. Folded
    /// onto the arm so backends can emit `best_kind = ...` inline at the
    /// transition site without re-resolving `target` against the state vec.
    pub target_accept: Option<i16>,
    /// Inclusive byte ranges that all transition to `target`. Stored
    /// sorted and gap-separated; adjacent runs (`hi + 1 == next.lo`)
    /// merge into a single range.
    pub ranges: Vec<(u8, u8)>,
}

/// Compile tokens to a DFA.
///
/// Each token becomes an NFA fragment whose end state accepts that token's
/// kind id; the per-token fragments share a single ε-joined start so the
/// lexer tries every pattern in parallel. Subset construction then
/// determinises the combined machine, partitioning each state's outgoing
/// byte ranges into the disjoint sub-intervals on which the destination
/// is constant.
pub fn compile(tokens: &[TokenInfo]) -> Vec<DfaState> {
    let nfa = build_nfa(tokens);
    subset_construct(&nfa)
}

type NfaStateId = usize;

struct Nfa {
    states: Vec<NfaState>,
    start: NfaStateId,
}

#[derive(Default)]
struct NfaState {
    /// Each entry is a half-open byte range `(lo, hi)` (inclusive) and the
    /// NFA state to enter on any byte in that range.
    range_trans: Vec<((u8, u8), NfaStateId)>,
    epsilon: Vec<NfaStateId>,
    accept: Option<i16>,
}

struct NfaFragment {
    start: NfaStateId,
    end: NfaStateId,
}

struct NfaBuilder {
    states: Vec<NfaState>,
}

impl NfaBuilder {
    fn new() -> Self {
        Self { states: Vec::new() }
    }

    fn new_state(&mut self) -> NfaStateId {
        self.states.push(NfaState::default());
        self.states.len() - 1
    }

    fn add_epsilon(&mut self, from: NfaStateId, to: NfaStateId) {
        self.states[from].epsilon.push(to);
    }

    fn add_range(&mut self, from: NfaStateId, lo: u8, hi: u8, to: NfaStateId) {
        self.states[from].range_trans.push(((lo, hi), to));
    }

    fn set_accept(&mut self, s: NfaStateId, kind: i16) {
        self.states[s].accept = Some(kind);
    }

    fn compile(&mut self, pat: &TokenPattern) -> NfaFragment {
        match pat {
            TokenPattern::Empty => {
                let s = self.new_state();
                let e = self.new_state();
                self.add_epsilon(s, e);
                NfaFragment { start: s, end: e }
            }
            TokenPattern::Literal(lit) => {
                let s = self.new_state();
                let mut cur = s;
                for b in lit.as_bytes() {
                    let n = self.new_state();
                    self.add_range(cur, *b, *b, n);
                    cur = n;
                }
                NfaFragment { start: s, end: cur }
            }
            TokenPattern::Class(cc) => {
                let s = self.new_state();
                let e = self.new_state();
                for (lo, hi) in class_ranges(cc) {
                    self.add_range(s, lo, hi, e);
                }
                NfaFragment { start: s, end: e }
            }
            TokenPattern::Ref(n) => unreachable!("unresolved token ref `{}` reached lexer DFA", n),
            TokenPattern::Seq(xs) => {
                if xs.is_empty() {
                    return self.compile(&TokenPattern::Empty);
                }
                let mut iter = xs.iter();
                let first = self.compile(iter.next().unwrap());
                let mut start = first.start;
                let mut end = first.end;
                for x in iter {
                    let f = self.compile(x);
                    self.add_epsilon(end, f.start);
                    end = f.end;

                    let _ = &mut start;
                }
                NfaFragment { start, end }
            }
            TokenPattern::Alt(xs) => {
                let s = self.new_state();
                let e = self.new_state();
                for x in xs {
                    let f = self.compile(x);
                    self.add_epsilon(s, f.start);
                    self.add_epsilon(f.end, e);
                }
                NfaFragment { start: s, end: e }
            }
            TokenPattern::Opt(x) => {
                let f = self.compile(x);
                let s = self.new_state();
                let e = self.new_state();
                self.add_epsilon(s, f.start);
                self.add_epsilon(f.end, e);
                self.add_epsilon(s, e);
                NfaFragment { start: s, end: e }
            }
            TokenPattern::Star(x) => {
                let f = self.compile(x);
                let s = self.new_state();
                let e = self.new_state();
                self.add_epsilon(s, f.start);
                self.add_epsilon(f.end, s);
                self.add_epsilon(s, e);
                NfaFragment { start: s, end: e }
            }
            TokenPattern::Plus(x) => {
                let first = self.compile(x);
                let star = self.compile(&TokenPattern::Star(Box::new((**x).clone())));
                self.add_epsilon(first.end, star.start);
                NfaFragment {
                    start: first.start,
                    end: star.end,
                }
            }
        }
    }
}

fn build_nfa(tokens: &[TokenInfo]) -> Nfa {
    let mut b = NfaBuilder::new();
    let start = b.new_state();
    for tok in tokens {
        let frag = b.compile(&tok.pattern);
        b.add_epsilon(start, frag.start);
        b.set_accept(frag.end, tok.kind);
    }
    Nfa {
        states: b.states,
        start,
    }
}

/// Reduce a [`CharClass`] to a sorted list of disjoint, gap-separated
/// inclusive byte ranges. Codepoints above `0xFF` are clamped to byte
/// space (matching the existing byte-DFA semantics — multi-byte codepoints
/// are surfaced through their constituent bytes by the parser).
fn class_ranges(cc: &CharClass) -> Vec<(u8, u8)> {
    let mut ranges: Vec<(u8, u8)> = cc
        .items
        .iter()
        .filter_map(|it| match *it {
            ClassItem::Char(c) if c <= 0xFF => Some((c as u8, c as u8)),
            ClassItem::Char(_) => None,
            ClassItem::Range(lo, hi) => {
                if lo > 0xFF {
                    None
                } else {
                    Some((lo as u8, hi.min(0xFF) as u8))
                }
            }
        })
        .collect();
    ranges.sort();
    let mut merged: Vec<(u8, u8)> = Vec::new();
    for (lo, hi) in ranges {
        if let Some(last) = merged.last_mut() {
            // Overlap or adjacency (last.hi + 1 == lo) → extend.
            if last.1 >= lo || (last.1 < 255 && last.1 + 1 == lo) {
                last.1 = last.1.max(hi);
                continue;
            }
        }
        merged.push((lo, hi));
    }
    if cc.negated {
        complement(&merged)
    } else {
        merged
    }
}

/// Complement of `ranges` over `[0, 255]`. Assumes `ranges` is sorted and
/// gap-separated (the form [`class_ranges`] produces before negation).
fn complement(ranges: &[(u8, u8)]) -> Vec<(u8, u8)> {
    let mut out: Vec<(u8, u8)> = Vec::new();
    let mut cursor: u16 = 0;
    for &(lo, hi) in ranges {
        if cursor < lo as u16 {
            out.push((cursor as u8, lo - 1));
        }
        cursor = hi as u16 + 1;
    }
    if cursor <= 255 {
        out.push((cursor as u8, 255));
    }
    out
}

#[derive(Copy, Clone)]
enum Endpoint {
    /// `target` becomes active at this position.
    Open(NfaStateId),
    /// `target` becomes inactive at this position (events use `hi + 1` so
    /// the close lands one past the last covered byte).
    Close(NfaStateId),
}

/// Sweep-line over `(range, target)` pairs, returning the maximal disjoint
/// sub-intervals together with the set of NFA targets active on each one.
/// Empty result is empty input.
fn partition_ranges(
    edges: &[((u8, u8), NfaStateId)],
) -> Vec<((u8, u8), BTreeSet<NfaStateId>)> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut events: Vec<(u16, Endpoint)> = Vec::with_capacity(edges.len() * 2);
    for &((lo, hi), target) in edges {
        events.push((lo as u16, Endpoint::Open(target)));
        events.push((hi as u16 + 1, Endpoint::Close(target)));
    }
    events.sort_by_key(|(p, _)| *p);

    let mut out: Vec<((u8, u8), BTreeSet<NfaStateId>)> = Vec::new();
    let mut active: BTreeSet<NfaStateId> = BTreeSet::new();
    let mut i = 0;
    while i < events.len() {
        let cur_pos = events[i].0;
        // Apply every event at this position before reading off the
        // active set — Open and Close at the same position cancel
        // cleanly because the result only matters for the *next* range.
        while i < events.len() && events[i].0 == cur_pos {
            match events[i].1 {
                Endpoint::Open(t) => {
                    active.insert(t);
                }
                Endpoint::Close(t) => {
                    active.remove(&t);
                }
            }
            i += 1;
        }
        let next_pos = if i < events.len() { events[i].0 } else { 256 };
        if !active.is_empty() && cur_pos < next_pos {
            out.push((
                (cur_pos as u8, (next_pos - 1) as u8),
                active.clone(),
            ));
        }
    }
    out
}

fn subset_construct(nfa: &Nfa) -> Vec<DfaState> {
    let mut set_to_id: HashMap<BTreeSet<NfaStateId>, u32> = HashMap::new();
    let mut states: Vec<DfaState> = Vec::new();
    let mut queue: VecDeque<BTreeSet<NfaStateId>> = VecDeque::new();

    let start_set = epsilon_closure(nfa, [nfa.start].into_iter().collect());
    set_to_id.insert(start_set.clone(), START);
    states.push(new_dfa_state(START, nfa, &start_set));
    queue.push_back(start_set);

    while let Some(cur_set) = queue.pop_front() {
        let cur_id = set_to_id[&cur_set];

        // Collect this DFA state's outgoing NFA range edges.
        let mut edges: Vec<((u8, u8), NfaStateId)> = Vec::new();
        for &s in &cur_set {
            for &(range, t) in &nfa.states[s].range_trans {
                edges.push((range, t));
            }
        }

        // Partition into disjoint sub-intervals; each interval maps to a
        // set of NFA targets, which we close and look up / register as a
        // DFA state. Track each target's accept kind alongside its ranges
        // so we can fold it onto the emitted `ByteArm`.
        let mut by_target: BTreeMap<u32, (Option<i16>, Vec<(u8, u8)>)> = BTreeMap::new();
        for ((lo, hi), targets) in partition_ranges(&edges) {
            let closed = epsilon_closure(nfa, targets);
            let tgt_id = if let Some(id) = set_to_id.get(&closed) {
                *id
            } else {
                let id = (states.len() as u32) + 1; // +1 because state 0 = DEAD is reserved.
                set_to_id.insert(closed.clone(), id);
                states.push(new_dfa_state(id, nfa, &closed));
                queue.push_back(closed);
                id
            };
            let target_accept = states[(tgt_id - START) as usize].accept;
            by_target
                .entry(tgt_id)
                .or_insert_with(|| (target_accept, Vec::new()))
                .1
                .push((lo, hi));
        }

        // Per-target ranges arrive in ascending order from the sweep;
        // merge any that turned out to be byte-adjacent so each ByteArm
        // carries the minimum-cardinality range list.
        let arms: Vec<ByteArm> = by_target
            .into_iter()
            .map(|(target, (target_accept, ranges))| ByteArm {
                target,
                target_accept,
                ranges: merge_adjacent(ranges),
            })
            .collect();
        states[(cur_id - START) as usize].arms = arms;
    }

    states
}

/// Merge adjacent byte ranges: `[(0, 5), (6, 9)]` → `[(0, 9)]`. Input is
/// expected to be sorted (which the sweep guarantees).
fn merge_adjacent(ranges: Vec<(u8, u8)>) -> Vec<(u8, u8)> {
    let mut out: Vec<(u8, u8)> = Vec::new();
    for (lo, hi) in ranges {
        if let Some(last) = out.last_mut() {
            if last.1 < 255 && last.1 + 1 == lo {
                last.1 = hi;
                continue;
            }
        }
        out.push((lo, hi));
    }
    out
}

fn new_dfa_state(id: u32, nfa: &Nfa, set: &BTreeSet<NfaStateId>) -> DfaState {
    // Priority-ties in the NFA collapse here: when a single DFA state
    // accepts multiple kinds, we keep the smallest id — which matches the
    // grammar's declaration order and gives earlier tokens precedence (for
    // example, keyword `if` beats generic `IDENT`).
    let mut accept: Option<i16> = None;
    for &s in set {
        if let Some(k) = nfa.states[s].accept {
            accept = Some(match accept {
                Some(prev) => prev.min(k),
                None => k,
            });
        }
    }
    DfaState {
        id,
        arms: Vec::new(),
        accept,
    }
}

fn epsilon_closure(nfa: &Nfa, seeds: BTreeSet<NfaStateId>) -> BTreeSet<NfaStateId> {
    let mut out = seeds.clone();
    let mut stack: Vec<NfaStateId> = seeds.into_iter().collect();
    while let Some(s) = stack.pop() {
        for &e in &nfa.states[s].epsilon {
            if out.insert(e) {
                stack.push(e);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(s: &DfaState, b: u8) -> u32 {
        for arm in &s.arms {
            for &(lo, hi) in &arm.ranges {
                if lo <= b && b <= hi {
                    return arm.target;
                }
            }
        }
        DEAD
    }

    /// Find a state by id (real states only; the dead sentinel never
    /// appears in `dfa`).
    fn by_id(dfa: &[DfaState], id: u32) -> &DfaState {
        dfa.iter().find(|s| s.id == id).expect("state id not in dfa")
    }

    fn scan(dfa: &[DfaState], bytes: &[u8]) -> Option<(usize, i16)> {
        let mut state = START;
        let mut pos = 0;
        let mut last: Option<(usize, i16)> = None;
        loop {
            if pos < bytes.len() {
                let next = step(by_id(dfa, state), bytes[pos]);
                if next != DEAD {
                    state = next;
                    pos += 1;
                    if let Some(k) = by_id(dfa, state).accept {
                        last = Some((pos, k));
                    }
                    continue;
                }
            }
            return last;
        }
    }

    fn tok(name: &str, pat: TokenPattern) -> TokenInfo {
        TokenInfo {
            name: name.into(),
            pattern: pat,
            skip: false,
            kind: 0,
        }
    }

    fn toks(xs: Vec<TokenInfo>) -> Vec<TokenInfo> {
        xs.into_iter()
            .enumerate()
            .map(|(i, mut t)| {
                t.kind = (i + 1) as i16;
                t
            })
            .collect()
    }

    fn lit(s: &str) -> TokenPattern {
        TokenPattern::Literal(s.to_string())
    }
    fn cls(items: Vec<ClassItem>, neg: bool) -> TokenPattern {
        TokenPattern::Class(CharClass {
            negated: neg,
            items,
        })
    }

    #[test]
    fn output_omits_dead_state_and_starts_at_start() {
        let t = toks(vec![tok("A", lit("a"))]);
        let dfa = compile(&t);
        assert!(!dfa.is_empty());
        assert_eq!(dfa[0].id, START);
        assert!(dfa.iter().all(|s| s.id != DEAD));
    }

    #[test]
    fn longest_match_and_priority() {
        let t = toks(vec![
            tok("IF", lit("if")),
            tok(
                "IDENT",
                TokenPattern::Plus(Box::new(cls(
                    vec![ClassItem::Range(b'a' as u32, b'z' as u32)],
                    false,
                ))),
            ),
        ]);
        let dfa = compile(&t);

        assert_eq!(scan(&dfa, b"iff"), Some((3, 2)));
        assert_eq!(scan(&dfa, b"if"), Some((2, 1)));
        assert_eq!(scan(&dfa, b"z"), Some((1, 2)));
        assert_eq!(scan(&dfa, b"1"), None);
    }

    #[test]
    fn json_shape_tokens() {
        let digit = cls(vec![ClassItem::Range(b'0' as u32, b'9' as u32)], false);
        let number = TokenPattern::Plus(Box::new(digit));
        let t = toks(vec![
            tok("LBRACE", lit("{")),
            tok("TRUE", lit("true")),
            tok("NUMBER", number),
            tok(
                "WS",
                TokenPattern::Plus(Box::new(cls(vec![ClassItem::Char(b' ' as u32)], false))),
            ),
        ]);
        let dfa = compile(&t);
        assert_eq!(scan(&dfa, b"{"), Some((1, 1)));
        assert_eq!(scan(&dfa, b"true"), Some((4, 2)));
        assert_eq!(scan(&dfa, b"123"), Some((3, 3)));
        assert_eq!(scan(&dfa, b"   x"), Some((3, 4)));
    }

    #[test]
    fn negated_class_accepts_other_bytes() {
        let t = toks(vec![tok(
            "X",
            cls(
                vec![ClassItem::Char(b'a' as u32), ClassItem::Char(b'b' as u32)],
                true,
            ),
        )]);
        let dfa = compile(&t);
        assert_eq!(scan(&dfa, b"c"), Some((1, 1)));
        assert_eq!(scan(&dfa, b"a"), None);
        assert_eq!(scan(&dfa, b"b"), None);
    }

    #[test]
    fn star_and_empty_match() {
        let t = toks(vec![tok("XS", TokenPattern::Star(Box::new(lit("x"))))]);
        let dfa = compile(&t);
        assert_eq!(scan(&dfa, b"xxx"), Some((3, 1)));
        assert_eq!(scan(&dfa, b"y"), None);
    }

    #[test]
    fn arms_collapse_contiguous_bytes_into_ranges() {
        // [a-z]+ — the start state has 26 transitions all going to the
        // same target. They should fold into a single arm with one range.
        let t = toks(vec![tok(
            "ID",
            TokenPattern::Plus(Box::new(cls(
                vec![ClassItem::Range(b'a' as u32, b'z' as u32)],
                false,
            ))),
        )]);
        let dfa = compile(&t);
        let arms = &dfa[0].arms;
        assert_eq!(arms.len(), 1);
        assert_eq!(arms[0].ranges, vec![(b'a', b'z')]);
    }

    #[test]
    fn arms_split_disjoint_byte_groups_into_ranges_per_arm() {
        // ('0'..'9' | 'a'..'f') — one target, two non-contiguous ranges
        // collapse into a single arm carrying both ranges.
        let t = toks(vec![tok(
            "HEX",
            cls(
                vec![
                    ClassItem::Range(b'0' as u32, b'9' as u32),
                    ClassItem::Range(b'a' as u32, b'f' as u32),
                ],
                false,
            ),
        )]);
        let dfa = compile(&t);
        let arms = &dfa[0].arms;
        assert_eq!(arms.len(), 1);
        assert_eq!(arms[0].ranges, vec![(b'0', b'9'), (b'a', b'f')]);
    }

    #[test]
    fn arms_grouped_by_target_state() {
        // Two literal tokens with different first bytes — distinct targets.
        let t = toks(vec![tok("A", lit("a")), tok("B", lit("b"))]);
        let dfa = compile(&t);
        let arms = &dfa[0].arms;
        assert_eq!(arms.len(), 2);
        // Each arm should be a single-byte range and they should differ.
        for arm in arms {
            assert_eq!(arm.ranges.len(), 1);
            let (lo, hi) = arm.ranges[0];
            assert_eq!(lo, hi);
        }
        assert_ne!(arms[0].target, arms[1].target);
    }

    #[test]
    fn class_ranges_merges_overlapping_and_adjacent() {
        let cc = CharClass {
            negated: false,
            items: vec![
                ClassItem::Range(b'a' as u32, b'c' as u32),
                ClassItem::Range(b'b' as u32, b'e' as u32), // overlaps
                ClassItem::Char(b'f' as u32),                // adjacent to the merged run
                ClassItem::Char(b'z' as u32),                // disjoint
            ],
        };
        assert_eq!(class_ranges(&cc), vec![(b'a', b'f'), (b'z', b'z')]);
    }

    #[test]
    fn class_ranges_negation_matches_complement_over_all_bytes() {
        let cc = CharClass {
            negated: true,
            items: vec![ClassItem::Char(b'a' as u32)],
        };
        assert_eq!(class_ranges(&cc), vec![(0, b'a' - 1), (b'a' + 1, 255)]);

        // Negated empty class = all bytes.
        let any = CharClass {
            negated: true,
            items: vec![],
        };
        assert_eq!(class_ranges(&any), vec![(0, 255)]);
    }

    #[test]
    fn partition_ranges_splits_overlapping_into_disjoint_pieces() {
        // Two NFA states (1 and 2) with overlapping ranges.
        let edges = vec![((b'a', b'd'), 1usize), ((b'b', b'e'), 2usize)];
        let parts = partition_ranges(&edges);
        let names: Vec<((u8, u8), Vec<usize>)> = parts
            .into_iter()
            .map(|(r, set)| (r, set.into_iter().collect()))
            .collect();
        assert_eq!(
            names,
            vec![
                ((b'a', b'a'), vec![1]),
                ((b'b', b'd'), vec![1, 2]),
                ((b'e', b'e'), vec![2]),
            ]
        );
    }
}
