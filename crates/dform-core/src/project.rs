//! Projects (docs/layout.md). A project is a directory tree whose root holds
//! `dform.toml`; without one, the git root, else the directory itself. Its
//! state is `dform.state/` at the root.
//!
//! The manifest is per project and small: `[project]` (a name, and the
//! dform versions it takes), `[providers]` (each provider's source and
//! version constraint, Cargo's semver syntax), `[defaults]` (a backend
//! template and `unknowns`, which a stack statement overrides) and
//! `[discovery]` (globs discovery skips). Programs stay in `.df` files: a
//! `provider NAME { ... }` block keeps its configuration and takes its
//! source from the manifest's entry of that name. Nothing per deployment
//! lives here: no inputs, keys or settings. Policy reads the manifest as
//! facts, `project_provider(Name, Constraint)` and
//! `project_default(Key, Value)`.
//!
//! Discovery walks the project for `.df` files: every file with a `stack`
//! statement is a stack, and stack names are unique per project. A
//! directory holding its own `dform.toml` is another project and is not
//! walked. In a project with a manifest the layout is linted: a module or
//! policy file with a `stack` statement is an error, and a `.df` outside
//! the layout's directories is a warning.

use crate::ast::{Atom, Term};
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The manifest's file name; its directory is the project root.
pub const MANIFEST: &str = "dform.toml";

/// The local backend's directory at the project root: per-deployment
/// state, audit logs, the plan key, the registry, and `cache/`.
pub const STATE_DIR: &str = "dform.state";

/// Where `.df` files belong in a project with a manifest.
pub const LAYOUT_DIRS: &[&str] = &["stacks", "modules", "policies", "scenarios", "providers"];

/// A project: its root, and its manifest when it has one.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub manifest: Option<Manifest>,
}

impl Project {
    /// The project `dir` is in: the nearest directory up holding a
    /// `dform.toml`, else the git root, else `dir`. `version` is the
    /// running dform's, which the manifest may constrain.
    pub fn find(dir: &Path, version: &str) -> Result<Project> {
        let dir = absolute(dir)?;
        if let Some(root) = manifest_root(&dir) {
            let manifest = Manifest::load(&root.join(MANIFEST), version)?;
            return Ok(Project {
                root,
                manifest: Some(manifest),
            });
        }
        let root = dir
            .ancestors()
            .find(|d| d.join(".git").exists())
            .unwrap_or(&dir)
            .to_path_buf();
        Ok(Project {
            root,
            manifest: None,
        })
    }

    /// The project root's state directory, `dform.state/`.
    pub fn state_root(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }
}

/// The nearest directory at or above `path` (a file or a directory)
/// holding a `dform.toml`.
pub fn manifest_root(path: &Path) -> Option<PathBuf> {
    let path = absolute(path).ok()?;
    path.ancestors()
        .find(|d| d.join(MANIFEST).is_file())
        .map(Path::to_path_buf)
}

fn absolute(p: &Path) -> Result<PathBuf> {
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().context("current_dir")?.join(p)
    };
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// `dform.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub project: Meta,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// The project root (the manifest's directory).
    #[serde(skip)]
    pub root: PathBuf,
}

/// `[project]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: Option<String>,
    /// The dform versions the project takes, as a semver requirement.
    pub dform: Option<String>,
}

/// `[providers] NAME = { source = "...", version = "..." }`, or
/// `NAME = "SOURCE"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProviderEntry {
    Source(String),
    Table(ProviderTable),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderTable {
    /// A path relative to the project root (a plugin executable, a
    /// directory holding one or a schema, a schema `.df`), else a built-in
    /// schema the mock plays (`fake`, `gke`, `k8s`, `aws-mock`).
    pub source: Option<String>,
    /// A version requirement in Cargo's syntax (`"2.1"` is `^2.1`).
    pub version: Option<String>,
}

impl ProviderEntry {
    fn source(&self) -> Option<&str> {
        match self {
            ProviderEntry::Source(s) => Some(s),
            ProviderEntry::Table(t) => t.source.as_deref(),
        }
    }

