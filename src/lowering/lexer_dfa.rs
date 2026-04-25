//! Compile a set of [`TokenPattern`]s into a deterministic byte-level DFA.
//!
//! Thompson-style NFA construction followed by subset construction.
//! Reserves state `0` ([`DEAD`]) as a sink — every missing transition lands
//! there, which matches the runtime's single-branch "exit on 0" check. The
//! start state is always [`START`].
//!
//! The output is a flat `Vec<DfaState>` indexed by state id; each entry
//! carries its accept kind and its transitions already collapsed into
//! [`ByteArm`]s, ready for code generation.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::grammar::ir::{CharClass, ClassItem, TokenPattern};
use crate::lowering::TokenInfo;

/// Reserved DFA state: the dead sink every missing transition points at.
/// Real states start at [`START`].
pub const DEAD: u32 = 0;

/// Start state of every compiled lexer DFA. State 0 is reserved for the
/// dead sink, so the start always lands at id 1.
pub const START: u32 = 1;

/// One DFA state: its byte transitions already grouped into [`ByteArm`]s
/// plus an optional accepted token kind. The state id is the index in the
/// `Vec<DfaState>` returned by [`compile`]; index 0 is [`DEAD`] and has no
/// arms.
#[derive(Clone, Debug)]
pub struct DfaState {
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
    /// Inclusive byte ranges that all transition to `target`.
    pub ranges: Vec<(u8, u8)>,
}

/// Compile tokens to a DFA. Each token becomes an NFA fragment whose end
/// state accepts its kind; alternation at the start state lets the lexer
/// try every pattern in parallel. Subset construction then determinises
/// the combined machine and the per-state byte transitions are collapsed
/// into [`ByteArm`]s before returning.
pub fn compile(tokens: &[TokenInfo]) -> Vec<DfaState> {
    let nfa = build_nfa(tokens);
    let raw = subset_construct(&nfa);
    raw.into_iter()
        .map(|s| DfaState {
            arms: collapse_arms(&s.trans),
            accept: s.accept,
        })
        .collect()
}

/// Group a 256-byte transition row into [`ByteArm`]s: bytes sharing a
/// target state fold into one arm, and contiguous bytes within an arm
/// collapse into ranges.
fn collapse_arms(trans: &[u32]) -> Vec<ByteArm> {
    let mut by_target: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    for (b, &t) in trans.iter().enumerate() {
        if t != DEAD {
            by_target.entry(t).or_default().push(b as u8);
        }
    }
    by_target
        .into_iter()
        .map(|(target, bytes)| {
            let mut ranges: Vec<(u8, u8)> = Vec::new();
            let mut iter = bytes.into_iter();
            if let Some(first) = iter.next() {
                let mut lo = first;
                let mut hi = first;
                for b in iter {
                    if b == hi + 1 {
                        hi = b;
                    } else {
                        ranges.push((lo, hi));
                        lo = b;
                        hi = b;
                    }
                }
                ranges.push((lo, hi));
            }
            ByteArm { target, ranges }
        })
        .collect()
}

type NfaStateId = usize;

struct Nfa {
    states: Vec<NfaState>,
    start: NfaStateId,
}

#[derive(Default)]
struct NfaState {
    byte_trans: Vec<(u8, NfaStateId)>,
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

