//! Plain-text dumper for [`crate::lowering::StateTable`].
//!
//! Used by `parsuna debug lowering` and by the playground's IR tab.
//! Format: an interned `FIRST` pool, an interned `SYNC` pool, then one
//! block per state — `<id>  <label>` followed by indented `Enter` /
//! `Expect` / `Star` / `Dispatch` / etc. lines that mirror the
//! `Body → Tail` shape. The dump is line-based so a JS overlay can
//! count columns without parsing structure.

use std::fmt::Write;

use crate::lowering::{
    Body, DispatchLeaf, DispatchTree, Instr, ModeActionInfo, StateTable, Tail,
};

/// Render the full state table as a multi-line string.
pub fn lowering_text(st: &StateTable) -> String {
    let mut out = String::new();
    writeln!(out, "FIRST-set intern pool ({} entries):", st.first_sets.len()).unwrap();
    for i in 0..st.first_sets.len() {
        writeln!(out, "  FIRST_{:<3} {}", i, format_first_pool(st, i as u32)).unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "SYNC-set intern pool ({} entries):", st.sync_sets.len()).unwrap();
    for i in 0..st.sync_sets.len() {
        writeln!(out, "  SYNC_{:<3}  {}", i, format_sync_set(st, i as u32)).unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "State table ({} states):", st.states.len()).unwrap();

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
        let marker = entry_by_id
            .get(&state.id)
            .map(|n| format!("    ; entry: {}", n))
            .unwrap_or_default();
        writeln!(
            out,
            "{:>w$}  {}{}",
            state.id,
            state.label,
            marker,
            w = id_width
        )
        .unwrap();
        for instr in &state.body.instrs {
            for line in format_instr(instr, st) {
                writeln!(out, "{:>w$}      {}", "", line, w = id_width).unwrap();
            }
        }
        for line in format_tail(&state.body.tail, st) {
            writeln!(out, "{:>w$}      {}", "", line, w = id_width).unwrap();
        }
        writeln!(out).unwrap();
    }
    out
}

