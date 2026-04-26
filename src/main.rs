use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use parsuna::grammar::ir::{CharClass, ClassItem};
use parsuna::lowering::lexer_dfa::{DfaState, START};
use parsuna::{
    analysis::{analyze, EOF_MARKER},
    codegen::{EmittedFile, GenerateTarget},
    grammar::parse_grammar,
    lowering::{self, StateTable},
    tree_sitter, Diagnostic, Expr, TokenPattern,
};

#[derive(Parser)]
#[command(
    name = "parsuna",
    version,
    about = "Parsuna — parser generator for .parsuna grammars",
    disable_help_subcommand = true
)]
struct Cli {
    /// Path to the .parsuna grammar file.
    grammar: PathBuf,

    /// Override the grammar identifier used for emitted file and package
    /// names. Defaults to the grammar file's stem.
    #[arg(long = "name", global = true)]
    name: Option<String>,

    /// How to treat warnings. `warn` prints them and continues; `error`
    /// promotes every warning to a hard error so the build fails.
    #[arg(long = "warnings", default_value = "warn", global = true)]
    warnings: WarningPolicy,

    #[command(subcommand)]
    op: Op,
}

/// What to do with warnings emitted by the analysis lints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WarningPolicy {
    /// Print warnings to stderr but exit 0 if there are no errors.
    Warn,
    /// Promote every warning to an error.
    Error,
}

#[derive(Subcommand)]
enum Op {
    /// Load, parse, and analyze the grammar; print a one-line summary.
    /// Exits non-zero on diagnostics — suitable as a CI gate.
    Check,
    /// Emit a parser for the given backend target.
    Generate(GenerateCmd),
    /// Emit a tree-sitter `grammar.js` for editor tooling.
    TreeSitter {
        /// Output directory for `grammar.js`. Defaults to `.`.
        #[arg(short = 'o', long = "out")]
        out: Option<PathBuf>,
    },
    /// Dump internal compiler state. Intended as a debugging aid while
    /// developing a grammar.
    #[command(subcommand)]
    Debug(DebugCmd),
}

/// `generate` subcommand: an output directory plus one per-backend
/// sub-subcommand carrying that target's `Args`.
#[derive(clap::Args)]
struct GenerateCmd {
    /// Output directory. Defaults to `.`.
    #[arg(short = 'o', long = "out", global = true)]
    out: Option<PathBuf>,
    #[command(subcommand)]
    target: GenerateTarget,
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Print counts of tokens, rules, and other top-level grammar stats.
    Stats,
    /// Dump every declared token with its resolved pattern.
    Tokens,
    /// Dump rule bodies, either as an indented tree or as a Graphviz
    /// digraph of railroad diagrams.
    Rules {
        #[arg(long, default_value = "tree")]
        format: RulesFormat,
    },
    /// Dump the LL(k) analysis — FIRST/FOLLOW sets and lookahead tables.
    Analysis,
    /// Dump the lowered state table that drives the generated parser.
    Lowering,
    /// Dump the lexer DFA — one entry per state, with transitions
    /// collapsed into byte ranges per target.
    Dfa {
        /// Output format: human-readable text or Graphviz dot.
        #[arg(long, default_value = "plain")]
        format: OutputFormat,
    },
}

/// Output format for tabular debug dumps.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum OutputFormat {
    /// Human-readable plain text.
    Plain,
    /// Graphviz `dot` source.
    Dot,
}