    fn version(&self) -> Option<&str> {
        match self {
            ProviderEntry::Source(_) => None,
            ProviderEntry::Table(t) => t.version.as_deref(),
        }
    }
}

/// `[defaults]`: what a stack statement that does not say takes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// `local("DIR")`, DIR relative to the project root, `{stack}` the
    /// stack's name.
    pub backend: Option<String>,
    /// `strict` or `permissive`.
    pub unknowns: Option<String>,
}

/// `[discovery]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Globs relative to the project root (`*`, `?`, `**`) discovery does
    /// not walk.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Manifest {
    /// Read and check `path`: every version requirement parses, the
    /// running dform (`version`) meets the project's, and the defaults are
    /// ones a stack statement could say.
    pub fn load(path: &Path, version: &str) -> Result<Manifest> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut m: Manifest =
            toml::from_str(&text).map_err(|e| anyhow!("{}: {}", path.display(), e.message()))?;
        m.root = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let at = |key: &str| format!("{}: {key}", path.display());
        if let Some(req) = &m.project.dform {
            let r = semver::VersionReq::parse(req)
                .map_err(|e| anyhow!("{} = {req:?}: {e}", at("[project] dform")))?;
            let v = semver::Version::parse(version)
                .map_err(|e| anyhow!("internal: dform's version {version}: {e}"))?;
            if !r.matches(&v) {
                bail!(
                    "{} requires dform {req}; this is dform {version}",
                    path.display()
                );
            }
        }
        for (name, p) in &m.providers {
            if let Some(req) = p.version() {
                semver::VersionReq::parse(req).map_err(|e| {
                    anyhow!(
                        "{} = {req:?}: {e} (Cargo's syntax: \"2.1\", \"~2.1\", \">=2.1, <3\")",
                        at(&format!("[providers.{name}] version"))
                    )
                })?;
            }
        }
        if let Some(u) = &m.defaults.unknowns
            && u != "strict"
            && u != "permissive"
        {
            bail!(
                "{} = {u:?}: `strict` or `permissive`",
                at("[defaults] unknowns")
            );
        }
        if let Some(b) = &m.defaults.backend
            && local_dir(b).is_none()
        {
            bail!(
                "{} = {b:?}: the one backend is `local(\"DIR\")`, DIR relative to the \
                 project root, `{{stack}}` the stack's name",
                at("[defaults] backend")
            );
        }
        for g in &m.discovery.exclude {
            if g.starts_with('/') {
                bail!(
                    "{} {g:?}: a glob relative to the project root",
                    at("[discovery] exclude")
                );
            }
        }
        Ok(m)
    }

    /// The manifest as facts: `project_provider(Name, Constraint)` per
    /// provider (`*` without one) and `project_default(Key, Value)` per
    /// default.
    pub fn facts(&self) -> Vec<Atom> {
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let mut out = Vec::new();
        for (name, p) in &self.providers {
            let req = p
                .version()
                .and_then(|r| semver::VersionReq::parse(r).ok())
                .map_or("*".to_string(), |r| r.to_string());
            out.push(atom("project_provider", vec![s(name), s(&req)]));
        }
        for (k, v) in [
            ("backend", &self.defaults.backend),
            ("unknowns", &self.defaults.unknowns),
        ] {
            if let Some(v) = v {
                out.push(atom("project_default", vec![s(k), s(v)]));
            }
        }
        out
    }

    /// The provider `name`'s source as `--provider` takes it, when the
    /// manifest has one: a path under the root (an executable, a directory
    /// holding one, else its `schema.df`; a `.df` file), else the source
    /// as written (a built-in schema's name).
    pub fn provider_source(&self, name: &str) -> Option<String> {
        let src = self.providers.get(name)?.source()?;
        let path = self.root.join(src);
        if !path.exists() {
            return Some(src.to_string());
        }
        let path = if path.is_dir() {
            crate::plugin::source::plugin_in(&path).unwrap_or_else(|| path.join("schema.df"))
        } else {
            path
        };
        Some(path.display().to_string())
    }

    /// The default backend's directory for `stack`, relative to the
    /// project root.
    pub fn backend(&self, stack: &str) -> Option<PathBuf> {
        let dir = local_dir(self.defaults.backend.as_deref()?)?;
        Some(PathBuf::from(dir.replace("{stack}", stack)))
    }
}

