//! Target-language code generators.
//!
//! Each backend is a sibling submodule exposing a `pub struct Args` and a
//! `pub fn emit(st: &StateTable, args: &Args) -> Vec<EmittedFile>`. There
//! is no central registry — the CLI dispatches per target via a clap
//! subcommand and calls the chosen backend directly. Backends are pure;
//! they never touch the filesystem.

use std::path::PathBuf;

use crate::lowering::StateTable;

pub mod c;
pub mod common;
pub mod csharp;
pub mod go;
pub mod java;
pub mod python;
pub mod rust;
pub mod typescript;

/// One file ready to be written by the CLI: the path (relative to the
/// output directory) and the contents as a UTF-8 string.
#[derive(Clone, Debug)]
pub struct EmittedFile {
    /// Path relative to the caller-supplied output directory.
    pub path: PathBuf,
    /// Complete UTF-8 file contents.
    pub contents: String,
}

/// One subcommand per backend. The wrapped `Args` struct carries the
/// target-specific flags (e.g. `java --package com.example.foo`); the
/// `Args` types derive [`clap::Args`] so the CLI gets per-target options
/// for free without re-declaring them at the dispatch site.
#[derive(clap::Subcommand)]
pub enum GenerateTarget {
    /// Rust crate source — `<grammar>.rs`. Plug into a `parsuna-rt` crate dep.
    Rust(rust::Args),
    /// Python package built around a generated Rust+pyo3 extension.
    Python(python::Args),
    /// TypeScript module — `<grammar>.ts`. Requires the `parsuna-rt` npm package.
    Typescript(typescript::Args),
    /// Go package — single file. Requires the `parsuna.dev/parsuna-rt-go` module.
    Go(go::Args),
    /// Java class — `<package>/Grammar.java`. Requires `dev.parsuna:parsuna-rt`.
    Java(java::Args),
    /// C# class — `<namespace>/Grammar.cs`. Requires the `Parsuna.Runtime` library.
    Csharp(csharp::Args),
    /// C header + implementation — `<grammar>.h` and `<grammar>.c`. Self-contained.
    C(c::Args),
}

impl GenerateTarget {
    /// Run the chosen backend's `emit` function on `st` and return its
    /// files. Pure dispatch — backends are responsible for their own
    /// output shape.
    pub fn emit(&self, st: &StateTable) -> Vec<EmittedFile> {
        match self {
            GenerateTarget::Rust(args) => rust::emit(st, args),
            GenerateTarget::Python(args) => python::emit(st, args),
            GenerateTarget::Typescript(args) => typescript::emit(st, args),
            GenerateTarget::Go(args) => go::emit(st, args),
            GenerateTarget::Java(args) => java::emit(st, args),
            GenerateTarget::Csharp(args) => csharp::emit(st, args),
            GenerateTarget::C(args) => c::emit(st, args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{analyze, AnalyzedGrammar};
    use crate::grammar::parse_grammar;
    use crate::lowering::{self, StateTable};

    fn analyze_minimal() -> AnalyzedGrammar {
        let g = parse_grammar("T = \"t\"; main = T;").expect("parse");
        let outcome = analyze(g);
        outcome.grammar.expect("grammar")
    }

    fn lowered_minimal() -> StateTable {
        lowering::lower(&analyze_minimal())
    }

    fn assert_non_empty(target: &str, files: Vec<EmittedFile>) {
        assert!(!files.is_empty(), "backend `{}` emitted no files", target);
        for f in &files {
            assert!(
                !f.contents.is_empty(),
                "backend `{}` emitted empty file {:?}",
                target,
                f.path
            );
        }
    }

    #[test]
    fn rust_backend_emits_file() {
        let st = lowered_minimal();
        assert_non_empty("rust", rust::emit(&st, &rust::Args::default()));
    }

    #[test]
    fn python_backend_emits_files() {
        let st = lowered_minimal();
        assert_non_empty("python", python::emit(&st, &python::Args::default()));
    }

    #[test]
    fn typescript_backend_emits_file() {
        let st = lowered_minimal();
        assert_non_empty(
            "typescript",
            typescript::emit(&st, &typescript::Args::default()),
        );
    }

    #[test]
    fn go_backend_emits_file() {
        let st = lowered_minimal();
        assert_non_empty("go", go::emit(&st, &go::Args::default()));
    }

    #[test]
    fn java_backend_emits_file() {
        let st = lowered_minimal();
        assert_non_empty("java", java::emit(&st, &java::Args::default()));
    }

    #[test]
    fn java_package_arg_drives_package_declaration_and_path() {
        let st = lowered_minimal();
        let args = java::Args {
            package: Some("com.example.foo".into()),
        };
        let files = java::emit(&st, &args);
        let f = &files[0];
        assert_eq!(
            f.path,
            std::path::PathBuf::from("com")
                .join("example")
                .join("foo")
                .join("Grammar.java")
        );
        assert!(f.contents.contains("package com.example.foo;"));
    }

    #[test]
    fn csharp_backend_emits_file() {
        let st = lowered_minimal();
        assert_non_empty("csharp", csharp::emit(&st, &csharp::Args::default()));
    }

    #[test]
    fn csharp_namespace_arg_drives_namespace_declaration() {
        let st = lowered_minimal();
        let args = csharp::Args {
            namespace: Some("MyApp.Parser".into()),
        };
        let files = csharp::emit(&st, &args);
        let f = &files[0];
        assert!(f.contents.contains("namespace MyApp.Parser;"));
        assert!(f.path.starts_with("MyApp.Parser"));
    }

    #[test]
    fn c_backend_emits_files() {
        let st = lowered_minimal();
        assert_non_empty("c", c::emit(&st, &c::Args::default()));
    }

    #[test]
    fn rust_backend_mentions_grammar_name_or_entry_point() {
        let st = lowered_minimal();
        let files = rust::emit(&st, &rust::Args::default());
        let combined: String = files.iter().map(|f| f.contents.as_str()).collect();
        assert!(combined.contains(&st.grammar_name) || combined.contains("parse_main"));
    }
}
