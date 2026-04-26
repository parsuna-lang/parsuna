//! Compile a set of [`TokenPattern`]s into a deterministic byte-level DFA.
//!
//! Thompson-style NFA construction followed by interval-based subset
//! construction. NFA edges and DFA arms carry inclusive byte ranges
//! `(u8, u8)`. Codepoint ranges in character classes (`'a'..'z'`, `.`,
//! negations) are expanded to UTF-8 byte sequences before they reach the
//! NFA, so a class like `'\u{00E0}'..'\u{017F}'` becomes a small
//! alternation of multi-byte sequences rather than a malformed byte set;
//! ASCII ranges still cost one edge. Subset construction uses a
//! sweep-line over endpoints to find the maximal sub-intervals on which
//! the active set of NFA targets is constant.
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
    pub accept: Option<u16>,
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
    pub target_accept: Option<u16>,
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
    accept: Option<u16>,
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

    fn set_accept(&mut self, s: NfaStateId, kind: u16) {
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
                for seq in class_byte_seqs(cc) {
                    if seq.is_empty() {
                        self.add_epsilon(s, e);
                        continue;
                    }
                    let mut cur = s;
                    let last = seq.len() - 1;
                    for (i, (lo, hi)) in seq.into_iter().enumerate() {
                        let target = if i == last { e } else { self.new_state() };
                        self.add_range(cur, lo, hi, target);
                        cur = target;
                    }
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

const MAX_CODEPOINT: u32 = 0x10FFFF;
const SURROGATE_LO: u32 = 0xD800;
const SURROGATE_HI: u32 = 0xDFFF;

/// Reduce a [`CharClass`] to a list of UTF-8 byte sequences. Each inner
/// `Vec<(u8, u8)>` is a sequence of byte ranges to match in order; the
/// outer list is the alternation. Codepoint ranges expand to their UTF-8
/// encoding via the canonical Russ-Cox decomposition, so `'\u{00E0}'..
/// '\u{00FF}'` becomes one 2-byte sequence rather than a stray byte
/// transition that would never match real UTF-8 input. Surrogates
/// `U+D800..=U+DFFF` are silently excluded — they are not Unicode scalar
/// values and cannot legally appear in UTF-8.
fn class_byte_seqs(cc: &CharClass) -> Vec<Vec<(u8, u8)>> {
    let mut cp_ranges: Vec<(u32, u32)> = cc
        .items
        .iter()
        .filter_map(|it| match *it {
            ClassItem::Char(c) if c <= MAX_CODEPOINT => Some((c, c)),
            ClassItem::Char(_) => None,
            ClassItem::Range(lo, hi) if lo <= MAX_CODEPOINT && lo <= hi => {
                Some((lo, hi.min(MAX_CODEPOINT)))
            }
            ClassItem::Range(_, _) => None,
        })
        .collect();
    cp_ranges.sort();
    let cp_ranges = merge_codepoint_ranges(cp_ranges);

    let cp_ranges = if cc.negated {
        complement_codepoints(&cp_ranges)
    } else {
        exclude_surrogates(cp_ranges)
    };

    let mut out = Vec::new();
    for (lo, hi) in cp_ranges {
        utf8_byte_sequences(lo, hi, &mut out);
    }
    out
}

/// Coalesce sorted, possibly-overlapping codepoint ranges into a sorted,
/// gap-separated list. Adjacent ranges (`a.hi + 1 == b.lo`) merge.
fn merge_codepoint_ranges(sorted: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in sorted {
        if let Some(last) = out.last_mut() {
            if lo <= last.1.saturating_add(1) {
                last.1 = last.1.max(hi);
                continue;
            }
        }
        out.push((lo, hi));
    }
    out
}

/// Drop the surrogate range `U+D800..=U+DFFF` from each input range,
/// splitting ranges that straddle it.
fn exclude_surrogates(ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (lo, hi) in ranges {
        if hi < SURROGATE_LO || lo > SURROGATE_HI {
            out.push((lo, hi));
        } else if lo < SURROGATE_LO && hi <= SURROGATE_HI {
            out.push((lo, SURROGATE_LO - 1));
        } else if lo >= SURROGATE_LO && hi > SURROGATE_HI {
            out.push((SURROGATE_HI + 1, hi));
        } else if lo < SURROGATE_LO && hi > SURROGATE_HI {
            out.push((lo, SURROGATE_LO - 1));
            out.push((SURROGATE_HI + 1, hi));
        }
        // else: range entirely within surrogates — drop.
    }
    out
}

/// Complement of `ranges` over `[U+0000, U+10FFFF]` minus surrogates.
/// Assumes `ranges` is sorted and gap-separated.
fn complement_codepoints(ranges: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut raw: Vec<(u32, u32)> = Vec::new();
    let mut cursor: u32 = 0;
    for &(lo, hi) in ranges {
        if cursor < lo {
            raw.push((cursor, lo - 1));
        }
        cursor = hi.saturating_add(1);
    }
    if cursor <= MAX_CODEPOINT {
        raw.push((cursor, MAX_CODEPOINT));
    }
    exclude_surrogates(raw)
}

/// Append the UTF-8 byte sequences that match every codepoint in
/// `[lo, hi]` to `out`. Caller must ensure the range is surrogate-free.
fn utf8_byte_sequences(lo: u32, hi: u32, out: &mut Vec<Vec<(u8, u8)>>) {
    if lo > hi {
        return;
    }
    debug_assert!(
        hi < SURROGATE_LO || lo > SURROGATE_HI,
        "utf8_byte_sequences must not see surrogate codepoints",
    );

    // Split at the boundaries where UTF-8 byte length changes.
    let breaks = [0x7Fu32, 0x7FF, 0xFFFF, MAX_CODEPOINT];
    let mut cursor = lo;
    while cursor <= hi {
        let break_at = *breaks.iter().find(|&&b| cursor <= b).unwrap();
        let chunk_hi = hi.min(break_at);
        emit_same_len(cursor, chunk_hi, out);
        if break_at == MAX_CODEPOINT {
            break;
        }
        cursor = break_at + 1;
    }
}

fn emit_same_len(lo: u32, hi: u32, out: &mut Vec<Vec<(u8, u8)>>) {
    let n = utf8_len(lo);
    debug_assert_eq!(n, utf8_len(hi));
    let mut lo_buf = [0u8; 4];
    let mut hi_buf = [0u8; 4];
    char::from_u32(lo).expect("non-surrogate scalar value").encode_utf8(&mut lo_buf);
    char::from_u32(hi).expect("non-surrogate scalar value").encode_utf8(&mut hi_buf);
    split_seqs(&lo_buf[..n], &hi_buf[..n], out);
}

fn utf8_len(c: u32) -> usize {
    match c {
        0..=0x7F => 1,
        0x80..=0x7FF => 2,
        0x800..=0xFFFF => 3,
        _ => 4,
    }
}

/// Decompose `[s, e]` (same-length UTF-8 byte sequences) into a list of
/// alternation byte sequences matching every value between them. At each
/// step:
///   - if every trailing byte of `s` is `0x80` and every trailing byte of
///     `e` is `0xBF`, the whole range collapses into a single sequence;
///   - otherwise, peel off a "lower" piece (first byte fixed at `s[0]`,
///     trailing bytes lifted up to `0xBF`), an optional "middle" piece
///     (first byte ranges over the gap, trailing bytes free over
///     `0x80..=0xBF`), and an "upper" piece (first byte fixed at `e[0]`,
///     trailing bytes recurse from `0x80` minimum).
fn split_seqs(s: &[u8], e: &[u8], out: &mut Vec<Vec<(u8, u8)>>) {
    let n = s.len();
    debug_assert_eq!(n, e.len());
    debug_assert!(n > 0);

    if s[1..].iter().all(|&b| b == 0x80) && e[1..].iter().all(|&b| b == 0xBF) {
        let mut seq = Vec::with_capacity(n);
        seq.push((s[0], e[0]));
        for _ in 1..n {
            seq.push((0x80, 0xBF));
        }
        out.push(seq);
        return;
    }

    if s[0] == e[0] {
        let mut sub = Vec::new();
        split_seqs(&s[1..], &e[1..], &mut sub);
        for x in sub {
            let mut seq = Vec::with_capacity(n);
            seq.push((s[0], s[0]));
            seq.extend(x);
            out.push(seq);
        }
        return;
    }

    let suffix_len = n - 1;

    let upper_of_lower: Vec<u8> = vec![0xBFu8; suffix_len];
    let mut lower = Vec::new();
    split_seqs(&s[1..], &upper_of_lower, &mut lower);
    for x in lower {
        let mut seq = Vec::with_capacity(n);
        seq.push((s[0], s[0]));
        seq.extend(x);
        out.push(seq);
    }

    if s[0] + 1 < e[0] {
        let mut seq = Vec::with_capacity(n);
        seq.push((s[0] + 1, e[0] - 1));
        for _ in 0..suffix_len {
            seq.push((0x80, 0xBF));
        }
        out.push(seq);
    }

    let lower_of_upper: Vec<u8> = vec![0x80u8; suffix_len];
    let mut upper = Vec::new();
    split_seqs(&lower_of_upper, &e[1..], &mut upper);
    for x in upper {
        let mut seq = Vec::with_capacity(n);
        seq.push((e[0], e[0]));
        seq.extend(x);
        out.push(seq);
    }
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
        let mut by_target: BTreeMap<u32, (Option<u16>, Vec<(u8, u8)>)> = BTreeMap::new();
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
    let mut accept: Option<u16> = None;
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

    fn scan(dfa: &[DfaState], bytes: &[u8]) -> Option<(usize, u16)> {
        let mut state = START;
        let mut pos = 0;
        let mut last: Option<(usize, u16)> = None;
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
                t.kind = (i + 1) as u16;
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
    fn class_byte_seqs_merges_overlapping_codepoint_ranges() {
        let cc = CharClass {
            negated: false,
            items: vec![
                ClassItem::Range(b'a' as u32, b'c' as u32),
                ClassItem::Range(b'b' as u32, b'e' as u32), // overlaps
                ClassItem::Char(b'f' as u32),                // adjacent to the merged run
                ClassItem::Char(b'z' as u32),                // disjoint
            ],
        };
        assert_eq!(
            class_byte_seqs(&cc),
            vec![vec![(b'a', b'f')], vec![(b'z', b'z')]],
        );
    }

    #[test]
    fn class_byte_seqs_expands_two_byte_codepoint_range() {
        // U+00E0..U+00FF: shared lead byte 0xC3, trailing 0xA0..0xBF.
        let cc = CharClass {
            negated: false,
            items: vec![ClassItem::Range(0x00E0, 0x00FF)],
        };
        assert_eq!(
            class_byte_seqs(&cc),
            vec![vec![(0xC3, 0xC3), (0xA0, 0xBF)]],
        );
    }

    #[test]
    fn class_byte_seqs_drops_nothing_for_high_unicode_range() {
        // U+0100..U+017F (Latin Extended-A): shared lead byte 0xC4 / 0xC5.
        let cc = CharClass {
            negated: false,
            items: vec![ClassItem::Range(0x0100, 0x017F)],
        };
        assert_eq!(
            class_byte_seqs(&cc),
            vec![vec![(0xC4, 0xC5), (0x80, 0xBF)]],
        );
    }

    #[test]
    fn class_byte_seqs_splits_range_crossing_byte_length_boundary() {
        // U+007E..U+0080 straddles the 1-byte → 2-byte boundary; expect a
        // 1-byte sequence for U+007E..U+007F and a 2-byte for U+0080.
        let cc = CharClass {
            negated: false,
            items: vec![ClassItem::Range(0x007E, 0x0080)],
        };
        let seqs = class_byte_seqs(&cc);
        assert_eq!(
            seqs,
            vec![
                vec![(0x7E, 0x7F)],
                vec![(0xC2, 0xC2), (0x80, 0x80)],
            ],
        );
    }

    #[test]
    fn negation_complements_over_unicode_excluding_surrogates() {
        let t = toks(vec![tok(
            "X",
            cls(vec![ClassItem::Char(b'a' as u32)], true),
        )]);
        let dfa = compile(&t);
        // ASCII byte that isn't 'a' matches as a 1-byte token.
        assert_eq!(scan(&dfa, b"b"), Some((1, 1)));
        assert_eq!(scan(&dfa, b"a"), None);
        // A 2-byte UTF-8 codepoint matches in one go (longest-match=2).
        assert_eq!(scan(&dfa, "é".as_bytes()), Some((2, 1)));
        // A 4-byte codepoint also matches in one go.
        assert_eq!(scan(&dfa, "🎉".as_bytes()), Some((4, 1)));
    }

    #[test]
    fn dot_matches_any_one_codepoint() {
        // A negated empty class = `.` per the grammar spec.
        let t = toks(vec![tok("ANY", cls(vec![], true))]);
        let dfa = compile(&t);
        assert_eq!(scan(&dfa, b"a"), Some((1, 1)));
        assert_eq!(scan(&dfa, "é".as_bytes()), Some((2, 1)));
        assert_eq!(scan(&dfa, "中".as_bytes()), Some((3, 1)));
        assert_eq!(scan(&dfa, "🎉".as_bytes()), Some((4, 1)));
        // Stray UTF-8 continuation byte: not a valid codepoint start.
        assert_eq!(scan(&dfa, &[0x80]), None);
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
