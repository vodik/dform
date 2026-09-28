use crate::ast::{Program, Stmt};
use crate::parser;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn load_program(entry_files: &[PathBuf]) -> Result<Program> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut statements = Vec::new();
    for f in entry_files {
        let abs = absolutize(f)?;
        let p = load_file(&abs, &mut seen)?;
        statements.extend(p.statements);
    }
    Ok(Program { statements })
}

fn load_file(path: &Path, seen: &mut BTreeSet<PathBuf>) -> Result<Program> {
    // One file reached two ways (`lib/x.df`, `lib/../lib/x.df`, a symlink)
    // is one file: dedup by its canonical path.
    let abs = absolutize(path)?;
    let abs = fs::canonicalize(&abs).unwrap_or(abs);
    if !seen.insert(abs.clone()) {
        // already loaded
        return Ok(Program { statements: vec![] });
    }
    let src = fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;
    let mut prog = parser::parse_file(&display_name(&abs), &src)?;

    let base_dir = abs.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for s in prog.statements.drain(..) {
        match s {
            Stmt::Import(i) => {
                let import_path = base_dir.join(&i.path);
                out.extend(load_file(&import_path, seen)?.statements);
            }
            other => out.push(other),
        }
    }
    Ok(Program { statements: out })
}

/// Predicates the provider or the CLI injects as facts (discovery, world,
/// schema, inputs). They are defined even when a run has no rows for them.
pub const PROVIDER_PREDS: &[&str] = &[
    "input",
    "data",
    "stack_output",
    "cloud_exists",
    "cloud_attr",
    "cloud_computed",
    "world_attr",
    "identity",
    "deformation",
    "world_digest",
    "may_derive",
    "drift",
    "type_attr",
    "type_list_key",
    "type_provider",
    "type_retry",
    "type_replace",
    "capability",
    "tag_path",
];

pub fn is_provider_pred(pred: &str) -> bool {
    PROVIDER_PREDS.contains(&pred)
}

/// Predicates the evaluator itself derives (the prelude and Rule 2): the
/// round-0 resolution and the stuck instances.
pub const ENGINE_PREDS: &[&str] = &["resolve", "resolved", "stuck", "__ref_dep"];

pub fn is_engine_pred(pred: &str) -> bool {
    ENGINE_PREDS.contains(&pred)
}

/// Compiler-owned predicates: never private to a module, and defined
/// whether or not the program writes them.
pub fn is_core_pred(pred: &str) -> bool {
    matches!(
        pred,
        "want"
            | "arg"
            | "arg_add"
            | "adopt"
            | "input"
            | "data"
            | "setting"
            | "setting_add"
            | "output"
            | "merge_rule"
            | "warn"
            | "deny"
            | "cloud_exists"
            | "cloud_attr"
            | "cloud_computed"
            | "member"
            | "env"
            | "has_env"
            | "attr"
            | "attr_conflict"
            | "attr_stuck"
            | "type_lattice"
            | "type_mint"
            | "ignore_changes"
            | "lifecycle"
            | "moved"
            | "allow_stuck"
    ) || is_engine_pred(pred)
        || is_provider_pred(pred)
}

/// How diagnostics name a file: relative to the working directory when it
/// is under it.
fn display_name(abs: &Path) -> String {
    let rel = std::env::current_dir()
        .ok()
        .and_then(|cwd| abs.strip_prefix(cwd).ok().map(Path::to_path_buf));
    let mut out = PathBuf::new();
    for c in rel.unwrap_or_else(|| abs.to_path_buf()).components() {
        match c {
            std::path::Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out.display().to_string()
}

fn absolutize(path: impl AsRef<Path>) -> Result<PathBuf> {
    let p = path.as_ref();
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().context("current_dir")?;
    Ok(cwd.join(p))
}
