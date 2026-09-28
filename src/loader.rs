use crate::ast::{Program, Stmt};
use crate::diag;
use crate::parser;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// A file as last parsed: its name and text, the parse, and the sources
/// the parse registered (pinned while the entry lives).
struct Parsed {
    name: String,
    text: String,
    program: Program,
    sources: Vec<u32>,
}

/// Every file loaded, by canonical path: a long-running controller loads
/// the program once per event and parses a file again only when its text
/// changed. (Compared by text, not mtime: a rewrite within the clock's
/// granularity is still seen.)
static PARSED: Mutex<BTreeMap<PathBuf, Parsed>> = Mutex::new(BTreeMap::new());

fn parse_cached(abs: &Path, name: &str, text: String) -> Result<Program> {
    let mut cache = PARSED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = cache.get(abs)
        && p.name == name
        && p.text == text
    {
        return Ok(p.program.clone());
    }
    let mark = diag::mark();
    let program = parser::parse_file(name, &text)?;
    let sources = diag::pin_since(mark);
    if let Some(old) = cache.insert(
        abs.to_path_buf(),
        Parsed {
            name: name.to_string(),
            text,
            program: program.clone(),
            sources,
        },
    ) {
        diag::remove(&old.sources);
    }
    Ok(program)
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
    let mut prog = parse_cached(&abs, &display_name(&abs), src)?;

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
            | "declassified"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;

    fn file_ids(p: &Program) -> BTreeSet<u32> {
        p.statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Fact(a) => Some(a.span.file),
                _ => None,
            })
            .collect()
    }

    /// A file is parsed once while its text stays the same; a changed
    /// file is parsed again and its old source leaves the registry.
    #[test]
    fn a_file_is_parsed_again_only_when_it_changed() {
        let dir = std::env::temp_dir().join(format!("dform-loader-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.df");
        fs::write(&f, "edition 2026.\np(1).\n").unwrap();
        let a = load_program(std::slice::from_ref(&f)).unwrap();
        let b = load_program(std::slice::from_ref(&f)).unwrap();
        assert_eq!(file_ids(&a), file_ids(&b));
        let old = *file_ids(&a).first().unwrap();

        fs::write(&f, "edition 2026.\np(2).\n").unwrap();
        let c = load_program(std::slice::from_ref(&f)).unwrap();
        assert_ne!(file_ids(&c), file_ids(&a));
        let span = |p: &Program| match &p.statements[0] {
            Stmt::Fact(a) => a.span,
            _ => unreachable!(),
        };
        assert_eq!(
            diag::at(span(&c)).map(|s| s.ends_with("p.df:2:1")),
            Some(true)
        );
        assert!(
            diag::at(Span {
                file: old,
                ..span(&c)
            })
            .is_none()
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