/// Output format for the `rules` debug dump.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum RulesFormat {
    /// Indented expression tree.
    Tree,
    /// Graphviz `dot` source rendering each rule as a railroad diagram.
    Dot,
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), ExitCode> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("error creating {}: {}", parent.display(), e);
                return Err(ExitCode::FAILURE);
            }
        }
    }
    std::fs::write(path, contents).map_err(|e| {
        eprintln!("error writing {}: {}", path.display(), e);
        ExitCode::FAILURE
    })?;
    println!("wrote {}", path.display());
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let grammar = cli.grammar.as_path();
    let name = cli.name.as_deref();
    let warnings = cli.warnings;
    match cli.op {
        Op::Check => cmd_check(grammar, name, warnings),
        Op::Generate(cmd) => cmd_generate(grammar, name, warnings, cmd),
        Op::TreeSitter { out } => cmd_tree_sitter(grammar, name, warnings, out),
        Op::Debug(sub) => cmd_debug(grammar, name, warnings, sub),
    }
}

fn cmd_debug(
    grammar: &Path,
    name: Option<&str>,
    warnings: WarningPolicy,
    cmd: DebugCmd,
) -> ExitCode {
    match cmd {
        DebugCmd::Stats => dbg_load_lowered(grammar, name, warnings, print_stats),
        DebugCmd::Tokens => dbg_load_analyzed(grammar, name, warnings, print_tokens),
        DebugCmd::Rules { format } => {
            dbg_load_analyzed(grammar, name, warnings, |ag| match format {
                RulesFormat::Tree => print_rules(ag),
                RulesFormat::Dot => print_rules_dot(ag),
            })
        }
        DebugCmd::Analysis => dbg_load_analyzed(grammar, name, warnings, print_analysis),
        DebugCmd::Lowering => dbg_load_lowered(grammar, name, warnings, print_lowering),
        DebugCmd::Dfa { format } => {
            dbg_load_lowered(grammar, name, warnings, move |_, st| match format {
                OutputFormat::Plain => print_dfa(st),
                OutputFormat::Dot => print_dfa_dot(st),
            })
        }
    }
}

fn dbg_load_analyzed(
    path: &Path,
    name: Option<&str>,
    warnings: WarningPolicy,
    f: impl FnOnce(&parsuna::AnalyzedGrammar),
) -> ExitCode {
    match load_and_analyze(path, name, warnings) {
        Ok(ag) => {
            f(&ag);
            ExitCode::SUCCESS
        }
        Err(c) => c,
    }
}

fn dbg_load_lowered(
    path: &Path,
    name: Option<&str>,
    warnings: WarningPolicy,
    f: impl FnOnce(&parsuna::AnalyzedGrammar, &StateTable),
) -> ExitCode {
    match load_and_analyze(path, name, warnings) {
        Ok(ag) => {
            let st = lowering::lower(&ag);
            f(&ag, &st);
            ExitCode::SUCCESS
        }
        Err(c) => c,
    }
}

fn print_stats(ag: &parsuna::AnalyzedGrammar, st: &StateTable) {
    let pub_tokens = ag
        .grammar
        .tokens
        .values()
        .filter(|t| !t.is_fragment)
        .count();
    let frag_tokens = ag.grammar.tokens.len() - pub_tokens;
    let pub_rules = ag.grammar.rules.values().filter(|r| !r.is_fragment).count();
    let frag_rules = ag.grammar.rules.len() - pub_rules;
    println!("grammar       : {}", ag.grammar.name);
    println!("tokens        : {} ({} fragments)", pub_tokens, frag_tokens);
    println!("rules         : {} ({} fragments)", pub_rules, frag_rules);
    println!("LL(k)         : {}", ag.k);
    println!("mark kinds    : {}", st.rule_kinds.len());
    println!("state table   : {} states", st.states.len());
    println!("FIRST pool    : {} interned", st.first_sets.len());
    println!("SYNC pool     : {} interned", st.sync_sets.len());
    println!(
        "lexer DFA     : {} states, start {}",
        st.lexer_dfa.len(),
        parsuna::lowering::lexer_dfa::START
    );
    println!("entry points  : {}", st.entry_states.len());
}

