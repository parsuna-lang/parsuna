//! Structured dumper for [`crate::lowering::StateTable`].
//!
//! Used by `parsuna debug lowering` and by the playground's IR tab.
//! Format: an interned `FIRST` pool, an interned `SYNC` pool, then one
//! block per state — `<id>  <label>` followed by indented `Enter` /
//! `Expect` / `Star` / `Dispatch` / etc. lines that mirror the
//! `Body → Tail` shape.
//!
//! The dump is built as `Vec<Vec<DumpSpan>>` where each inner span
//! carries a [`DumpSpanKind`]. [`lowering_spans`] returns the structured
//! form (used by the playground for color rendering); [`lowering_text`]
//! flattens the same data to a single string for the CLI debug dump.

use std::fmt::Write;

use crate::lowering::{
    Body, DispatchLeaf, DispatchTree, Instr, ModeActionInfo, StateTable, Tail,
};

/// One colored fragment of a dump line.
#[derive(Clone, Debug)]
pub struct DumpSpan {
    pub kind: DumpSpanKind,
    pub text: String,
}

/// Lexical category assigned to each dump fragment, matching what the
/// playground stylesheet knows how to paint.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DumpSpanKind {
    /// Whitespace and other uncolored filler.
    Plain,
    /// Pure punctuation (`{`, `}`, `->`, `,`, `:`, `=`).
    Punct,
    /// Reserved words like `Enter`, `Expect`, `Star`, `look`, `sync`.
    Keyword,
    /// User-declared rule names plus state labels (which derive from rules).
    Rule,
    /// User-declared token names; `EOF` is included.
    Token,
    /// User-declared label names (`name:NAME` positions).
    Label,
    /// Numeric literals (state ids, look depth, FIRST_/SYNC_ pool indices).
    Number,
    /// Inline `; ...` annotations.
    Comment,
}

/// Render the full state table as structured per-line spans.
pub fn lowering_spans(st: &StateTable) -> Vec<Vec<DumpSpan>> {
    let mut b = SpanBuilder::new();

    // ---- FIRST pool ----
    b.kw("FIRST-set intern pool ");
    b.punct("(");
    b.num(st.first_sets.len().to_string());
    b.punct(")");
    b.plain(":");
    b.newline();
    for i in 0..st.first_sets.len() {
        b.plain("  ");
        b.num(format!("FIRST_{:<3}", i));
        b.plain(" ");
        emit_first_pool(&mut b, st, i as u32);
        b.newline();
    }
    b.newline();

    // ---- SYNC pool ----
    b.kw("SYNC-set intern pool ");
    b.punct("(");
    b.num(st.sync_sets.len().to_string());
    b.punct(")");
    b.plain(":");
    b.newline();
    for i in 0..st.sync_sets.len() {
        b.plain("  ");
        b.num(format!("SYNC_{:<3} ", i));
        b.plain(" ");
        emit_sync_set(&mut b, st, i as u32);
        b.newline();
    }
    b.newline();

    // ---- States ----
    b.kw("State table ");
    b.punct("(");
    b.num(st.states.len().to_string());
    b.plain(" states");
    b.punct(")");
    b.plain(":");
    b.newline();

    let entry_by_id: std::collections::HashMap<u32, &str> = st
        .entry_states
        .iter()
        .map(|(n, id)| (*id, n.as_str()))
        .collect();

    let id_width = st
        .states
        .keys()
        .last()
        .copied()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);

    for state in st.states.values() {
        b.plain(format!("{:>w$}", state.id, w = id_width));
        b.plain("  ");
        b.rule(&state.label);
        if let Some(name) = entry_by_id.get(&state.id) {
            b.plain("    ");
            b.comment(format!("; entry: {}", name));
        }
        b.newline();
        for instr in &state.body.instrs {
            b.plain(format!("{:>w$}      ", "", w = id_width));
            emit_instr(&mut b, instr, st);
            b.newline();
        }
        emit_tail(&mut b, &state.body.tail, st, &format!("{:>w$}      ", "", w = id_width));
        b.newline();
    }

    b.finish()
}

