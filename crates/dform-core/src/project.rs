//! Projects (docs/layout.md). A project is a directory tree whose root holds
//! `dform.toml`, found up from the working directory; there is no other
//! kind (`dform init` makes one). Its state is `dform.state/` at the root,
//! and every path a program states resolves from the root.
//!
//! The manifest is per project and small: `[project]` (a name, and the
//! dform versions it takes), `[providers]` (each provider's source and
//! version constraint, Cargo's semver syntax), `[stacks.NAME]` (the
//! stack's operational settings: where its state lives, who approves),
//! `[defaults]` (what a stack's table does not say, and the lease) and
//! `[discovery]` (globs discovery skips). Programs stay in `.df` files: a
//! `provider NAME { ... }` block keeps its configuration and takes its
//! source from the manifest's entry of that name. No inputs, and no key
//! values: a deployment is named by its target. Policy reads the manifest
//! as facts, `project_provider(Name, Constraint)`,
//! `project_default(Key, Value)` and `project_stack(Name, Key, Value)`.
//!
//! Discovery (R-29): a stack is a file, named after itself. With a
//! `stacks/` directory at the root, its `.df` files are the stacks and a
//! `.df` at the root is outside the layout; without one, the root's `.df`
//! files are. A stack's keys are its `key` statements. A directory
//! holding its own `dform.toml` is another project and is not walked.
//! The layout is linted: a module or policy file with a `key` is an
//! error, and a `.df` outside the layout's directories is a warning.

use crate::ast::{Atom, Term};
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::Spanned;

/// The manifest's file name; its directory is the project root.
pub const MANIFEST: &str = "dform.toml";

/// The local backend's directory at the project root: per-deployment
/// state, audit logs, the plan key, the registry, and `cache/`.
pub const STATE_DIR: &str = "dform.state";

/// Where `.df` files belong in a project.
pub const LAYOUT_DIRS: &[&str] = &["stacks", "modules", "policies", "providers"];

/// A project: its root and its manifest.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub manifest: Manifest,
}

impl Project {
    /// The project `dir` is in: the nearest directory up holding a
    /// `dform.toml`; `None` outside every project. `version` is the
    /// running dform's, which the manifest may constrain.
    pub fn find(dir: &Path, version: &str) -> Result<Option<Project>> {
        let dir = absolute(dir)?;
        let Some(root) = manifest_root(&dir) else {
            return Ok(None);
        };
        let manifest = Manifest::load(&root.join(MANIFEST), version)?;
        Ok(Some(Project { root, manifest }))
    }

    /// The project `dir` is in, or the error that says there is none.
    pub fn require(dir: &Path, version: &str) -> Result<Project> {
        match Project::find(dir, version)? {
            Some(p) => Ok(p),
            None => Err(not_in_a_project(dir)),
        }
    }

    /// The project root's state directory, `dform.state/`.
    pub fn state_root(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }
}

/// The error of a command that needs a project, run outside one.
pub fn not_in_a_project(dir: &Path) -> anyhow::Error {
    let dir = absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    anyhow!(
        "not in a project: no {MANIFEST} above {}; run `dform init` to make one",
        dir.display()
    )
}

/// `dform init [NAME]`: a minimal `dform.toml` in `dir`, and `dform.state/`
/// in the nearest `.gitignore` (a new one in `dir` when there is none).
/// Returns what it did, a line each.
pub fn init(dir: &Path, name: Option<&str>) -> Result<Vec<String>> {
    let dir = absolute(dir)?;
    let manifest = dir.join(MANIFEST);
    if manifest.exists() {
        bail!(
            "{} exists: {} is a project already",
            manifest.display(),
            dir.display()
        );
    }
    let name = match name {
        Some(n) => n.to_string(),
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into()),
    };
    std::fs::write(
        &manifest,
        format!("# The project's root (docs/layout.md).\n\n[project]\nname = {name:?}\n"),
    )
    .with_context(|| format!("write {}", manifest.display()))?;
    let mut out = vec![format!("wrote {}", manifest.display())];
    let ignore = dir
        .ancestors()
        .map(|d| d.join(".gitignore"))
        .find(|f| f.is_file())
        .unwrap_or_else(|| dir.join(".gitignore"));
    let text = std::fs::read_to_string(&ignore).unwrap_or_default();
    let ours = format!("{STATE_DIR}/");
    if !text
        .lines()
        .any(|l| l.trim() == ours || l.trim() == STATE_DIR)
    {
        let mut text = text;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&ours);
        text.push('\n');
        std::fs::write(&ignore, text).with_context(|| format!("write {}", ignore.display()))?;
        out.push(format!("added {ours} to {}", ignore.display()));
    }
    Ok(out)
}