fn print_tokens(ag: &parsuna::AnalyzedGrammar) {
    let max_name = ag
        .grammar
        .tokens
        .values()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0);
    for t in ag.grammar.tokens.values() {
        let tag = match (t.skip, t.is_fragment) {
            (true, _) => " [skip]",
            (_, true) => " [fragment]",
            _ => "",
        };

        let resolved = resolve_pattern(&t.pattern, &ag.grammar);
        println!(
            "{:<w$} = {}{}",
            t.name,
            format_pattern(&resolved),
            tag,
            w = max_name
        );
    }
}

fn print_rules(ag: &parsuna::AnalyzedGrammar) {
    for (i, r) in ag.grammar.rules.values().enumerate() {
        if i > 0 {
            println!();
        }
        let tag = if r.is_fragment { " [fragment]" } else { "" };
        println!("{}{}", r.name, tag);

        print_expr_tree(&r.body, "", true);
    }
}

fn print_analysis(ag: &parsuna::AnalyzedGrammar) {
    println!("nullable:");
    for r in ag.grammar.rules.values() {
        let nn = ag.nullable.get(&r.name).copied().unwrap_or(false);
        println!("  {:<20} {}", r.name, if nn { "yes" } else { "no" });
    }
    println!();

    println!("FIRST (k={}):", ag.k);
    for r in ag.grammar.rules.values() {
        let empty = Default::default();
        let f = ag.first.get(&r.name).unwrap_or(&empty);
        println!("  {:<20} {}", r.name, format_first_set(f));
    }
    println!();

    println!("FOLLOW:");
    for r in ag.grammar.rules.values() {
        let mut names: Vec<&str> = ag
            .follow
            .get(&r.name)
            .map(|set| set.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        names.sort();
        let formatted: Vec<String> = names
            .iter()
            .map(|n| {
                if *n == EOF_MARKER {
                    "$".to_string()
                } else {
                    (*n).to_string()
                }
            })
            .collect();
        println!("  {:<20} {{{}}}", r.name, formatted.join(", "));
    }
}

fn print_lowering(_ag: &parsuna::AnalyzedGrammar, st: &StateTable) {
    println!("FIRST-set intern pool ({} entries):", st.first_sets.len());
    for i in 0..st.first_sets.len() {
        println!("  FIRST_{:<3} {}", i, format_first_pool(st, i as u32));
    }
    println!();

    println!("SYNC-set intern pool ({} entries):", st.sync_sets.len());
    for i in 0..st.sync_sets.len() {
        println!("  SYNC_{:<3}  {}", i, format_sync_set(st, i as u32));
    }
    println!();

    println!("State table ({} states):", st.states.len());

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
        println!("{:>w$}  {}{}", state.id, state.label, marker, w = id_width);
        for op in &state.ops {
            for line in format_op(op, st) {
                println!("{:>w$}      {}", "", line, w = id_width);
            }
        }
        println!();
    }
}

fn print_dfa(st: &StateTable) {
    let dfa: &[DfaState] = &st.lexer_dfa;

    println!("DFA: {} real states, start = {}", dfa.len(), START);
    println!();

    for state in dfa {
        let accept = match state.accept {
            Some(k) => format!("accept={}({})", k, token_name_for_kind(st, k)),
            None => "-".to_string(),
        };
        println!("state {:>3}  {}", state.id, accept);
        if !state.self_loop.is_empty() {
            let label: Vec<String> = state
                .self_loop
                .iter()
                .map(|&(lo, hi)| byte_range_label(lo, hi))
                .collect();
            println!("            self-loop  {{{}}}", label.join(", "));
        }
        if state.arms.is_empty() {
            println!("            (terminal — no live transitions)");
        } else {
            for arm in &state.arms {
                for &(from, to) in &arm.ranges {
                    let label = byte_range_label(from, to);
                    println!("            {:>18}  -> {}", label, arm.target);
                }
            }
        }
    }
}