fn atom(pred: &str, args: Vec<Term>) -> Atom {
    Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span: Default::default(),
    }
}

/// `local("DIR")`'s DIR.
fn local_dir(backend: &str) -> Option<&str> {
    let dir = backend
        .trim()
        .strip_prefix("local(\"")?
        .strip_suffix("\")")?;
    (!dir.is_empty() && !dir.contains('"')).then_some(dir)
}

/// A stack discovery found: its name, its key's inputs, and its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub keys: Vec<String>,
    pub file: PathBuf,
}

/// What discovery found in a project, and what its layout lints say.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub stacks: Vec<Found>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl Discovered {
    /// The stacks named `name`.
    pub fn named(&self, name: &str) -> Vec<&Found> {
        self.stacks.iter().filter(|s| s.name == name).collect()
    }

    /// Fails listing the errors (two stacks of one name, a module file
    /// with a `stack` statement), if there are any.
    pub fn check(&self) -> Result<()> {
        if self.errors.is_empty() {
            return Ok(());
        }
        bail!("{}", self.errors.join("\n"))
    }
}

/// Walk `project` for its stacks. Files are named relative to the working
/// directory when under it (as diagnostics name them).
pub fn discover(project: &Project) -> Discovered {
    let mut files = Vec::new();
    let exclude: &[String] = project
        .manifest
        .as_ref()
        .map_or(&[], |m| m.discovery.exclude.as_slice());
    walk(&project.root, &project.root, exclude, &mut files);
    files.sort();
    let mut out = Discovered::default();
    for f in files {
        let rel = f.strip_prefix(&project.root).unwrap_or(&f).to_path_buf();
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let parse = crate::syntax::parser::parse(&text);
        if !parse.errors.is_empty() {
            continue;
        }
        let header = crate::syntax::resolve::stack_header(&parse.syntax());
        let first = rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .unwrap_or_default()
            .to_string();
        let top = rel.components().count() > 1;
        if project.manifest.is_some() {
            if header.is_some() && top && (first == "modules" || first == "policies") {
                out.errors.push(format!(
                    "{}: a {} file has a `stack` statement; a stack is its own file, \
                     stacks/<name>.df (docs/layout.md)",
                    display(&f),
                    if first == "modules" {
                        "module"
                    } else {
                        "policy"
                    }
                ));
            }
            if !top || !LAYOUT_DIRS.contains(&first.as_str()) {
                out.warnings.push(format!(
                    "{} is outside the project layout ({}/ under the root; docs/layout.md)",
                    display(&f),
                    LAYOUT_DIRS.join("/, ")
                ));
            }
        }
        if let Some((name, keys)) = header {
            out.stacks.push(Found {
                name,
                keys,
                file: PathBuf::from(display(&f)),
            });
        }
    }
    let mut by: BTreeMap<&str, Vec<&Found>> = BTreeMap::new();
    for s in &out.stacks {
        by.entry(&s.name).or_default().push(s);
    }
    for (name, fs) in by.iter().filter(|(_, fs)| fs.len() > 1) {
        out.errors.push(format!(
            "stack {name} is stated by {} files; a stack name is unique in its project: {}",
            fs.len(),
            fs.iter()
                .map(|f| f.file.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// Every `.df` file of the project discovery walks, named as discovery
/// names them (`dform fmt` with no path).
pub fn df_files(project: &Project) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let exclude: &[String] = project
        .manifest
        .as_ref()
        .map_or(&[], |m| m.discovery.exclude.as_slice());
    walk(&project.root, &project.root, exclude, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|f| PathBuf::from(display(&f)))
        .collect()
}

/// A path relative to the working directory when it is under it.
fn display(p: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
            p.strip_prefix(cwd).ok().map(Path::to_path_buf)
        })
        .unwrap_or_else(|| p.to_path_buf())
        .display()
        .to_string()
}

fn walk(root: &Path, dir: &Path, exclude: &[String], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if exclude.iter().any(|g| glob(g, &rel)) {
            continue;
        }
        let Ok(ty) = e.file_type() else {
            continue;
        };
        if ty.is_dir() {
            let skip = name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == STATE_DIR
                || path.join(MANIFEST).is_file();
            if !skip {
                walk(root, &path, exclude, out);
            }
        } else if name.ends_with(".df") {
            out.push(path);
        }
    }
}

/// Does `path` (`/`-separated, relative) match `pat`: `*` and `?` within
/// a segment, `**` any number of segments. A pattern matching a
/// directory matches what is under it.
pub fn glob(pat: &str, path: &str) -> bool {
    let p: Vec<&str> = pat.trim_end_matches('/').split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    fn segs(p: &[&str], s: &[&str]) -> bool {
        match p.split_first() {
            None => true,
            Some((&"**", rest)) => (0..=s.len()).any(|i| segs(rest, &s[i..])),
            Some((first, rest)) => {
                !s.is_empty() && seg(first.as_bytes(), s[0].as_bytes()) && segs(rest, &s[1..])
            }
        }
    }
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => seg(&p[1..], s) || (!s.is_empty() && seg(p, &s[1..])),
            (Some(b'?'), Some(_)) => seg(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => seg(&p[1..], &s[1..]),
            _ => false,
        }
    }
    segs(&p, &s)
}