/// Plain-text rendering — joins the same span lines that
/// [`lowering_spans`] returns. Kept because the CLI debug dump and a
/// few existing call sites consume strings.
pub fn lowering_text(st: &StateTable) -> String {
    let lines = lowering_spans(st);
    spans_to_text(&lines)
}

/// Helper: flatten a span tree to a string, with newlines between lines.
pub fn spans_to_text(lines: &[Vec<DumpSpan>]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        for span in line {
            out.push_str(&span.text);
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

// ---------------------------------------------------------------------
// SpanBuilder
// ---------------------------------------------------------------------

/// Accumulates spans into the current line and flushes on `newline()`.
/// Public so `wasm/src/lib.rs` can build the analysis-summary header in
/// the same shape the state-table dump uses.
pub struct SpanBuilder {
    lines: Vec<Vec<DumpSpan>>,
    cur: Vec<DumpSpan>,
}

impl SpanBuilder {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            cur: Vec::new(),
        }
    }

    fn push(&mut self, kind: DumpSpanKind, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        // Coalesce consecutive same-kind spans so the rendered HTML is
        // smaller and selection / copy-paste don't expose hidden seams.
        if let Some(last) = self.cur.last_mut() {
            if last.kind == kind {
                last.text.push_str(&text);
                return;
            }
        }
        self.cur.push(DumpSpan { kind, text });
    }

    pub fn plain(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Plain, t);
    }
    pub fn punct(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Punct, t);
    }
    pub fn kw(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Keyword, t);
    }
    pub fn rule(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Rule, t);
    }
    pub fn token(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Token, t);
    }
    pub fn label(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Label, t);
    }
    pub fn num(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Number, t);
    }
    pub fn comment(&mut self, t: impl Into<String>) {
        self.push(DumpSpanKind::Comment, t);
    }

    pub fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.cur));
    }

    pub fn finish(mut self) -> Vec<Vec<DumpSpan>> {
        if !self.cur.is_empty() {
            self.lines.push(std::mem::take(&mut self.cur));
        }
        self.lines
    }
}

impl Default for SpanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------
// Emitters — typed equivalents of the old `format_*` helpers.
// ---------------------------------------------------------------------

fn emit_first_pool(b: &mut SpanBuilder, st: &StateTable, id: u32) {
    let Some(set) = st.first_sets.get(id as usize) else {
        b.num(format!("FIRST_{}", id));
        return;
    };
    b.punct("{");
    let mut seqs: Vec<&Vec<u16>> = set.seqs.iter().collect();
    seqs.sort();
    seqs.dedup();
    for (i, seq) in seqs.iter().enumerate() {
        if i > 0 {
            b.punct(",");
            b.plain(" ");
        }
        if seq.is_empty() {
            b.kw("ε");
        } else {
            for (j, kind) in seq.iter().enumerate() {
                if j > 0 {
                    b.plain(" ");
                }
                emit_token_kind(b, st, *kind);
            }
        }
    }
    b.punct("}");
}

fn emit_sync_set(b: &mut SpanBuilder, st: &StateTable, id: u32) {
    let Some(set) = st.sync_sets.get(id as usize) else {
        b.num(format!("SYNC_{}", id));
        return;
    };
    let mut kinds: Vec<u16> = set.kinds.clone();
    kinds.sort();
    kinds.dedup();
    b.punct("{");
    for (i, kind) in kinds.iter().enumerate() {
        if i > 0 {
            b.punct(",");
            b.plain(" ");
        }
        emit_token_kind(b, st, *kind);
    }
    b.punct("}");
}