/// The directory a program file's paths resolve from: its project's root
/// (named relative to the working directory when under it), else, outside
/// every project, the file's own directory.
pub fn base_of(file: &Path) -> PathBuf {
    match manifest_root(file) {
        Some(root) => {
            let cwd = std::env::current_dir()
                .map(|c| std::fs::canonicalize(&c).unwrap_or(c))
                .unwrap_or_default();
            match root.strip_prefix(&cwd) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => root,
            }
        }
        None => file.parent().map(Path::to_path_buf).unwrap_or_default(),
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
    /// `[stacks.NAME]`: the stack `NAME.df`'s settings.
    #[serde(default)]
    pub stacks: BTreeMap<String, StackTable>,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteEntry>,
    /// The project root (the manifest's directory).
    #[serde(skip)]
    pub root: PathBuf,
    /// The manifest's text, which the spans of its values index.
    #[serde(skip)]
    pub text: String,
}

/// `[remotes] NAME = { backend = "TERM" }`: another project whose stacks'
/// outputs this one reads, `stack_output("NAME.STACK[k=v]", ..)`, through
/// its backend (`stack::remote_location`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEntry {
    /// A backend term as `[defaults] backend` writes it, `{stack}` the
    /// stack's name; without `{stack}`, the stacks are under it by name.
    pub backend: String,
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

/// `[defaults]`: what a stack's `[stacks.NAME]` does not say, and the
/// lease. Its settings are a stack table's.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// `local("DIR")`, DIR relative to the project root, or
    /// `s3("BUCKET", "PREFIX", {endpoint: "URL", region: "R"})`; `{stack}`
    /// the stack's name.
    pub backend: Option<Spanned<String>>,
    pub role: Option<Spanned<String>>,
    pub approvals: Option<Spanned<String>>,
    pub audit_sink: Option<Spanned<String>>,
    pub isolated: Option<Spanned<bool>>,
    pub config: Option<Spanned<String>>,
    /// How long an `s3` backend's lease lasts (`60s`; `500ms`, `2m`).
    pub lease_duration: Option<String>,
    /// How often its holder renews it (`20s`), less than the duration.
    pub lease_renewal: Option<String>,
}

/// `[stacks.NAME]`: a stack's operational settings (docs/grammar.md
/// "Stack settings"), a closed list. A term is written as a string:
/// `backend = 's3("acme", "shop/{env}")'`, `{stack}` the stack's name and
/// `{k}` the value of its key `k`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackTable {
    /// Where its state lives: `local("DIR")` or `s3(..)`.
    pub backend: Option<Spanned<String>>,
    /// `bootstrap`: it creates what a controller runs in.
    pub role: Option<Spanned<String>>,
    /// Whose signatures approve a plan: `jwks(..)`, `jwks_file(..)`, a
    /// list of them.
    pub approvals: Option<Spanned<String>>,
    /// A command each audit log entry is also piped to.
    pub audit_sink: Option<Spanned<String>>,
    /// Each key value deploys into its own account.
    pub isolated: Option<Spanned<bool>>,
    /// A document of the deployment's settings: `yaml("config/{env}.yaml")`.
    pub config: Option<Spanned<String>>,
}

/// The settings a stack table holds, as text: a term's (`backend`,
/// `approvals`, `config`), else a plain value's.
pub const STACK_SETTINGS: &[&str] = &[
    "backend",
    "role",
    "approvals",
    "audit_sink",
    "isolated",
    "config",
];

/// A stack setting's value as the manifest writes it.
#[derive(Debug, Clone)]
pub enum SettingText {
    Str(Spanned<String>),
    Bool(Spanned<bool>),
}

