//! Every identifier of every example, and of a project shaped as a real
//! one (modules used by their paths, a stack another stack uses, a
//! component's copies, an object input, std functions, two resources of
//! one name), walked through `dform lsp` (R-78):
//!
//! - no request errors or stops the server at a name, and inside any
//!   other token (whitespace, a comment, a literal, punctuation) none
//!   errors, a rename's preparation included;
//! - a name's definition lands on a declaration of it, and the references
//!   of that declaration include the place walked from;
//! - what has no definition is a variable, an attribute path, a key, a
//!   provider's name, a builtin type or relation, or a word of the syntax.

mod common;
mod lsp_client;

use dform_core::names::{Decls, Parsed, Symbol, What};
use dform_core::syntax::SyntaxKind;
use lsp_client::{Client, example, modules_project, uri};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn df_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    es.sort_by_key(|e| e.path());
    for e in es {
        let p = e.path();
        if p.is_dir() {
            df_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "df") {
            out.push(p);
        }
    }
}

/// A request at a position, its result or its error.
fn at(
    c: &mut Client,
    method: &str,
    file: &Path,
    (line, character): (u32, u32),
) -> Result<Value, Value> {
    let mut params = json!({
        "textDocument": { "uri": uri(file) },
        "position": { "line": line, "character": character },
    });
    if method == "textDocument/references" {
        params["context"] = json!({ "includeDeclaration": true });
    }
    c.try_request(method, params)
}

/// The byte of a position in an ASCII text.
fn offset(text: &str, (line, character): (u32, u32)) -> usize {
    let start: usize = text
        .split_inclusive('\n')
        .take(line as usize)
        .map(str::len)
        .sum();
    (start + character as usize).min(text.len())
}

/// A location's file, start and the text it covers.
fn target(loc: &Value) -> (PathBuf, (u32, u32), String) {
    let u = loc["uri"].as_str().unwrap();
    let path = PathBuf::from(u.strip_prefix("file://").unwrap());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{u}: {e}"));
    let pos = |p: &Value| {
        (
            p["line"].as_u64().unwrap() as u32,
            p["character"].as_u64().unwrap() as u32,
        )
    };
    let (s, e) = (pos(&loc["range"]["start"]), pos(&loc["range"]["end"]));
    let covered = text[offset(&text, s)..offset(&text, e)].to_string();
    (path, s, covered)
}

/// Words of the syntax that lex as names: none declares them.
const WORDS: &[&str] = &["check", "from", "mixed", "world", "as"];

/// Whether a schema file declares `typ`: a built-in one or the project's.
fn declared_type(project: &Path, typ: &str) -> bool {
    let mut texts: Vec<String> = dform_core::schema::BUILTINS
        .iter()
        .filter_map(|n| dform_core::schema::builtin(n))
        .map(str::to_string)
        .collect();
    let mut schemas = Vec::new();
    if project.join("providers").is_dir() {
        df_files(&project.join("providers"), &mut schemas);
    }
    texts.extend(
        schemas
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok()),
    );
    texts
        .iter()
        .any(|t| t.contains(&format!("({typ},")) || t.contains(&format!("(\"{typ}\",")))
}