    fn add_byte(&mut self, from: NfaStateId, byte: u8, to: NfaStateId) {
        self.states[from].byte_trans.push((byte, to));
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
                    self.add_byte(cur, *b, n);
                    cur = n;
                }
                NfaFragment { start: s, end: cur }
            }
            TokenPattern::Class(cc) => {
                let s = self.new_state();
                let e = self.new_state();
                for b in class_bytes(cc) {
                    self.add_byte(s, b, e);
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

fn class_bytes(cc: &CharClass) -> Vec<u8> {
    let mut hit = [false; 256];
    for it in &cc.items {
        match *it {
            ClassItem::Char(c) => {
                if c <= 0xFF {
                    hit[c as usize] = true;
                }
            }
            ClassItem::Range(lo, hi) => {
                let lo = lo.min(0xFF);
                let hi = hi.min(0xFF);
                for b in lo..=hi {
                    hit[b as usize] = true;
                }
            }
        }
    }
    if cc.negated {
        (0..=255u8).filter(|b| !hit[*b as usize]).collect()
    } else {
        (0..=255u8).filter(|b| hit[*b as usize]).collect()
    }
}

/// Raw DFA state used during subset construction. Carries a 256-byte
/// transition row that's natural for the algorithm to fill in; collapsed
/// into [`DfaState`] by [`compile`] before being exposed.
struct RawDfaState {
    trans: Vec<u32>,
    accept: Option<i16>,
}

fn subset_construct(nfa: &Nfa) -> Vec<RawDfaState> {
    let mut set_to_id: HashMap<BTreeSet<NfaStateId>, u32> = HashMap::new();
    let mut states: Vec<RawDfaState> = vec![RawDfaState {
        trans: vec![0; 256],
        accept: None,
    }];
    let mut queue: VecDeque<BTreeSet<NfaStateId>> = VecDeque::new();

    let start_set = epsilon_closure(nfa, [nfa.start].into_iter().collect());
    set_to_id.insert(start_set.clone(), START);
    states.push(build_raw_state(nfa, &start_set));
    queue.push_back(start_set);

    while let Some(cur_set) = queue.pop_front() {
        let cur_id = set_to_id[&cur_set];

        let mut by_byte: BTreeMap<u8, BTreeSet<NfaStateId>> = BTreeMap::new();
        for &s in &cur_set {
            for &(b, t) in &nfa.states[s].byte_trans {
                by_byte.entry(b).or_default().insert(t);
            }
        }
        for (b, targets) in by_byte {
            let closed = epsilon_closure(nfa, targets);
            let tgt_id = if let Some(id) = set_to_id.get(&closed) {
                *id
            } else {
                let id = states.len() as u32;
                set_to_id.insert(closed.clone(), id);
                states.push(build_raw_state(nfa, &closed));
                queue.push_back(closed);
                id
            };
            states[cur_id as usize].trans[b as usize] = tgt_id;
        }
    }

    states
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

fn build_raw_state(nfa: &Nfa, set: &BTreeSet<NfaStateId>) -> RawDfaState {
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
    RawDfaState {
        trans: vec![0; 256],
        accept,
    }
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

    fn scan(dfa: &[DfaState], bytes: &[u8]) -> Option<(usize, i16)> {
        let mut state = START;
        let mut pos = 0;
        let mut last: Option<(usize, i16)> = None;
        loop {
            if pos < bytes.len() {
                let next = step(&dfa[state as usize], bytes[pos]);
                if next != DEAD {
                    state = next;
                    pos += 1;
                    if let Some(k) = dfa[state as usize].accept {
                        last = Some((pos, k));
                    }
                    continue;
                }
            }
            return last;
        }
    }

    #[test]
    fn dead_state_at_zero_real_states_shifted() {
        let t = toks(vec![tok("A", lit("a"))]);
        let dfa = compile(&t);

        assert_eq!(dfa[DEAD as usize].accept, None);
        assert!(dfa[DEAD as usize].arms.is_empty());

        assert_eq!(START, 1);
        assert!(dfa.len() >= 2);
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
        let arms = &dfa[START as usize].arms;
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
        let arms = &dfa[START as usize].arms;
        assert_eq!(arms.len(), 1);
        assert_eq!(arms[0].ranges, vec![(b'0', b'9'), (b'a', b'f')]);
    }

    #[test]
    fn arms_grouped_by_target_state() {
        // Two literal tokens with different first bytes — distinct targets.
        let t = toks(vec![tok("A", lit("a")), tok("B", lit("b"))]);
        let dfa = compile(&t);
        let arms = &dfa[START as usize].arms;
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
    fn dead_state_has_no_arms() {
        let t = toks(vec![tok("A", lit("a"))]);
        let dfa = compile(&t);
        assert!(dfa[DEAD as usize].arms.is_empty());
    }
}
