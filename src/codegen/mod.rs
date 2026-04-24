//! Target-language code generators.
//!
//! Each backend consumes a [`crate::lowering::StateTable`] and returns one
//! or more [`EmittedFile`]s ready to be written to disk. The set of
//! available backends is listed in [`BACKENDS`]; [`find`] resolves a
//! case-insensitive name and [`emit`] is a convenience that runs
//! [`crate::lowering::lower`] and then the backend.

use std::path::PathBuf;

use crate::analysis::AnalyzedGrammar;
use crate::lowering::{self, StateTable};

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

/// Signature every backend implements: turn a state table into a list of
/// files. Backends are pure — they never touch the filesystem themselves.
pub type EmitFn = fn(&StateTable) -> Vec<EmittedFile>;

/// A named target language plus its emit function. Appears in [`BACKENDS`]
/// and is what the CLI hands to [`emit`].
pub struct Backend {
    /// Target name used by the CLI (`rust`, `python`, …). Always
    /// lowercase ASCII.
    pub name: &'static str,
    /// Emit function implementing this target.
    pub emit: EmitFn,
}

/// Every registered backend, in the order the CLI lists them. Order is not
/// semantically meaningful but is kept stable for predictable help output.
pub const BACKENDS: &[Backend] = &[
    Backend {
        name: "rust",
        emit: rust::emit,
    },
    Backend {
        name: "python",
        emit: python::emit,
    },
    Backend {
        name: "typescript",
        emit: typescript::emit,
    },
    Backend {
        name: "go",
        emit: go::emit,
    },
    Backend {
        name: "java",
        emit: java::emit,
    },
    Backend {
        name: "csharp",
        emit: csharp::emit,
    },
    Backend {
        name: "c",
        emit: c::emit,
    },
];

/// Look up a backend by case-insensitive name. Returns `None` if no such
/// target exists.
pub fn find(name: &str) -> Option<&'static Backend> {
    let n = name.to_ascii_lowercase();
    BACKENDS.iter().find(|b| b.name == n)
}

/// Lower an analyzed grammar and emit files for the given backend.
pub fn emit(backend: &Backend, ag: &AnalyzedGrammar) -> Vec<EmittedFile> {
    let st = lowering::lower(ag);
    (backend.emit)(&st)
}