/// Walk the project at `root`: every name's definition and references,
/// and with `every_token` its hover and signature help and every other
/// token's requests too. The problems found.
fn walk(name: &str, root: &Path, every_token: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let mut c = Client::start(root, json!({}));
    let mut files = Vec::new();
    df_files(root, &mut files);
    files.retain(|f| !f.starts_with(root.join("providers")));
    let parsed: Vec<Parsed> = files
        .iter()
        .map(|f| Parsed::new(f.clone(), std::fs::read_to_string(f).unwrap()))
        .collect();
    let d = Decls::of_files(root, &parsed);
    let mut refs_of: BTreeMap<(PathBuf, (u32, u32)), Vec<Value>> = BTreeMap::new();
    for f in &parsed {
        let rel = f.path.strip_prefix(root).unwrap().display().to_string();
        let place = |at: usize, text: &str| {
            let p = dform_lsp::text::position(&f.text, at);
            (
                (p.line, p.character),
                format!("{name}/{rel}:{}:{} `{text}`", p.line + 1, p.character + 1),
            )
        };
        // Inside every other token: at its start a cursor is also at the
        // end of the word before it, and names that word.
        for t in f
            .tree
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
        {
            let len: usize = t.text_range().len().into();
            let abuts = t.prev_token().is_some_and(|x| x.kind().is_word());
            if !every_token || t.kind().is_word() || (len < 2 && abuts) {
                continue;
            }
            let start: usize = t.text_range().start().into();
            let (pos, here) = place(start + usize::from(len >= 2), t.text());
            for m in [
                "textDocument/definition",
                "textDocument/references",
                "textDocument/hover",
                "textDocument/signatureHelp",
                "textDocument/prepareRename",
            ] {
                if let Err(e) = at(&mut c, m, &f.path, pos) {
                    problems.push(format!("{here}: {m}: {e}"));
                }
            }
        }
        // Every name, those of interpolation holes too.
        for n in d.names(f) {
            let t = &n.token;
            let (pos, here) = place(n.range.start().into(), t.text());
            let others: &[&str] = if every_token {
                &["textDocument/hover", "textDocument/signatureHelp"]
            } else {
                &[]
            };
            for m in others {
                if let Err(e) = at(&mut c, m, &f.path, pos) {
                    problems.push(format!("{here}: {m}: {e}"));
                }
            }
            let what = n.what.clone();
            let def = match at(&mut c, "textDocument/definition", &f.path, pos) {
                Ok(v) => v.as_array().cloned().unwrap_or_default(),
                Err(e) => {
                    problems.push(format!("{here}: definition: {e}"));
                    continue;
                }
            };
            if def.is_empty() {
                let fine = match &what {
                    What::Variable | What::Path | What::Key | What::Provider => true,
                    What::Type(typ) => !declared_type(root, typ),
                    What::Name(Symbol::Predicate(_, p), _) => {
                        dform_core::names::is_builtin_relation(p)
                    }
                    // A form of the language (`cloud_ref`, R-155): the
                    // lowering's, declared nowhere a program reads.
                    What::Name(Symbol::Function(f), _) => {
                        dform_core::functions::FORMS.contains(&f.as_str())
                    }
                    // A word, or a directory in a path.
                    What::Other => {
                        WORDS.contains(&t.text())
                            || t.parent().is_some_and(|p| {
                                matches!(p.kind(), SyntaxKind::USE | SyntaxKind::RESOURCE)
                            })
                    }
                    _ => false,
                };
                if !fine {
                    problems.push(format!("{here}: no definition for {what:?}"));
                }
                continue;
            }
            let named = matches!(what, What::Name(..) | What::Names(_));
            for loc in &def {
                let (file, start, covered) = target(loc);
                let whole_file = start == (0, 0) && covered.is_empty();
                if !whole_file && !covered.split(['.', '(', ' ']).any(|s| s == t.text()) {
                    problems.push(format!(
                        "{here}: definition {}:{}:{} covers `{covered}`, not the name",
                        file.display(),
                        start.0 + 1,
                        start.1 + 1
                    ));
                }
                // From a declaration in the project, its references reach
                // back here.
                if !named || whole_file || !file.starts_with(root) {
                    continue;
                }
                let found = refs_of.entry((file.clone(), start)).or_insert_with(|| {
                    at(&mut c, "textDocument/references", &file, start)
                        .ok()
                        .and_then(|v| v.as_array().cloned())
                        .unwrap_or_default()
                });
                let back = found.iter().any(|l| {
                    l["uri"] == json!(uri(&f.path))
                        && l["range"]["start"]["line"] == json!(pos.0)
                        && l["range"]["start"]["character"] == json!(pos.1)
                });
                if !back {
                    problems.push(format!(
                        "{here}: the references of its definition {}:{} miss it",
                        file.strip_prefix(root).unwrap_or(&file).display(),
                        start.0 + 1
                    ));
                }
            }
        }
    }
    c.shutdown();
    problems
}

/// The examples walked, one test each (they run in parallel); a test
/// checks the list is every example.
const EXAMPLES: &[&str] = &[
    "adopt",
    "advanced",
    "approvals",
    "aws",
    "bootstrap",
    "crud-api",
    "decl",
    "demo",
    "gke",
    "k8s",
    "pngu",
    "refine",
    "tour",
];

fn walk_example(name: &str) {
    assert!(EXAMPLES.contains(&name));
    let (_s, root) = example(name);
    let problems = walk(name, &root, name == "demo");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

macro_rules! walks {
    ($($test:ident: $name:literal,)*) => {
        $(
            #[test]
            fn $test() {
                walk_example($name);
            }
        )*
    };
}

walks! {
    walk_adopt: "adopt",
    walk_advanced: "advanced",
    walk_approvals: "approvals",
    walk_aws: "aws",
    walk_bootstrap: "bootstrap",
    walk_crud_api: "crud-api",
    walk_decl: "decl",
    walk_demo: "demo",
    walk_gke: "gke",
    walk_k8s: "k8s",
    walk_pngu: "pngu",
    walk_refine: "refine",
    walk_tour: "tour",
}

#[test]
fn the_walk_is_of_every_example() {
    let mut names: Vec<String> = std::fs::read_dir(common::repo().join("examples"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("dform.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, EXAMPLES);
}

#[test]
fn walk_modules() {
    let (_s, root) = modules_project();
    let problems = walk("modules", &root, true);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The walk over any project: `DFORM_WALK=DIR cargo test --test lsp_walk
/// -- --ignored` (a copy of DIR is walked).
#[test]
#[ignore]
fn walk_a_project() {
    let Some(dir) = std::env::var_os("DFORM_WALK") else {
        return;
    };
    let s = common::Scratch::new("lsp-walk-project");
    let root = s.dir.join("project");
    common::copy_dir(Path::new(&dir), &root);
    let root = std::fs::canonicalize(&root).unwrap();
    let problems = walk("project", &root, true);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