impl StackTable {
    /// Its settings, each by name, in [`STACK_SETTINGS`]' order.
    fn settings(&self) -> Vec<(&'static str, SettingText)> {
        let s = |v: &Option<Spanned<String>>| v.clone().map(SettingText::Str);
        [
            ("backend", s(&self.backend)),
            ("role", s(&self.role)),
            ("approvals", s(&self.approvals)),
            ("audit_sink", s(&self.audit_sink)),
            ("isolated", self.isolated.clone().map(SettingText::Bool)),
            ("config", s(&self.config)),
        ]
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect()
    }
}

impl Defaults {
    fn table(&self) -> StackTable {
        StackTable {
            backend: self.backend.clone(),
            role: self.role.clone(),
            approvals: self.approvals.clone(),
            audit_sink: self.audit_sink.clone(),
            isolated: self.isolated.clone(),
            config: self.config.clone(),
        }
    }
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
    /// running dform (`version`) meets the project's, and the settings are
    /// ones a stack could have.
    pub fn load(path: &Path, version: &str) -> Result<Manifest> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let m = Manifest::parse(path, &text)?;
        if let Some(req) = &m.project.dform {
            let at = format!("{}: [project] dform", path.display());
            let r = semver::VersionReq::parse(req).map_err(|e| anyhow!("{at} = {req:?}: {e}"))?;
            let v = semver::Version::parse(version)
                .map_err(|e| anyhow!("internal: dform's version {version}: {e}"))?;
            if !r.matches(&v) {
                bail!(
                    "{} requires dform {req}; this is dform {version}",
                    path.display()
                );
            }
        }
        Ok(m)
    }

    /// The manifest `text` of `path`, checked but for the dform version it
    /// requires.
    pub fn parse(path: &Path, text: &str) -> Result<Manifest> {
        let mut m: Manifest =
            toml::from_str(text).map_err(|e| anyhow!("{}: {}", path.display(), e.message()))?;
        m.root = path.parent().unwrap_or(Path::new("")).to_path_buf();
        m.text = text.to_string();
        let at = |key: &str| format!("{}: {key}", path.display());
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
        let tables = std::iter::once(("[defaults]".to_string(), m.defaults.table())).chain(
            m.stacks
                .iter()
                .map(|(n, t)| (format!("[stacks.{n}]"), t.clone())),
        );
        for (table, t) in tables {
            if let Some(b) = &t.backend
                && let Err(e) =
                    crate::stack::parse_backend(&b.get_ref().replace("{stack}", "stack"))
            {
                bail!(
                    "{} = {:?}: {e}; the backends are `local(\"DIR\")`, DIR relative to \
                     the project root, and `s3(\"BUCKET\", \"PREFIX\", {{endpoint: \"URL\", \
                     region: \"R\"}})`; `{{stack}}` is the stack's name, `{{k}}` its key k's \
                     value",
                    at(&format!("{table} backend")),
                    b.get_ref()
                );
            }
        }
        for (key, v) in [
            ("lease_duration", &m.defaults.lease_duration),
            ("lease_renewal", &m.defaults.lease_renewal),
        ] {
            if let Some(v) = v
                && crate::store::parse_duration(v).is_none_or(|d| d.is_zero())
            {
                bail!(
                    "{} = {v:?}: a duration, `500ms`, `30s` or `2m`",
                    at(&format!("[defaults] {key}"))
                );
            }
        }
        let t = m.lease_times();
        if t.renewal >= t.duration {
            bail!(
                "{}: the lease is renewed every {:?} but lasts {:?}; renew it more often \
                 than it lasts",
                at("[defaults] lease_renewal"),
                t.renewal,
                t.duration
            );
        }
        for (name, r) in &m.remotes {
            if name.is_empty() || name.contains(['.', '[', ']']) {
                bail!(
                    "{}: a remote's name is read as the first part of a dotted stack name;                      it has no `.`, `[` or `]`",
                    at(&format!("[remotes] {name:?}"))
                );
            }
            if crate::stack::parse_backend(&r.backend.replace("{stack}", "stack")).is_err() {
                bail!(
                    "{} = {:?}: the backends are `local(\"DIR\")`, DIR relative to the \
                     project root, and `s3(\"BUCKET\", \"PREFIX\", {{endpoint: \"URL\", \
                     region: \"R\"}})`; `{{stack}}` is the stack's name",
                    at(&format!("[remotes] {name} backend")),
                    r.backend
                );
            }
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
        let text = |v: &SettingText| match v {
            SettingText::Str(v) => Value::Str(v.get_ref().clone()),
            SettingText::Bool(v) => Value::Bool(*v.get_ref()),
        };
        for (k, v) in self.defaults.table().settings() {
            if let Value::Str(v) = text(&v) {
                out.push(atom("project_default", vec![s(k), s(&v)]));
            }
        }
        for (k, v) in [
            ("lease_duration", &self.defaults.lease_duration),
            ("lease_renewal", &self.defaults.lease_renewal),
        ] {
            if let Some(v) = v {
                out.push(atom("project_default", vec![s(k), s(v)]));
            }
        }
        for (name, t) in &self.stacks {
            for (k, v) in t.settings() {
                out.push(atom(
                    "project_stack",
                    vec![s(name), s(k), Term::Val(text(&v))],
                ));
            }
        }
        out
    }

    /// The settings of the stack `name`: `[stacks.NAME]`'s, else
    /// `[defaults]`'.
    pub fn stack_settings(&self, name: &str) -> Vec<(&'static str, SettingText)> {
        let own = self
            .stacks
            .get(name)
            .map(StackTable::settings)
            .unwrap_or_default();
        let defaults = self.defaults.table().settings();
        STACK_SETTINGS
            .iter()
            .filter_map(|k| own.iter().chain(&defaults).find(|(x, _)| x == k).cloned())
            .collect()
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

    /// The default backend of `stack` (a `local` directory relative to
    /// the project root).
    pub fn backend(&self, stack: &str) -> Option<crate::stack::Backend> {
        let text = self
            .defaults
            .backend
            .as_ref()?
            .get_ref()
            .replace("{stack}", stack);
        crate::stack::parse_backend(&text).ok()
    }

    /// `[remotes]`: each remote's backend term.
    pub fn remotes(&self) -> BTreeMap<String, String> {
        self.remotes
            .iter()
            .map(|(k, r)| (k.clone(), r.backend.clone()))
            .collect()
    }

    /// The lease's duration and renewal interval (an `s3` backend's).
    pub fn lease_times(&self) -> crate::store::LeaseTimes {
        let d = crate::store::LeaseTimes::default();
        let get = |v: &Option<String>| v.as_deref().and_then(crate::store::parse_duration);
        crate::store::LeaseTimes {
            duration: get(&self.defaults.lease_duration).unwrap_or(d.duration),
            renewal: get(&self.defaults.lease_renewal).unwrap_or(d.renewal),
        }
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

    /// Fails listing the errors (a module file with a `key`, a
    /// `[stacks.NAME]` no file is), if there are any.
    pub fn check(&self) -> Result<()> {
        if self.errors.is_empty() {
            return Ok(());
        }
        bail!("{}", self.errors.join("\n"))
    }
}

/// The directory a project's stacks are in (docs/layout.md).
pub const STACKS_DIR: &str = "stacks";

/// Walk `project` for its stacks: `stacks/*.df`, or, with no `stacks/`,
/// the root's `.df` files, each named after itself. Files are named
/// relative to the working directory when under it (as diagnostics name
/// them).
pub fn discover(project: &Project) -> Discovered {
    let mut files = Vec::new();
    let exclude = project.manifest.discovery.exclude.as_slice();
    walk(&project.root, &project.root, exclude, &mut files);
    files.sort();
    let in_dir = project.root.join(STACKS_DIR).is_dir();
    let mut out = Discovered::default();
    for f in files {
        let rel = f.strip_prefix(&project.root).unwrap_or(&f).to_path_buf();
        let first = rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .unwrap_or_default()
            .to_string();
        let depth = rel.components().count();
        let stack = match in_dir {
            true => depth == 2 && first == STACKS_DIR,
            false => depth == 1,
        };
        let keys = std::fs::read_to_string(&f)
            .ok()
            .map(|text| crate::syntax::parser::parse(&text))
            .filter(|p| p.errors.is_empty())
            .map(|p| crate::syntax::resolve::key_names(&p.syntax()))
            .unwrap_or_default();
        if !keys.is_empty() && depth > 1 && (first == "modules" || first == "policies") {
            out.errors.push(format!(
                "{}: a {} file has a `key`; a key selects a stack's deployment, so it is \
                 declared in the stack's own file, stacks/<name>.df (docs/layout.md)",
                display(&f),
                if first == "modules" {
                    "module"
                } else {
                    "policy"
                }
            ));
        }
        if !stack && (depth == 1 || !LAYOUT_DIRS.contains(&first.as_str())) {
            out.warnings.push(format!(
                "{} is outside the project layout ({}/ under the root; docs/layout.md)",
                display(&f),
                LAYOUT_DIRS.join("/, ")
            ));
        }
        if stack {
            out.stacks.push(Found {
                name: crate::state::stack_name(&f),
                keys,
                file: PathBuf::from(display(&f)),
            });
        }
    }
    for name in project.manifest.stacks.keys() {
        if out.named(name).is_empty() {
            let file = match in_dir {
                true => format!("{STACKS_DIR}/{name}.df"),
                false => format!("{name}.df"),
            };
            out.errors.push(format!(
                "{}: [stacks.{name}] names no stack: there is no {file}",
                display(&project.root.join(MANIFEST)),
            ));
        }
    }
    out
}

/// Every `.df` file of the project discovery walks, named as discovery
/// names them (`dform fmt` with no path).
pub fn df_files(project: &Project) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let exclude = project.manifest.discovery.exclude.as_slice();
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
    fn a_manifest_takes_an_s3_backend_and_its_lease_times() {
        let m = manifest(
            "[defaults]\nbackend = 's3(\"bucket\", \"dform/{stack}\", \
             {endpoint: \"http://127.0.0.1:9000\", region: \"gra\"})'\n\
             lease_duration = \"2s\"\nlease_renewal = \"500ms\"\n",
        )
        .unwrap();
        assert_eq!(
            m.backend("app"),
            Some(crate::stack::Backend::S3(crate::store::S3Spec {
                bucket: "bucket".into(),
                prefix: "dform/app".into(),
                endpoint: Some("http://127.0.0.1:9000".into()),
                region: Some("gra".into()),
            }))
        );
        assert_eq!(
            m.lease_times(),
            crate::store::LeaseTimes {
                duration: std::time::Duration::from_secs(2),
                renewal: std::time::Duration::from_millis(500),
            }
        );
        let e = manifest("[defaults]\nlease_duration = \"10s\"\nlease_renewal = \"10s\"\n")
            .unwrap_err();
        assert!(e.to_string().contains("renew it more often"), "{e}");
        let e = manifest("[defaults]\nlease_duration = \"soon\"\n").unwrap_err();
        assert!(e.to_string().contains("[defaults] lease_duration"), "{e}");
        let e = manifest("[defaults]\nbackend = 's3(\"\", \"p\")'\n").unwrap_err();
        assert!(e.to_string().contains("[defaults] backend"), "{e}");
    }

    #[test]
    fn a_manifest_checks_versions_and_defaults() {
        let m = manifest(
            "[project]\nname = \"p\"\ndform = \">=0.1\"\n\
             [providers]\naws = { source = \"aws-mock\", version = \"2.1\" }\nk8s = \"k8s\"\n\
             [defaults]\nbackend = 'local(\"state/{stack}\")'\nrole = \"bootstrap\"\n",
        )
        .unwrap();
        assert_eq!(m.provider_source("aws").as_deref(), Some("aws-mock"));
        assert_eq!(
            m.backend("app"),
            Some(crate::stack::Backend::Local(PathBuf::from("state/app")))
        );
        let facts: Vec<String> = m.facts().iter().map(crate::partition::fmt_atom).collect();
        assert_eq!(
            facts,
            [
                "project_provider(\"aws\", \"^2.1\")",
                "project_provider(\"k8s\", \"*\")",
                "project_default(\"backend\", \"local(\\\"state/{stack}\\\")\")",
                "project_default(\"role\", \"bootstrap\")",
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
        let e = manifest("[defaults]\nunknowns = \"strict\"\n").unwrap_err();
        assert!(e.to_string().contains("unknown field `unknowns`"), "{e}");
        let e = manifest("[inputs]\nenv = \"prod\"\n").unwrap_err();
        assert!(e.to_string().contains("unknown field `inputs`"), "{e}");
    }
}
