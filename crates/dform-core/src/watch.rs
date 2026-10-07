//! The sources a run reads from outside its program text (DESIGN.org
//! "Reactive inputs and controller mode"): the tables and documents it
//! loads (`crate::tables`) and the program's own files. Every run reads
//! them where they are now: `plan` and `apply` once, the controller
//! whenever a source's stamp changes (`stamp`), by polling. A `git` source
//! is stamped by the commit its ref names. A `.df` file of facts is a
//! module of the program (R-39): a change to it, as to any program file,
//! is an input event.

use crate::ast::Span;
use std::path::{Path, PathBuf};

/// Where a source is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Source {
    File(PathBuf),
    Git {
        repo: PathBuf,
        rev: String,
        path: String,
    },
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(p) => write!(f, "file {}", p.display()),
            Source::Git { repo, rev, path } => {
                write!(f, "git {} {rev}:{path}", repo.display())
            }
        }
    }
}

/// A source a run read, by the relation or module it is: a table's
/// `p`, a program file's module (`data.releases`).
#[derive(Debug, Clone)]
pub struct Relation {
    pub pred: String,
    pub arity: usize,
    pub source: Source,
    pub span: Span,
}

/// What changes when a source's facts may have: a digest of a file's
/// contents (a missing file stamps as missing), the commit a git ref names.
pub fn stamp(s: &Source) -> String {
    match s {
        Source::File(p) => match std::fs::read(p) {
            Ok(bytes) => digest(&bytes),
            Err(_) => "missing".into(),
        },
        Source::Git { repo, rev, .. } => {
            crate::git::resolve(repo, rev).unwrap_or_else(|_| "missing".into())
        }
    }
}

/// A file's stamp: a digest of its contents.
pub fn digest(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The program's files as sources: each by its module's name, its path
/// from the project root with dots (`data/releases.df` is
/// `data.releases`), so that the controller sees a change to one as an
/// input event.
pub fn program_sources(files: &[PathBuf]) -> Vec<Relation> {
    files
        .iter()
        .map(|f| {
            let root = crate::project::manifest_root(f);
            let rel = root
                .as_deref()
                .and_then(|r| f.strip_prefix(r).ok())
                .unwrap_or(f.file_name().map(Path::new).unwrap_or(f));
            let name = rel
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(".");
            Relation {
                pred: name,
                arity: 0,
                source: Source::File(f.clone()),
                span: Span::default(),
            }
        })
        .collect()
}