fn format_pattern(p: &TokenPattern) -> String {
    match p {
        TokenPattern::Empty => "ε".to_string(),
        TokenPattern::Literal(s) => format!("{:?}", s),
        TokenPattern::Class(cc) => format_class(cc),
        TokenPattern::Ref(n) => n.clone(),
        TokenPattern::Seq(xs) => xs.iter().map(format_pattern).collect::<Vec<_>>().join(" "),
        TokenPattern::Alt(xs) => {
            let parts: Vec<String> = xs.iter().map(format_pattern).collect();
            format!("({})", parts.join(" | "))
        }
        TokenPattern::Opt(x) => format!("{}?", paren_if_composite(x)),
        TokenPattern::Star(x) => format!("{}*", paren_if_composite(x)),
        TokenPattern::Plus(x) => format!("{}+", paren_if_composite(x)),
    }
}

fn paren_if_composite(p: &TokenPattern) -> String {
    match p {
        TokenPattern::Seq(_) | TokenPattern::Alt(_) => format!("({})", format_pattern(p)),
        _ => format_pattern(p),
    }
}

fn format_class(cc: &CharClass) -> String {
    let mut items = String::new();
    for it in &cc.items {
        match *it {
            ClassItem::Char(c) => items.push_str(&format_char(c)),
            ClassItem::Range(lo, hi) => {
                items.push_str(&format_char(lo));
                items.push('-');
                items.push_str(&format_char(hi));
            }
        }
    }
    if cc.negated {
        format!("[^{}]", items)
    } else {
        format!("[{}]", items)
    }
}

fn format_char(cp: u32) -> String {
    match cp {
        0x09 => "\\t".into(),
        0x0A => "\\n".into(),
        0x0D => "\\r".into(),
        0x20 => "\\ ".into(),
        0x22 => "\\\"".into(),
        0x5C => "\\\\".into(),
        0x5D => "\\]".into(),
        0x2D => "\\-".into(),
        cp if (0x21..0x7F).contains(&cp) => (char::from_u32(cp).unwrap()).to_string(),
        _ => format!("\\u{:04X}", cp),
    }
}

fn print_expr_tree(e: &Expr, prefix: &str, is_last: bool) {
    let branch = if is_last { "└─ " } else { "├─ " };
    let child_pad = if is_last { "   " } else { "│  " };
    let label = match e {
        Expr::Empty => "ε".to_string(),
        Expr::Token(n) => format!("Token({})", n),
        Expr::Rule(n) => format!("Rule({})", n),
        Expr::Seq(_) => "Seq".to_string(),
        Expr::Alt(_) => "Alt".to_string(),
        Expr::Opt(_) => "Opt".to_string(),
        Expr::Star(_) => "Star".to_string(),
        Expr::Plus(_) => "Plus".to_string(),
    };
    println!("{}{}{}", prefix, branch, label);

    let next_prefix = format!("{}{}", prefix, child_pad);

    match e {
        Expr::Seq(xs) | Expr::Alt(xs) => {
            let n = xs.len();
            for (i, x) in xs.iter().enumerate() {
                print_expr_tree(x, &next_prefix, i + 1 == n);
            }
        }
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => {
            print_expr_tree(x, &next_prefix, true);
        }
        _ => {}
    }
}

fn format_first_set(f: &parsuna::analysis::FirstSet) -> String {
    let mut rows: Vec<String> = f
        .iter()
        .map(|seq| {
            if seq.is_empty() {
                "ε".to_string()
            } else {
                seq.join(" ")
            }
        })
        .collect();
    rows.sort();
    format!("{{{}}}", rows.join(", "))
}

