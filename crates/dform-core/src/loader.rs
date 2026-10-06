use crate::ast::Program;
use crate::diag;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
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

/// Every file of the program is parsed, then the whole program is resolved
/// at once (R-65): the entry files, and every module and component a `use`
/// or a resource's type names by its path, and theirs in turn, each
/// loaded once however often it is named. A path is looked up, never searched:
/// `modules.net` is `modules/net.df` under the project root (outside every
/// project, beside the entry), a first segment `[packages]` names is that
/// project's root, `std.x` is the standard library, and a stack's file is
/// not loaded at all: what is read of it is its deployments' outputs.
pub fn load_program(entry_files: &[PathBuf]) -> Result<Program> {
    load_program_with(entry_files, &|p| fs::read_to_string(p))
}

/// [`load_program`], each file's text read by `read` (given the file's
/// canonical path): a language server's open buffers, unsaved, over the
/// files on disk.
pub fn load_program_with(
    entry_files: &[PathBuf],
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Program> {
    load_program_files_with(entry_files, read).map(|(p, _, _)| p)
}

/// [`load_program_with`], the files it read, in the order they load (the
/// sources a controller watches, R-39), and the stacks its `use`s name.
pub fn load_program_files_with(
    entry_files: &[PathBuf],
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<(Program, Vec<PathBuf>, Vec<crate::syntax::resolve::Deployed>)> {
    let loaded = load_units(entry_files, read)?;
    let stack = match entry_files.first() {
        Some(f) => stack_source(f, read)?,
        None => None,
    };
    crate::syntax::resolve::lower_stack(
        &loaded.units,
        &loaded.entries,
        true,
        crate::syntax::resolve::Mode::Program,
        stack.as_ref(),
        &loaded.deployed,
    )
    .map(|p| (p, loaded.files.clone(), loaded.deployed.clone()))
    .map_err(|d| diag::Diagnostics(d).into())
}

/// The units of a program: its entry files first, then the modules their
/// paths reach.
struct Loaded {
    units: Vec<crate::syntax::resolve::Unit>,
    entries: Vec<usize>,
    /// Each unit's file, canonical.
    files: Vec<PathBuf>,
    /// The stacks a `use` names (R-65).
    deployed: Vec<crate::syntax::resolve::Deployed>,
}

/// Where paths are looked up: the project root (or, outside every
/// project, the entry's directory) and the packages mounted in it.
struct Mounts {
    root: PathBuf,
    project: bool,
    packages: BTreeMap<String, PathBuf>,
    /// The providers `dform.toml` names (`[providers]`).
    providers: BTreeSet<String>,
}

/// What a path names.
enum Target {
    /// A module's file: its module path, its canonical file.
    File(String, PathBuf),
    /// A stack, deployed by the tool.
    Stack(crate::syntax::resolve::Deployed),
    /// The standard library.
    Std,
    /// Nothing: the files it could have been.
    Missing(Vec<PathBuf>),
}

impl Mounts {
    fn of(entry: &Path, read: &dyn Fn(&Path) -> std::io::Result<String>) -> Result<Mounts> {
        let Some(root) = crate::project::manifest_root(entry) else {
            return Ok(Mounts {
                root: entry.parent().unwrap_or(Path::new(".")).to_path_buf(),
                project: false,
                packages: BTreeMap::new(),
                providers: BTreeSet::new(),
            });
        };
        let path = root.join(crate::project::MANIFEST);
        let text = read(&path).with_context(|| format!("read {}", path.display()))?;
        let manifest = crate::project::Manifest::parse(&path, &text)?;
        Ok(Mounts {
            packages: manifest.package_roots(),
            providers: manifest.providers.keys().cloned().collect(),
            root,
            project: true,
        })
    }

    /// Whether `name` is a provider a `use` may import (R-112): one
    /// `dform.toml` names, a built-in (`file`, `env`, the mock's schemas,
    /// by their name or their types' namespace: `aws` of aws-mock's
    /// `aws.vpc`) or a project's `providers/NAME/`.
    fn provider(&self, name: &str) -> bool {
        self.providers.contains(name)
            || crate::externs::builtin(name).is_some()
            || crate::schema::builtin(name).is_some()
            || crate::syntax::resolve::builtin_namespace(name)
            || self.root.join("providers").join(name).is_dir()
    }

    /// The file a path names: `a.b.c` is `a/b/c.df`, or the item `c` of
    /// `a/b.df`.
    fn lookup(&self, path: &str) -> Target {
        let segs: Vec<&str> = path.split('.').collect();
        if segs[0] == "std" {
            return Target::Std;
        }
        let (base, rest, prefix, project) = match self.packages.get(segs[0]) {
            Some(root) if segs.len() > 1 => (root.clone(), &segs[1..], Some(segs[0]), true),
            _ => (self.root.clone(), &segs[..], None, self.project),
        };
        let file = |segs: &[&str]| {
            let mut f = base.clone();
            for s in segs {
                f.push(s);
            }
            f.set_extension("df");
            f
        };
        let mut tried = vec![file(rest)];
        let (found, module) = if tried[0].is_file() {
            (tried[0].clone(), path.to_string())
        } else if rest.len() > 1 && file(&rest[..rest.len() - 1]).is_file() {
            let m = segs[..segs.len() - 1].join(".");
            (file(&rest[..rest.len() - 1]), m)
        } else {
            if rest.len() > 1 {
                tried.push(file(&rest[..rest.len() - 1]));
            }
            return Target::Missing(tried);
        };
        let found = fs::canonicalize(&found).unwrap_or(found);
        let base = fs::canonicalize(&base).unwrap_or(base);
        // A stack is a file under stacks/ (R-65), or one dform.toml's
        // `[stacks.NAME]` names.
        let under_stacks = found
            .parent()
            .is_some_and(|d| d == base.join(crate::project::STACKS_DIR));
        let named = || {
            let stem = crate::state::stack_name(&found);
            found.parent() == Some(base.as_path())
                && std::fs::read_to_string(base.join(crate::project::MANIFEST))
                    .ok()
                    .and_then(|t| {
                        crate::project::Manifest::parse(&base.join(crate::project::MANIFEST), &t)
                            .ok()
                    })
                    .is_some_and(|m| m.stacks.contains_key(&stem))
        };
        if project && (under_stacks || named()) {
            let keys = fs::read_to_string(&found)
                .ok()
                .map(|text| crate::syntax::parser::parse(&text))
                .filter(|p| p.errors.is_empty())
                .map(|p| crate::syntax::resolve::key_names(&p.syntax()))
                .unwrap_or_default();
            let stem = crate::state::stack_name(&found);
            return Target::Stack(crate::syntax::resolve::Deployed {
                path: module,
                name: match prefix {
                    Some(p) => format!("{p}.{stem}"),
                    None => stem,
                },
                keys,
            });
        }
        Target::File(module, found)
    }
}

fn load_units(
    entry_files: &[PathBuf],
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Loaded> {
    use crate::syntax::SyntaxKind::{COMPONENT, RESOURCE, USE};
    let mut loaded = Loaded {
        units: Vec::new(),
        entries: Vec::new(),
        files: Vec::new(),
        deployed: Vec::new(),
    };
    let Some(first) = entry_files.first() else {
        return Ok(loaded);
    };
    let mounts = Mounts::of(&absolutize(first)?, read)?;
    let std = crate::functions::registry().packages();
    for f in entry_files {
        let abs = absolutize(f)?;
        let abs = fs::canonicalize(&abs).unwrap_or(abs);
        if loaded.files.contains(&abs) {
            continue;
        }
        let i = load_unit(&abs, None, read, &mut loaded)?;
        loaded.entries.push(i);
    }
    // The edges between files, for the cycle check: (from, to, where).
    let mut edges: Vec<(usize, usize, crate::ast::Span)> = Vec::new();
    let mut errors = Vec::new();
    let mut done = 0;
    while done < loaded.units.len() {
        let i = done;
        done += 1;
        let (file, root) = (loaded.units[i].file, loaded.units[i].root.clone());
        // Names the file binds itself: its components, and what its
        // `use`s bind. A resource's type starting with one is the file's
        // own, resolved where it is lowered.
        let mut local = BTreeSet::new();
        let mut components = BTreeSet::new();
        for n in root.descendants() {
            match n.kind() {
                COMPONENT => {
                    components.insert(crate::syntax::resolve::component_name(&n));
                    local.insert(crate::syntax::resolve::component_name(&n));
                }
                USE => {
                    local.insert(crate::syntax::resolve::use_parts(&n).1);
                }
                // A component signature (R-104): the resolver says it is
                // not instanced.
                crate::syntax::SyntaxKind::TYPE_ALIAS
                    if n.children()
                        .any(|c| c.kind() == crate::syntax::SyntaxKind::SIGNATURE) =>
                {
                    local.insert(crate::syntax::resolve::component_name(&n));
                }
                _ => {}
            }
        }
        for n in root.descendants() {
            let path = match n.kind() {
                USE => crate::syntax::resolve::use_parts(&n).0,
                RESOURCE => crate::syntax::resolve::copy_parts(&n).0,
                _ => continue,
            };
            // A path whose first segment the file binds itself is its own,
            // resolved where it is lowered: a component it declares, or a
            // module one of its `use`s brings.
            let head = path.split('.').next().unwrap_or_default();
            if components.contains(head) || (n.kind() == RESOURCE && local.contains(head)) {
                continue;
            }
            if path.is_empty() {
                continue;
            }
            let span = span_at(file, n.text_range());
            // A resource's type by its path from the root (R-113): a
            // component of a file is loaded with it, and a module's or a
            // stack's for the resolver to say what it is; anything else is
            // a provider's type, or one the program declares. A built-in
            // schema's type (`net.vpc`) stays the type, and a file naming
            // its own component by its path loads nothing.
            if n.kind() == RESOURCE {
                if crate::syntax::resolve::builtin_type(&path) {
                    continue;
                }
                match mounts.lookup(&path) {
                    Target::File(module, f) if f != loaded.files[i] => {
                        let j = match loaded.files.iter().position(|x| *x == f) {
                            Some(j) => j,
                            None => load_unit(&f, Some(module), read, &mut loaded)?,
                        };
                        edges.push((i, j, span));
                    }
                    Target::Stack(d) if !loaded.deployed.iter().any(|x| x.path == d.path) => {
                        loaded.deployed.push(d);
                    }
                    _ => {}
                }
                continue;
            }
            match mounts.lookup(&path) {
                Target::Std => {}
                Target::Stack(d) => {
                    if !loaded.deployed.iter().any(|x| x.path == d.path) {
                        loaded.deployed.push(d);
                    }
                }
                // One segment that is no module: a provider's `use`
                // (R-112), which the resolver configures.
                Target::Missing(_)
                    if n.kind() == USE
                        && !path.contains('.')
                        && (mounts.provider(&path) || crate::syntax::resolve::names_source(&n)) => {
                }
                Target::Missing(tried) => {
                    let tried: Vec<String> = tried.iter().map(|f| display_name(f)).collect();
                    let mut d = diag::Diagnostic::error(
                        span,
                        format!("no module `{path}`: there is no {}", tried.join(" and no ")),
                    )
                    .with_help(
                        "a path is the file's from the project root, its `/` a `.`: \
                         `modules.net` is modules/net.df, and `modules.net.vpc` its \
                         `component vpc`",
                    );
                    if !path.contains('.') {
                        d = d.with_note(format!(
                            "nor a provider: `dform.toml` names none `{path}`, and there is \
                             no built-in one and no providers/{path}/"
                        ));
                    }
                    errors.push(d);
                }
                Target::File(module, f) => {
                    let stem = crate::state::stack_name(&f);
                    if std.contains(&stem.as_str()) {
                        errors.push(
                            diag::Diagnostic::error(
                                span,
                                format!(
                                    "the module {} is named like the standard library's \
                                     `{stem}`",
                                    display_name(&f)
                                ),
                            )
                            .with_help(format!("rename it: std.{stem} is always in scope")),
                        );
                        continue;
                    }
                    let j = match loaded.files.iter().position(|x| *x == f) {
                        Some(j) => j,
                        None => load_unit(&f, Some(module), read, &mut loaded)?,
                    };
                    edges.push((i, j, span));
                }
            }
        }
    }
    if errors.is_empty() {
        errors.extend(cycle(&loaded, &edges));
    }
    if !errors.is_empty() {
        return Err(diag::Diagnostics(errors).into());
    }
    Ok(loaded)
}

/// A `use` or resource-type cycle among the program's files: an error naming
/// it, at the statement that closes it.
fn cycle(loaded: &Loaded, edges: &[(usize, usize, crate::ast::Span)]) -> Option<diag::Diagnostic> {
    fn visit(
        i: usize,
        edges: &[(usize, usize, crate::ast::Span)],
        path: &mut Vec<(usize, Option<crate::ast::Span>)>,
        done: &mut BTreeSet<usize>,
    ) -> Option<Vec<(usize, Option<crate::ast::Span>)>> {
        if let Some(at) = path.iter().position(|(x, _)| *x == i) {
            return Some(path[at..].to_vec());
        }
        if !done.insert(i) {
            return None;
        }
        for (from, to, span) in edges {
            if *from == i {
                path.push((i, Some(*span)));
                if let Some(c) = visit(*to, edges, path, done) {
                    return Some(c);
                }
                path.pop();
            }
        }
        None
    }
    let mut done = BTreeSet::new();
    for &e in &loaded.entries {
        let mut path = Vec::new();
        if let Some(c) = visit(e, edges, &mut path, &mut done) {
            let name = |i: usize| match &loaded.units[i].path {
                Some(p) => p.clone(),
                None => display_name(&loaded.files[i]),
            };
            let mut names: Vec<String> = c.iter().map(|(i, _)| name(*i)).collect();
            names.push(name(c[0].0));
            let at = c.last().and_then(|(_, s)| *s).unwrap_or_default();
            return Some(
                diag::Diagnostic::error(at, format!("use cycle: {}", names.join(" -> ")))
                    .with_note("a module is loaded before what uses it, so none may use itself"),
            );
        }
    }
    None
}

fn span_at(file: u32, r: rowan::TextRange) -> crate::ast::Span {
    crate::ast::Span {
        file,
        start: r.start().into(),
        end: r.end().into(),
        origin: 0,
    }
}

/// The manifests as last registered, by path: their name, their text and
/// the sources they registered (pinned while the entry lives), as
/// [`PARSED`] keeps the files.
type Registered = (String, String, u32, Vec<u32>);
static MANIFESTS: Mutex<BTreeMap<PathBuf, Registered>> = Mutex::new(BTreeMap::new());

/// The stack `entry` is (R-29): named after the file, with the settings
/// its project's dform.toml gives it, `[stacks.NAME]` over `[defaults]`.
/// `None` outside a project, or when the manifest gives it none.
fn stack_source(
    entry: &Path,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Option<crate::syntax::resolve::StackSource>> {
    use crate::project::SettingText;
    use crate::syntax::resolve::{Setting, SettingValue, StackSource};
    let abs = absolutize(entry)?;
    let Some(root) = crate::project::manifest_root(&abs) else {
        return Ok(None);
    };
    let path = root.join(crate::project::MANIFEST);
    let text = read(&path).with_context(|| format!("read {}", path.display()))?;
    let name = display_name(&path);
    let manifest = crate::project::Manifest::parse(Path::new(&name), &text)?;
    let stack = crate::state::stack_name(entry);
    let settings = manifest.stack_settings(&stack);
    if settings.is_empty() {
        return Ok(None);
    }
    let file = {
        let mut cache = MANIFESTS.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(&path) {
            Some((n, t, file, _)) if *n == name && *t == text => *file,
            _ => {
                let mark = diag::mark();
                let file = diag::add_source(&name, &text);
                let sources = diag::pin_since(mark);
                if let Some((_, _, _, old)) =
                    cache.insert(path.clone(), (name.clone(), text.clone(), file, sources))
                {
                    diag::remove(&old);
                }
                file
            }
        }
    };
    let span = |r: std::ops::Range<usize>| crate::ast::Span {
        file,
        start: r.start as u32,
        end: r.end as u32,
        origin: 0,
    };
    let plain = |v: crate::value::Value| SettingValue::Plain(crate::ast::Term::Val(v));
    let settings = settings
        .into_iter()
        .map(|(key, v)| {
            let (value, at) = match v {
                SettingText::Bool(b) => (plain(crate::value::Value::Bool(*b.get_ref())), b.span()),
                SettingText::Str(v) if matches!(key, "backend" | "approvals") => {
                    // The term starts after the string's opening quote.
                    let raw = &text[v.span()];
                    let quote = if raw.starts_with("'''") || raw.starts_with(r#"""""#) {
                        3
                    } else {
                        1
                    };
                    let offset = (v.span().start + quote) as u32;
                    let text = v.get_ref().clone();
                    (SettingValue::Term { text, offset }, v.span())
                }
                SettingText::Str(v) => (
                    plain(crate::value::Value::Str(v.get_ref().clone())),
                    v.span(),
                ),
            };
            Setting {
                key: key.to_string(),
                value,
                span: span(at),
            }
        })
        .collect();
    Ok(Some(StackSource {
        name: stack,
        settings,
        span: span(0..0),
    }))
}

/// The files of the program `entry_files` name, in the order they load:
/// each entry file, and every module file its paths reach.
pub fn program_files(entry_files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    Ok(load_units(entry_files, &|p| fs::read_to_string(p))?.files)
}

/// Parse `abs` (canonical) into a unit: an entry file, or with `module`
/// the module that path names.
fn load_unit(
    abs: &Path,
    module: Option<String>,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
    loaded: &mut Loaded,
) -> Result<usize> {
    let text = read(abs).with_context(|| format!("read {}", abs.display()))?;
    let name = display_name(abs);
    let (green, file) = {
        let mut cache = PARSED.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(abs) {
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
                    abs.to_path_buf(),
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
    let i = loaded.units.len();
    loaded.files.push(abs.to_path_buf());
    loaded.units.push(crate::syntax::resolve::Unit {
        file,
        root: crate::syntax::SyntaxNode::new_root(green),
        path: module,
    });
    Ok(i)
}

/// Predicates the provider or the CLI injects as facts (discovery, world,
/// schema, inputs). They are defined even when a run has no rows for them.
pub const PROVIDER_PREDS: &[&str] = &[
    "input",
    "data",
    "instance_of",
    "cloud_exists",
    "cloud_attr",
    "cloud_computed",
    "world_attr",
    "identity",
    "deformation",
    "derived_at_last_apply",
    "in_instance",
    "world_digest",
    "may_derive",
    "drift",
    "type_attr",
    "type_doc",
    "type_list_key",
    "type_provider",
    "type_retry",
    "type_replace",
    "capability",
    "tag_path",
    "project_provider",
    "project_default",
    "project_stack",
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
            | "attr_base"
            | "type_lattice"
            | "type_mint"
            | "ignore_changes"
            | "lifecycle"
            | "moved"
            | "doc"
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
        fs::write(&f, "\np(1)\n").unwrap();
        let a = load_program(std::slice::from_ref(&f)).unwrap();
        let b = load_program(std::slice::from_ref(&f)).unwrap();
        assert_eq!(file_ids(&a), file_ids(&b));
        let old = *file_ids(&a).first().unwrap();

        fs::write(&f, "\np(2)\n").unwrap();
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

    /// `load_program_with` reads each file through its reader: an open
    /// buffer's text, not the file's on disk, and a module it uses too.
    #[test]
    fn a_reader_stands_in_for_the_disk() {
        let dir = std::env::temp_dir().join(format!("dform-loader-with-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (main, lib) = (dir.join("main.df"), dir.join("lib.df"));
        fs::write(&main, "\nuse lib\np(1)\n").unwrap();
        fs::write(&lib, "\nq(1)\n").unwrap();
        let buffer = |p: &Path| -> std::io::Result<String> {
            if p.ends_with("lib.df") {
                Ok("\nq(2)\n".into())
            } else {
                fs::read_to_string(p)
            }
        };
        let p = load_program_with(std::slice::from_ref(&main), &buffer).unwrap();
        let facts: Vec<String> = p
            .statements
            .iter()
            .flat_map(|s| match s {
                Stmt::Module(m) => m.body.clone(),
                s => vec![s.clone()],
            })
            .filter_map(|s| match s {
                Stmt::Fact(a) => Some(crate::partition::fmt_atom(&a)),
                _ => None,
            })
            .collect();
        assert!(facts.contains(&"q(2)".to_string()), "{facts:?}");
        assert!(!facts.contains(&"q(1)".to_string()), "{facts:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