fn emit_token_kind(b: &mut SpanBuilder, st: &StateTable, kind: u16) {
    if kind == parsuna_rt::TOKEN_EOF {
        b.kw("EOF");
        return;
    }
    if kind == 0 {
        b.plain("?");
        return;
    }
    match st.tokens.get(kind as usize - 1) {
        Some(t) => b.token(&t.name),
        None => b.plain("?"),
    }
}

fn emit_token_kind_fallback(b: &mut SpanBuilder, st: &StateTable, kind: u16, fallback: &str) {
    if kind == 0 {
        b.token(fallback);
        return;
    }
    match st.tokens.get(kind as usize - 1) {
        Some(t) => b.token(&t.name),
        None => b.token(fallback),
    }
}

fn emit_rule_kind(b: &mut SpanBuilder, st: &StateTable, kind: u16) {
    match st.rule_kinds.get(kind as usize) {
        Some(name) => b.rule(name),
        None => b.plain(format!("?{}", kind)),
    }
}

fn emit_state_ref(b: &mut SpanBuilder, st: &StateTable, id: u32) {
    match st.states.get(&id) {
        Some(s) if !s.label.is_empty() => b.rule(&s.label),
        _ => b.num(id.to_string()),
    }
}

fn emit_label(b: &mut SpanBuilder, st: &StateTable, id: u16) {
    if id == 0 {
        b.plain("?");
        return;
    }
    match st.labels.get((id - 1) as usize) {
        Some(name) => b.label(name),
        None => b.plain(format!("?{}", id)),
    }
}

#[allow(dead_code)]
pub(crate) fn mode_actions_suffix(st: &StateTable, kind: u16) -> String {
    let Some(t) = st.tokens.get(kind as usize - 1) else {
        return String::new();
    };
    if t.mode_actions.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = t
        .mode_actions
        .iter()
        .map(|a| match a {
            ModeActionInfo::Push(id) => {
                let name = st
                    .modes
                    .get(*id as usize)
                    .map(|m| m.name.as_str())
                    .unwrap_or("?");
                format!("push({})", name)
            }
            ModeActionInfo::Pop => "pop".to_string(),
        })
        .collect();
    let mut s = String::new();
    write!(s, " → {}", parts.join(", ")).unwrap();
    s
}

fn emit_instr(b: &mut SpanBuilder, op: &Instr, st: &StateTable) {
    match op {
        Instr::Enter(k) => {
            b.kw("Enter");
            b.plain(" ");
            emit_rule_kind(b, st, *k);
        }
        Instr::Exit(k) => {
            b.kw("Exit");
            b.plain(" ");
            emit_rule_kind(b, st, *k);
        }
        Instr::Expect {
            kind,
            token_name,
            sync,
            label,
        } => {
            b.kw("Expect");
            b.plain(" ");
            emit_token_kind_fallback(b, st, *kind, token_name);
            b.plain(" ");
            b.kw("sync");
            b.punct("=");
            emit_sync_set(b, st, *sync);
            if let Some(id) = label {
                b.plain(" ");
                b.kw("label");
                b.punct("=");
                emit_label(b, st, *id);
            }
        }
        Instr::PushRet(r) => {
            b.kw("PushRet");
            b.plain(" ");
            emit_state_ref(b, st, *r);
        }
    }
}