fn format_op(op: &parsuna::lowering::Op, st: &StateTable) -> Vec<String> {
    use parsuna::lowering::Op;
    match op {
        Op::Enter(k) => vec![format!("Enter {}", rule_kind_name(st, *k))],
        Op::Exit(k) => vec![format!("Exit {}", rule_kind_name(st, *k))],
        Op::Expect {
            kind,
            token_name,
            sync,
        } => vec![format!(
            "Expect {} sync={}",
            token_name_for_kind_fallback(st, *kind, token_name),
            format_sync_set(st, *sync)
        )],
        Op::PushRet(r) => vec![format!("PushRet {}", state_ref(st, *r))],
        Op::Jump(n) => vec![format!("Jump {}", state_ref(st, *n))],
        Op::Ret => vec!["Ret".into()],
        Op::Star { first, body, next } => vec![format!(
            "Star {} body={} next={}",
            format_first_pool(st, *first),
            state_ref(st, *body),
            state_ref(st, *next)
        )],
        Op::Opt { first, body, next } => vec![format!(
            "Opt {} body={} next={}",
            format_first_pool(st, *first),
            state_ref(st, *body),
            state_ref(st, *next)
        )],
        Op::Dispatch { tree, sync, next } => {
            let mut lines = vec![format!(
                "Dispatch sync={} fall={}",
                format_sync_set(st, *sync),
                state_ref(st, *next)
            )];
            format_dispatch_tree(st, tree, "  ", &mut lines);
            lines
        }
    }
}

fn format_dispatch_tree(
    st: &StateTable,
    tree: &parsuna::lowering::DispatchTree,
    indent: &str,
    out: &mut Vec<String>,
) {
    use parsuna::lowering::{DispatchLeaf, DispatchTree};
    let leaf_str = |leaf: &DispatchLeaf| -> String {
        match leaf {
            DispatchLeaf::Arm(t) => format!("-> {}", state_ref(st, *t)),
            DispatchLeaf::Fallthrough => "-> fall".into(),
            DispatchLeaf::Error => "-> error".into(),
        }
    };
    match tree {
        DispatchTree::Leaf(l) => out.push(format!("{}{}", indent, leaf_str(l))),
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
                        out.push(format!("{}{} {}", child_indent, name, leaf_str(l)));
                    }
                    _ => {
                        out.push(format!("{}{}:", child_indent, name));
                        format_dispatch_tree(st, sub, &format!("{}  ", child_indent), out);
                    }
                }
            }
            out.push(format!("{}else {}", child_indent, leaf_str(default)));
        }
    }
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

fn byte_range_label(from: u8, to: u8) -> String {
    if from == to {
        display_byte(from)
    } else {
        format!("{}..{}", display_byte(from), display_byte(to))
    }
}

fn display_byte(b: u8) -> String {
    match b {
        0x09 => "\\t".into(),
        0x0A => "\\n".into(),
        0x0D => "\\r".into(),
        0x20 => "SP".into(),
        b if (0x21..0x7F).contains(&b) => format!("'{}'", b as char),
        _ => format!("0x{:02X}", b),
    }
}

fn print_dfa_dot(st: &StateTable) {
    let dfa: &[DfaState] = &st.lexer_dfa;
    println!("digraph lexer_dfa {{");
    println!("  rankdir=LR;");
    println!("  node [fontname=\"Menlo,Monaco,Consolas,monospace\"];");
    println!("  edge [fontname=\"Menlo,Monaco,Consolas,monospace\"];");

    println!("  _start [shape=point, width=0.12];");
    println!("  _start -> s{};", START);

    for state in dfa {
        let (shape, label) = match state.accept {
            Some(k) => (
                "doublecircle",
                format!("{}\\n{}", state.id, dot_escape(token_name_for_kind(st, k))),
            ),
            None => ("circle", state.id.to_string()),
        };
        println!("  s{} [shape={}, label=\"{}\"];", state.id, shape, label);
        for arm in &state.arms {
            for &(from, to) in &arm.ranges {
                let label = dot_escape(&byte_range_label(from, to));
                println!("  s{} -> s{} [label=\"{}\"];", state.id, arm.target, label);
            }
        }
    }
    println!("}}");
}

