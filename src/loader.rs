use crate::ast::Program;
use crate::diag;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A file as last parsed: its name and text, the tree, and the source it
/// registered (pinned while the entry lives). A long-running controller
/// loads the program once per event and parses a file again only when its
/// text changed (compared by text, not mtime: a rewrite within the clock's
/// granularity is still seen).
struct Parsed {
    name: String,
    text: String,
    green: rowan::GreenNode,
    file: u32,
    sources: Vec<u32>,
}

static PARSED: Mutex<BTreeMap<PathBuf, Parsed>> = Mutex::new(BTreeMap::new());

/// Every file of the program is parsed (imports followed, a file reached
/// twice loaded once), then the whole program is resolved at once: a name
/// declared in one file is used in another. Each import is inlined where
/// it stands.
pub fn load_program(entry_files: &[PathBuf]) -> Result<Program> {
    let mut units = Vec::new();
    let mut index: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut entries = Vec::new();
    for f in entry_files {
        let abs = absolutize(f)?;
        if let Some(i) = load_unit(&abs, &mut units, &mut index)? {
            entries.push(i);
        }
    }
    crate::syntax::resolve::lower(
        &units,
        &entries,
        true,
        crate::syntax::resolve::Mode::Program,
    )
    .map_err(|d| diag::Diagnostics(d).into())
}

fn load_unit(
    path: &Path,
    units: &mut Vec<crate::syntax::resolve::Unit>,
    index: &mut BTreeMap<PathBuf, usize>,
) -> Result<Option<usize>> {
    let abs = absolutize(path)?;
    let abs = fs::canonicalize(&abs).unwrap_or(abs);
    if index.contains_key(&abs) {
        return Ok(None);
    }
    let text = fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;
    let name = display_name(&abs);
    let (green, file) = {
        let mut cache = PARSED.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(&abs) {
            Some(p) if p.name == name && p.text == text => (p.green.clone(), p.file),
            _ => {
                let parse = crate::syntax::parser::parse(&text);
                if !parse.errors.is_empty() {
                    return Err(crate::parser::syntax_diagnostics(&name, &text, &parse).into());
                }
                let mark = diag::mark();
                let file = diag::add_source(&name, &text);
                let sources = diag::pin_since(mark);
                if let Some(old) = cache.insert(
                    abs.clone(),
                    Parsed {
                        name: name.clone(),
                        text: text.clone(),
                        green: parse.green.clone(),
                        file,
                        sources,
                    },
                ) {
                    diag::remove(&old.sources);
                }
                (parse.green, file)
            }
        }
    };
    let root = crate::syntax::SyntaxNode::new_root(green);
    let i = units.len();
    index.insert(abs.clone(), i);
    units.push(crate::syntax::resolve::Unit {
        file,
        root: root.clone(),
        imports: None,
    });
    let base_dir = abs.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut imports = Vec::new();
    for n in root
        .children()
        .filter(|n| n.kind() == crate::syntax::SyntaxKind::IMPORT)
    {
        let Some(t) = n
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == crate::syntax::SyntaxKind::STRING)
        else {
            continue;
        };
        let rel = crate::syntax::resolve::unescape(t.text()).unwrap_or_default();
        imports.push(load_unit(&base_dir.join(rel), units, index)?);
    }
    units[i].imports = Some(imports);
    Ok(Some(i))
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
    use crate::ast::Stmt;
    use std::collections::BTreeSet;

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
        fs::write(&f, "edition 2026\np(1)\n").unwrap();
        let a = load_program(std::slice::from_ref(&f)).unwrap();
        let b = load_program(std::slice::from_ref(&f)).unwrap();
        assert_eq!(file_ids(&a), file_ids(&b));
        let old = *file_ids(&a).first().unwrap();

        fs::write(&f, "edition 2026\np(2)\n").unwrap();
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