/// The git commit the directory `dir` is at, when it is in a repository.
pub fn git_head(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("scratch/**", "scratch/a/b.df"));
        assert!(glob("scratch", "scratch/a.df"));
        assert!(glob("**/*.df", "a/b/c.df"));
        assert!(glob("*.df", "c.df"));
        assert!(!glob("*.df", "a/c.df"));
        assert!(glob("stacks/?.df", "stacks/a.df"));
        assert!(!glob("stacks/?.df", "stacks/ab.df"));
    }

    fn manifest(text: &str) -> Result<Manifest> {
        let dir = std::env::temp_dir().join(format!(
            "dform-manifest-{}-{}",
            std::process::id(),
            text.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST), text).unwrap();
        let m = Manifest::load(&dir.join(MANIFEST), "0.1.0");
        std::fs::remove_dir_all(&dir).unwrap();
        m
    }

    #[test]
    fn a_manifest_checks_versions_and_defaults() {
        let m = manifest(
            "[project]\nname = \"p\"\ndform = \">=0.1\"\n\
             [providers]\naws = { source = \"aws-mock\", version = \"2.1\" }\nk8s = \"k8s\"\n\
             [defaults]\nbackend = 'local(\"state/{stack}\")'\nunknowns = \"strict\"\n",
        )
        .unwrap();
        assert_eq!(m.provider_source("aws").as_deref(), Some("aws-mock"));
        assert_eq!(m.backend("app"), Some(PathBuf::from("state/app")));
        let facts: Vec<String> = m.facts().iter().map(crate::partition::fmt_atom).collect();
        assert_eq!(
            facts,
            [
                "project_provider(\"aws\", \"^2.1\")",
                "project_provider(\"k8s\", \"*\")",
                "project_default(\"backend\", \"local(\\\"state/{stack}\\\")\")",
                "project_default(\"unknowns\", \"strict\")",
            ]
        );
        let e = manifest("[project]\ndform = \">=9\"\n").unwrap_err();
        assert!(
            e.to_string()
                .contains("requires dform >=9; this is dform 0.1.0"),
            "{e}"
        );
        let e = manifest("[providers]\naws = { version = \"~>2.1\" }\n").unwrap_err();
        assert!(e.to_string().contains("[providers.aws] version"), "{e}");
        let e = manifest("[defaults]\nunknowns = \"lax\"\n").unwrap_err();
        assert!(e.to_string().contains("[defaults] unknowns"), "{e}");
        let e = manifest("[inputs]\nenv = \"prod\"\n").unwrap_err();
        assert!(e.to_string().contains("unknown field `inputs`"), "{e}");
    }
}