fn print_rules_dot(ag: &parsuna::AnalyzedGrammar) {
    println!("digraph grammar {{");
    println!("  rankdir=LR;");
    println!("  compound=true;");
    println!("  node [fontname=\"Menlo,Monaco,Consolas,monospace\"];");
    println!("  edge [fontname=\"Menlo,Monaco,Consolas,monospace\", arrowsize=0.6];");

    for (rule_idx, r) in ag.grammar.rules.values().enumerate() {
        let prefix = format!("r{}_", rule_idx);
        let cluster_label = if r.is_fragment {
            format!("{} (fragment)", r.name)
        } else {
            r.name.clone()
        };
        let style = if r.is_fragment {
            "rounded,dashed"
        } else {
            "rounded"
        };

        println!();
        println!("  subgraph cluster_{} {{", rule_idx);
        println!("    label=\"{}\";", dot_escape(&cluster_label));
        println!("    style=\"{}\";", style);
        println!("    labeljust=l;");
        println!("    // start / end terminals of the production");
        let start = format!("{}start", prefix);
        let end = format!("{}end", prefix);
        println!(
            "    {} [shape=circle, label=\"\", width=0.2, style=filled, fillcolor=black];",
            start
        );
        println!("    {} [shape=doublecircle, label=\"\", width=0.2];", end);

        let mut rr = RailroadCtx {
            prefix: &prefix,
            next_id: 0,
            out: String::new(),
        };
        let (entry, exit) = rr.emit(&r.body);
        print!("{}", rr.out);
        println!("    {} -> {};", start, entry);
        println!("    {} -> {};", exit, end);
        println!("  }}");
    }
    println!("}}");
}

struct RailroadCtx<'a> {
    prefix: &'a str,
    next_id: usize,
    out: String,
}

impl<'a> RailroadCtx<'a> {
    fn fresh(&mut self, kind: &str) -> String {
        let n = self.next_id;
        self.next_id += 1;
        format!("{}{}{}", self.prefix, kind, n)
    }

    fn anchor(&mut self) -> String {
        let n = self.fresh("a");
        writeln!(&mut self.out, "    {} [shape=point, width=0.05];", n).unwrap();
        n
    }

    fn token(&mut self, name: &str) -> String {
        let n = self.fresh("t");
        writeln!(
            &mut self.out,
            "    {} [shape=ellipse, label=\"{}\"];",
            n,
            dot_escape(name)
        )
        .unwrap();
        n
    }

    fn rule_ref(&mut self, name: &str) -> String {
        let n = self.fresh("r");
        writeln!(
            &mut self.out,
            "    {} [shape=box, style=rounded, label=\"{}\"];",
            n,
            dot_escape(name)
        )
        .unwrap();
        n
    }

    fn edge(&mut self, from: &str, to: &str) {
        writeln!(&mut self.out, "    {} -> {};", from, to).unwrap();
    }

    fn emit(&mut self, e: &Expr) -> (String, String) {
        match e {
            Expr::Empty => {
                let a = self.anchor();
                (a.clone(), a)
            }
            Expr::Token(name) => {
                let n = self.token(name);
                (n.clone(), n)
            }
            Expr::Rule(name) => {
                let n = self.rule_ref(name);
                (n.clone(), n)
            }
            Expr::Seq(xs) => {
                if xs.is_empty() {
                    return self.emit(&Expr::Empty);
                }
                let mut iter = xs.iter();
                let first = iter.next().unwrap();
                let (entry, mut prev_exit) = self.emit(first);
                for x in iter {
                    let (e1, e2) = self.emit(x);
                    self.edge(&prev_exit, &e1);
                    prev_exit = e2;
                }
                (entry, prev_exit)
            }
            Expr::Alt(xs) => {
                let br = self.anchor();
                let jn = self.anchor();
                for x in xs {
                    let (e1, e2) = self.emit(x);
                    self.edge(&br, &e1);
                    self.edge(&e2, &jn);
                }
                (br, jn)
            }
            Expr::Opt(x) => {
                let br = self.anchor();
                let jn = self.anchor();
                let (e1, e2) = self.emit(x);
                self.edge(&br, &e1);
                self.edge(&e2, &jn);
                self.edge(&br, &jn);
                (br, jn)
            }
            Expr::Star(x) => {
                let br = self.anchor();
                let jn = self.anchor();
                let (e1, e2) = self.emit(x);
                self.edge(&br, &jn);
                self.edge(&br, &e1);
                self.edge(&e2, &jn);
                self.edge(&jn, &br);
                (br, jn)
            }
            Expr::Plus(x) => {
                let br = self.anchor();
                let jn = self.anchor();
                let (e1, e2) = self.emit(x);
                self.edge(&br, &e1);
                self.edge(&e2, &jn);
                self.edge(&jn, &br);
                (br, jn)
            }
        }
    }
}