fn format_first_pool(st: &StateTable, id: u32) -> String {
    let Some(set) = st.first_sets.get(id as usize) else {
        return format!("FIRST_{}", id);
    };
    let mut parts: Vec<String> = set
        .seqs
        .iter()
        .map(|seq| {
            if seq.is_empty() {
                "ε".to_string()
            } else {
                seq.iter()
                    .map(|k| token_name_for_kind(st, *k).to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        })
        .collect();
    parts.sort();
    parts.dedup();
    format!("{{{}}}", parts.join(", "))
}

fn format_sync_set(st: &StateTable, id: u32) -> String {
    let Some(set) = st.sync_sets.get(id as usize) else {
        return format!("SYNC_{}", id);
    };
    let mut names: Vec<&str> = set
        .kinds
        .iter()
        .map(|k| token_name_for_kind(st, *k))
        .collect();
    names.sort();
    names.dedup();
    format!("{{{}}}", names.join(", "))
}

fn rule_kind_name(st: &StateTable, kind: u16) -> String {
    st.rule_kinds
        .get(kind as usize)
        .cloned()
        .unwrap_or_else(|| format!("?{}", kind))
}

fn state_ref(st: &StateTable, id: u32) -> String {
    match st.states.get(&id) {
        Some(s) if !s.label.is_empty() => s.label.clone(),
        _ => id.to_string(),
    }
}

fn token_name_for_kind(st: &StateTable, kind: u16) -> &str {
    if kind == parsuna_rt::TOKEN_EOF {
        return "EOF";
    }
    if kind == 0 {
        return "?";
    }
    st.tokens
        .get(kind as usize - 1)
        .map(|t| t.name.as_str())
        .unwrap_or("?")
}

fn label_name(st: &StateTable, id: u16) -> String {
    if id == 0 {
        return "?".to_string();
    }
    st.labels
        .get((id - 1) as usize)
        .cloned()
        .unwrap_or_else(|| format!("?{}", id))
}

fn token_name_for_kind_fallback<'a>(st: &'a StateTable, kind: u16, fallback: &'a str) -> &'a str {
    if kind == 0 {
        return fallback;
    }
    let i = kind as usize - 1;
    if i < st.tokens.len() {
        &st.tokens[i].name
    } else {
        fallback
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
    format!(" → {}", parts.join(", "))
}

fn format_instr(op: &Instr, st: &StateTable) -> Vec<String> {
    match op {
        Instr::Enter(k) => vec![format!("Enter {}", rule_kind_name(st, *k))],
        Instr::Exit(k) => vec![format!("Exit {}", rule_kind_name(st, *k))],
        Instr::Expect {
            kind,
            token_name,
            sync,
            label,
        } => vec![format!(
            "Expect {} sync={}{}",
            token_name_for_kind_fallback(st, *kind, token_name),
            format_sync_set(st, *sync),
            label
                .map(|id| format!(" label={}", label_name(st, id)))
                .unwrap_or_default()
        )],
        Instr::PushRet(r) => vec![format!("PushRet {}", state_ref(st, *r))],
    }
}

fn format_tail(tail: &Tail, st: &StateTable) -> Vec<String> {
    match tail {
        Tail::Jump(n) => vec![format!("Jump {}", state_ref(st, *n))],
        Tail::Ret => vec!["Ret".into()],
        Tail::Star {
            first,
            body,
            cont,
            head,
        } => {
            let mut lines = vec![format!(
                "Star {} head={} {}",
                format_first_pool(st, *first),
                state_ref(st, *head),
                match cont {
                    Some(n) => format!("cont={}", state_ref(st, *n)),
                    None => "tail".into(),
                },
            )];
            append_body_ops(st, body, "  ", &mut lines);
            lines
        }
        Tail::Opt { first, body, cont } => {
            let mut lines = vec![format!(
                "Opt {} {}",
                format_first_pool(st, *first),
                match cont {
                    Some(n) => format!("cont={}", state_ref(st, *n)),
                    None => "tail".into(),
                },
            )];
            append_body_ops(st, body, "  ", &mut lines);
            lines
        }
        Tail::Dispatch { tree, sync, cont } => {
            let mut lines = vec![format!(
                "Dispatch sync={} {}",
                format_sync_set(st, *sync),
                match cont {
                    Some(n) => format!("cont={}", state_ref(st, *n)),
                    None => "tail".into(),
                },
            )];
            format_dispatch_tree(st, tree, "  ", &mut lines);
            lines
        }
    }
}

fn append_body_ops(st: &StateTable, body: &Body, indent: &str, out: &mut Vec<String>) {
    for instr in &body.instrs {
        for line in format_instr(instr, st) {
            out.push(format!("{}{}", indent, line));
        }
    }
    for line in format_tail(&body.tail, st) {
        out.push(format!("{}{}", indent, line));
    }
}

fn format_dispatch_tree(
    st: &StateTable,
    tree: &DispatchTree,
    indent: &str,
    out: &mut Vec<String>,
) {
    match tree {
        DispatchTree::Leaf(l) => emit_dispatch_leaf(st, l, "", indent, out),
        DispatchTree::Switch {
            depth,
            arms,
            default,
        } => {
            out.push(format!("{}look({}):", indent, depth));
            let child_indent = format!("{}  ", indent);
            for (kind, sub) in arms {
                let name = token_name_for_kind(st, *kind);
                match sub {
                    DispatchTree::Leaf(l) => {
                        emit_dispatch_leaf(st, l, &format!("{} ", name), &child_indent, out);
                    }
                    _ => {
                        out.push(format!("{}{}:", child_indent, name));
                        format_dispatch_tree(st, sub, &format!("{}  ", child_indent), out);
                    }
                }
            }
            emit_dispatch_leaf(st, default, "else ", &child_indent, out);
        }
    }
}

fn emit_dispatch_leaf(
    st: &StateTable,
    leaf: &DispatchLeaf,
    prefix: &str,
    indent: &str,
    out: &mut Vec<String>,
) {
    match leaf {
        DispatchLeaf::Arm(body) => {
            out.push(format!("{}{}->", indent, prefix));
            append_body_ops(st, body, &format!("{}  ", indent), out);
        }
        DispatchLeaf::Fallthrough => {
            out.push(format!("{}{}-> fall", indent, prefix));
        }
        DispatchLeaf::Error => {
            out.push(format!("{}{}-> error", indent, prefix));
        }
    }
}