fn emit_tail(b: &mut SpanBuilder, tail: &Tail, st: &StateTable, indent: &str) {
    match tail {
        Tail::Jump(n) => {
            b.plain(indent);
            b.kw("Jump");
            b.plain(" ");
            emit_state_ref(b, st, *n);
            b.newline();
        }
        Tail::Ret => {
            b.plain(indent);
            b.kw("Ret");
            b.newline();
        }
        Tail::Star {
            first,
            body,
            cont,
            head,
        } => {
            b.plain(indent);
            b.kw("Star");
            b.plain(" ");
            emit_first_pool(b, st, *first);
            b.plain(" ");
            b.kw("head");
            b.punct("=");
            emit_state_ref(b, st, *head);
            b.plain(" ");
            match cont {
                Some(n) => {
                    b.kw("cont");
                    b.punct("=");
                    emit_state_ref(b, st, *n);
                }
                None => b.kw("tail"),
            }
            b.newline();
            let inner = format!("{}  ", indent);
            emit_body(b, body, st, &inner);
        }
        Tail::Opt { first, body, cont } => {
            b.plain(indent);
            b.kw("Opt");
            b.plain(" ");
            emit_first_pool(b, st, *first);
            b.plain(" ");
            match cont {
                Some(n) => {
                    b.kw("cont");
                    b.punct("=");
                    emit_state_ref(b, st, *n);
                }
                None => b.kw("tail"),
            }
            b.newline();
            let inner = format!("{}  ", indent);
            emit_body(b, body, st, &inner);
        }
        Tail::Dispatch { tree, sync, cont, .. } => {
            b.plain(indent);
            b.kw("Dispatch");
            b.plain(" ");
            b.kw("sync");
            b.punct("=");
            emit_sync_set(b, st, *sync);
            b.plain(" ");
            match cont {
                Some(n) => {
                    b.kw("cont");
                    b.punct("=");
                    emit_state_ref(b, st, *n);
                }
                None => b.kw("tail"),
            }
            b.newline();
            let inner = format!("{}  ", indent);
            emit_dispatch_tree(b, tree, st, &inner);
        }
    }
}

fn emit_body(b: &mut SpanBuilder, body: &Body, st: &StateTable, indent: &str) {
    for instr in &body.instrs {
        b.plain(indent);
        emit_instr(b, instr, st);
        b.newline();
    }
    emit_tail(b, &body.tail, st, indent);
}

fn emit_dispatch_tree(
    b: &mut SpanBuilder,
    tree: &DispatchTree,
    st: &StateTable,
    indent: &str,
) {
    match tree {
        DispatchTree::Leaf(l) => emit_dispatch_leaf(b, l, st, None, indent),
        DispatchTree::Switch {
            depth,
            arms,
            default,
        } => {
            b.plain(indent);
            b.kw("look");
            b.punct("(");
            b.num(depth.to_string());
            b.punct(")");
            b.punct(":");
            b.newline();
            let child_indent = format!("{}  ", indent);
            for (kind, sub) in arms {
                match sub {
                    DispatchTree::Leaf(l) => {
                        emit_dispatch_leaf(b, l, st, Some(*kind), &child_indent);
                    }
                    _ => {
                        b.plain(&child_indent);
                        emit_token_kind(b, st, *kind);
                        b.punct(":");
                        b.newline();
                        emit_dispatch_tree(b, sub, st, &format!("{}  ", child_indent));
                    }
                }
            }
            // `else` arm.
            b.plain(&child_indent);
            b.kw("else");
            b.plain(" ");
            emit_dispatch_leaf_body(b, default, st, &child_indent);
        }
    }
}

fn emit_dispatch_leaf(
    b: &mut SpanBuilder,
    leaf: &DispatchLeaf,
    st: &StateTable,
    arm_kind: Option<u16>,
    indent: &str,
) {
    b.plain(indent);
    if let Some(kind) = arm_kind {
        emit_token_kind(b, st, kind);
        b.plain(" ");
    }
    emit_dispatch_leaf_body(b, leaf, st, indent);
}

fn emit_dispatch_leaf_body(
    b: &mut SpanBuilder,
    leaf: &DispatchLeaf,
    st: &StateTable,
    indent: &str,
) {
    match leaf {
        DispatchLeaf::Arm(body) => {
            b.punct("->");
            b.newline();
            emit_body(b, body, st, &format!("{}  ", indent));
        }
        DispatchLeaf::Fallthrough => {
            b.punct("->");
            b.plain(" ");
            b.kw("fall");
            b.newline();
        }
        DispatchLeaf::Error => {
            b.punct("->");
            b.plain(" ");
            b.kw("error");
            b.newline();
        }
    }
}