fn dot_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn resolve_pattern(p: &TokenPattern, g: &parsuna::Grammar) -> TokenPattern {
    match p {
        TokenPattern::Empty | TokenPattern::Literal(_) | TokenPattern::Class(_) => p.clone(),
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

fn cmd_tree_sitter(
    grammar: &Path,
    name: Option<&str>,
    warnings: WarningPolicy,
    out: Option<PathBuf>,
) -> ExitCode {
    let ag = match load_and_analyze(grammar, name, warnings) {
        Ok(ag) => ag,
        Err(code) => return code,
    };
    let contents = tree_sitter::emit(&ag);
    let path = out.unwrap_or_else(|| PathBuf::from(".")).join("grammar.js");
    match write_file(&path, contents.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn cmd_check(
    grammar_path: &Path,
    override_name: Option<&str>,
    warnings: WarningPolicy,
) -> ExitCode {
    let ag = match load_and_analyze(grammar_path, override_name, warnings) {
        Ok(ag) => ag,
        Err(code) => return code,
    };
    println!(
        "grammar `{}` OK: {} tokens, {} rules, LL({})",
        ag.grammar.name,
        ag.grammar.tokens.len(),
        ag.grammar.rules.len(),
        ag.k,
    );
    ExitCode::SUCCESS
}

fn cmd_generate(
    grammar: &Path,
    name: Option<&str>,
    warnings: WarningPolicy,
    cmd: GenerateCmd,
) -> ExitCode {
    let ag = match load_and_analyze(grammar, name, warnings) {
        Ok(ag) => ag,
        Err(code) => return code,
    };
    let st = lowering::lower(&ag);
    let files = cmd.target.emit(&st);

    let base = cmd.out.unwrap_or_else(|| PathBuf::from("."));
    for f in files {
        if let Err(code) = write_emitted(&base, &f) {
            return code;
        }
    }
    ExitCode::SUCCESS
}

fn write_emitted(base: &Path, f: &EmittedFile) -> Result<(), ExitCode> {
    write_file(&base.join(&f.path), f.contents.as_bytes())
}

fn load_and_analyze(
    path: &Path,
    override_name: Option<&str>,
    warnings_policy: WarningPolicy,
) -> Result<parsuna::AnalyzedGrammar, ExitCode> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        eprintln!("error: {}: {}", path.display(), e);
        ExitCode::FAILURE
    })?;
    let mut g = parse_grammar(&src).map_err(|errs| {
        for e in errs {
            eprintln!("{}", Diagnostic::from(e));
        }
        ExitCode::FAILURE
    })?;
    g.name = match override_name {
        Some(n) => n.to_string(),
        None => path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("parser")
            .to_string(),
    };
    let mut outcome = analyze(g);
    for d in &mut outcome.diagnostics {
        if !d.is_error() && warnings_policy == WarningPolicy::Error {
            d.severity = parsuna::Severity::Error;
        }
        eprintln!("{}", d);
    }
    match outcome.grammar {
        Some(ag) if !outcome.has_errors() => Ok(ag),
        _ => Err(ExitCode::FAILURE),
    }
}
