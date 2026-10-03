//! Stacks (DESIGN.org L6, R-29): a stack is a file, named after itself
//! (`stacks/shop.df` is `shop`), and one program is one stack. Its
//! operational settings are dform.toml's `[stacks.NAME]` over
//! `[defaults]`: `backend = 'local("dir")'` is the directory its state,
//! world and lock live in (or a bucket, `store`), and a lock there makes a
//! second concurrent apply fail cleanly; `role`, `approvals`,
//! `audit_sink` and `isolated` are the rest (docs/grammar.md
//! "Stack settings"). The loader lowers them to the program's `stack`.
//!
//! Keyed stacks: `key env: T` declares an input the target gives (`shop
//! env=prod`), never `--set`. Each value of the key, several keys' in
//! their order, is its own deployment ([`Instance`]),
//! `app[env=prod,region=us-east1]`, with its own state directory under the
//! stack's (`dform.state/app/env=prod,region=us-east1/`), lock, registry
//! entry and controller; the other inputs are parameters of a deployment
//! and change it in place. `rekey` moves one deployment's state to another
//! key value. `provider name { source = "path" }` selects a provider: a
//! plugin executable, or a schema the mock provider plays; the block's
//! other settings configure it (`provider_config`, lowered by the
//! resolver).
//!
//! Cross-stack values: an apply records the stack's outputs in its state
//! and publishes them beside it, apart ([`Published`], `outputs.json`), and
//! records where the deployment's objects are in the registry `stacks.json`
//! under the state root (`dform.state/` at the project root); every other
//! program reads them as `stack_output(Stack, Key, Value)` facts. A
//! project reads another's through that one's backend (`[remotes]`,
//! [`remote_location`]): the same read of the same object.
//!
//! `role = "bootstrap"` marks the stack that creates what a controller runs
//! in: the controller refuses it. `handover` moves another stack's objects
//! to a new backend and records it in the registry, where every later run
//! finds it; `rekey` moves them to another key value. Both go through the
//! stores ([`crate::store::Store`]), so a directory and a bucket move alike.

use crate::ast::{Atom, Config, Program, Span, Stmt, Term};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::store::{Deployment, Location, OpenS3, S3Spec, Store};
use crate::value::Value;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The program's stack settings and `provider` statements.
#[derive(Debug, Clone, Default)]
pub struct Stack {
    /// The stack's name, when the manifest gave it settings (else the
    /// entry file's stem, `state::stack_name`).
    pub name: Option<String>,
    /// `backend = local("dir")`: where the state, the world and the lock
    /// live; `s3(...)`: the state, plan key, audit log and lease in a
    /// bucket. `None`: `dform.state/<name>/`.
    pub backend: Option<Backend>,
    /// `role = bootstrap`: the stack creates what a controller runs in. It
    /// stays batch: `dform controller run` refuses it.
    pub bootstrap: bool,
    /// Provider schemas, as `--provider` takes them: a name or a path.
    pub providers: Vec<String>,
    /// Each `provider NAME { .. }` block's spec (as in `providers`) ->
    /// NAME: the block's settings configure the provider it selects.
    pub provider_blocks: BTreeMap<String, String>,
    /// `key env: T`, `key region: T`: the inputs that key the stack, in
    /// order.
    pub keys: Vec<(String, Span)>,
    /// `isolated = true`: every key value deploys into its own account (or
    /// world), so a name that does not vary by key does not collide; the
    /// collision lint (`lint::key_collisions`) is off.
    pub isolated: bool,
    /// `approvals = jwks("https://...")` (or `jwks_file("path")`, or a
    /// list of them): whose signatures approve a plan (`approval`).
    pub approvals: Vec<crate::approval::TrustRoot>,
    /// `audit_sink = "CMD"`: each audit log entry is also piped to CMD
    /// (`audit`); `--audit-sink` overrides it.
    pub audit_sink: Option<String>,
}

/// A stack's backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `local("DIR")`: DIR relative to the project root.
    Local(PathBuf),
    /// `s3("BUCKET", "PREFIX", {endpoint: URL, region: R})`.
    S3(crate::store::S3Spec),
}

/// A `backend = ...` term.
fn backend(v: &Term) -> Result<Backend, String> {
    let Term::Func { name, args } = v else {
        return Err("unknown backend".into());
    };
    match (name.as_str(), args.as_slice()) {
        ("local", [dir]) => match dir.as_str() {
            Some(dir) => Ok(Backend::Local(PathBuf::from(dir))),
            None => Err("backend local(DIR) takes a directory string".into()),
        },
        ("s3", [bucket, prefix, rest @ ..]) if rest.len() <= 1 => {
            let usage = "backend s3(\"BUCKET\", \"PREFIX\", {endpoint: \"URL\", region: \"R\"}) \
                         takes a bucket and a prefix string, and optionally a record of \
                         endpoint and region strings";
            let (Some(bucket), Some(prefix)) = (bucket.as_str(), prefix.as_str()) else {
                return Err(usage.into());
            };
            if bucket.is_empty() {
                return Err("backend s3: the bucket is empty".into());
            }
            let mut spec = crate::store::S3Spec {
                bucket: bucket.to_string(),
                prefix: prefix.trim_matches('/').to_string(),
                endpoint: None,
                region: None,
            };
            if let Some(opts) = rest.first() {
                let Term::Obj(m) = opts else {
                    return Err(usage.into());
                };
                for (k, v) in m {
                    let v = v
                        .as_str()
                        .ok_or_else(|| format!("backend s3: {k} is a string"))?;
                    match k.as_str() {
                        "endpoint" => spec.endpoint = Some(v.trim_end_matches('/').to_string()),
                        "region" => spec.region = Some(v.to_string()),
                        other => {
                            return Err(format!(
                                "backend s3 has no option {other}; its options: endpoint, region"
                            ));
                        }
                    }
                }
            }
            Ok(Backend::S3(spec))
        }
        ("s3", _) => Err(
            "backend s3(\"BUCKET\", \"PREFIX\", {endpoint: \"URL\", region: \"R\"}) takes a \
             bucket, a prefix and optionally a record of options"
                .into(),
        ),
        _ => Err("unknown backend".into()),
    }
}

/// A backend as the manifest writes it, a term of the language in a
/// string.
pub fn parse_backend(text: &str) -> Result<Backend> {
    let t = crate::syntax::resolve::data_term(text)
        .map_err(|e| anyhow::anyhow!("not a backend term: {e}"))?;
    backend(&t).map_err(|e| anyhow::anyhow!(e))
}

/// The backend with each key's `{k}` its value in `key` (escaped, as
/// [`Instance::segment`] prints it), and whether it named any: a backend
/// that names the key is the deployment's own place, not its stack's
/// (`s3("acme", "shop/{env}")`).
pub fn keyed_backend(b: &Backend, key: &[(String, String)]) -> (Backend, bool) {
    let mut named = false;
    let mut sub = |s: &str| {
        let mut out = s.to_string();
        for (k, v) in key {
            let hole = format!("{{{k}}}");
            if out.contains(&hole) {
                named = true;
                out = out.replace(&hole, v);
            }
        }
        out
    };
    let b = match b {
        Backend::Local(dir) => Backend::Local(PathBuf::from(sub(&dir.to_string_lossy()))),
        Backend::S3(spec) => Backend::S3(S3Spec {
            prefix: sub(&spec.prefix),
            ..spec.clone()
        }),
    };
    (b, named)
}

/// A backend that names its keys (`local("state/app-{env}")`), cut at the
/// directory above its first `{k}`: that directory's backend and the rest
/// of the template (`app-{env}`), which [`template_key`] matches against
/// what is found under it. `None` for a backend that names no key.
pub fn keyed_parent(b: &Backend) -> Option<(Backend, String)> {
    let template = match b {
        Backend::Local(dir) => dir.to_string_lossy().into_owned(),
        Backend::S3(spec) => spec.prefix.clone(),
    };
    let hole = template.find('{')?;
    let cut = template[..hole].rfind('/').map_or(0, |j| j + 1);
    let (parent, rest) = template.split_at(cut);
    let parent = match b {
        Backend::Local(_) if parent.is_empty() => Backend::Local(PathBuf::from(".")),
        Backend::Local(_) => Backend::Local(PathBuf::from(parent)),
        Backend::S3(spec) => Backend::S3(S3Spec {
            prefix: parent.to_string(),
            ..spec.clone()
        }),
    };
    Some((parent, rest.to_string()))
}

/// The deployment segment (`env=prod`, keys in `keys`' order) whose place
/// holds `path`, an object found under [`keyed_parent`]'s directory, when
/// `rest` (its template) matches the path's leading directories.
pub fn template_key(rest: &str, keys: &[String], path: &str) -> Option<String> {
    let mut pattern = String::from("^");
    let mut names = Vec::new();
    let mut text = rest;
    while let Some(open) = text.find('{') {
        let close = text[open..].find('}')? + open;
        pattern.push_str(&regex::escape(&text[..open]));
        let k = &text[open + 1..close];
        if keys.iter().any(|x| x == k) {
            pattern.push_str("([^/]+)");
            names.push(k.to_string());
        } else {
            pattern.push_str(&regex::escape(&text[open..=close]));
        }
        text = &text[close + 1..];
    }
    pattern.push_str(&regex::escape(text));
    pattern.push('/');
    let caps = regex::Regex::new(&pattern).ok()?.captures(path)?;
    let found: BTreeMap<&str, &str> = names
        .iter()
        .zip(caps.iter().skip(1))
        .filter_map(|(k, c)| Some((k.as_str(), c?.as_str())))
        .collect();
    keys.iter()
        .map(|k| found.get(k.as_str()).map(|v| format!("{k}={v}")))
        .collect::<Option<Vec<_>>>()
        .map(|kv| kv.join(","))
}

/// Read the stack's settings, keys and `provider` statements of a loaded
/// program.
pub fn config(program: &Program) -> Result<Stack> {
    let mut out = Stack::default();
    let mut diags = Vec::new();
    let settings = program.stack.as_ref();
    for s in &program.statements {
        match s {
            Stmt::Input(i) if i.key => out.keys.push((i.name.clone(), i.span)),
            Stmt::Provider(c) => {
                // A built-in fact provider dform answers itself starts nothing.
                if crate::externs::builtin(&c.name).is_some_and(|b| b.in_process) {
                    continue;
                }
                let spec = provider(c, &mut diags);
                out.provider_blocks.insert(spec.clone(), c.name.clone());
                out.providers.push(spec);
            }
            _ => {}
        }
    }
    if let Some(c) = settings {
        out.name = Some(c.name.clone());
        stack_config(c, &mut out, &mut diags);
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// One deployment of a stack: the stack's name and, when it is keyed, the
/// value of each key input, in the header's order (as the value prints,
/// a string bare).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub stack: String,
    pub key: Vec<(String, String)>,
    /// The key inputs whose value is the input's default: the run named
    /// none.
    pub defaulted: Vec<String>,
}

impl Instance {
    /// `k1=v1,k2=v2`, each value escaped for a file name: the directory of
    /// the deployment under the stack's, and the part of its name in
    /// brackets. `None` for an unkeyed stack.
    pub fn segment(&self) -> Option<String> {
        if self.key.is_empty() {
            return None;
        }
        Some(
            self.key
                .iter()
                .map(|(k, v)| format!("{k}={}", escape(v)))
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    /// `app`, or `app[env=prod]`: what the registry, `stack_output/3`,
    /// `handover`, `taint` and the controller call the deployment.
    pub fn name(&self) -> String {
        match self.segment() {
            Some(seg) => format!("{}[{seg}]", self.stack),
            None => self.stack.clone(),
        }
    }

    /// What `plan` and `apply` print first: the name, and which key values
    /// are defaults, `pngu[env=dev] (env from its default)`.
    pub fn describe(&self) -> String {
        match self.defaulted.as_slice() {
            [] => self.name(),
            [k] => format!("{} ({k} from its default)", self.name()),
            ks => format!("{} ({} from their defaults)", self.name(), ks.join(", ")),
        }
    }

    /// The deployment's directory: `base` (the stack's directory) for an
    /// unkeyed stack, else `base/<segment>`.
    pub fn dir(&self, base: &Path) -> PathBuf {
        match self.segment() {
            Some(seg) => base.join(seg),
            None => base.to_path_buf(),
        }
    }
}

/// A key value as a file name: ASCII letters, digits, `-`, `_` and a `.`
/// that does not lead are kept; every other byte is `%XX`, so `=`, `,`,
/// `/` and `%` itself never appear raw and `.`/`..` are not directories.
pub fn escape(v: &str) -> String {
    let mut out = String::new();
    for (i, b) in v.bytes().enumerate() {
        let keep = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || (b == b'.' && i > 0);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The directory of the deployment named `name` (`app` or `app[env=prod]`,
/// as [`Instance::name`] prints it) under the state root.
pub fn instance_dir(root: &Path, name: &str) -> PathBuf {
    match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
        Some((stack, seg)) => root.join(stack).join(seg),
        None => root.join(name),
    }
}

/// The deployment a run is of: the stack, and each key's value as this
/// run gives it: the target (`set`, the `input(k, v)` facts), else an
/// input fact or an `--input-file` contribution of the program, else the
/// input's default (named in [`Instance::defaulted`]). A key with none is
/// an error naming the input.
pub fn instance(cfg: &Stack, stack: &str, program: &Program, set: &[Atom]) -> Result<Instance> {
    let (mut key, mut defaulted) = (Vec::new(), Vec::new());
    let mut diags = Vec::new();
    for (k, span) in &cfg.keys {
        let given = |a: &Atom| -> Option<Value> {
            match (a.pred.as_str(), a.args.as_slice()) {
                ("input", [Term::Val(Value::Str(x)), Term::Val(v)]) if x == k => Some(v.clone()),
                (
                    "arg",
                    [
                        Term::Val(Value::Str(t)),
                        Term::Val(Value::Str(scope)),
                        Term::Val(Value::Str(x)),
                        Term::Val(v),
                        _,
                    ],
                ) if t == crate::modules::INPUT && scope.is_empty() && x == k => Some(v.clone()),
                _ => None,
            }
        };
        let from_program = || {
            program.statements.iter().find_map(|s| match s {
                Stmt::Fact(a) => given(a),
                _ => None,
            })
        };
        let default = || {
            program.statements.iter().find_map(|s| match s {
                Stmt::Input(i) if &i.name == k => match &i.default {
                    Some(Term::Val(v)) => Some(v.clone()),
                    _ => None,
                },
                _ => None,
            })
        };
        let named = set.iter().rev().find_map(given).or_else(from_program);
        if named.is_none() && default().is_some() {
            defaulted.push(k.clone());
        }
        match named.or_else(default) {
            Some(v) => key.push((k.clone(), crate::partition::fmt_bare(&v))),
            None => diags.push(
                Diagnostic::error(
                    *span,
                    format!("stack {stack} is keyed by {k}, which has no value"),
                )
                .with_help(format!(
                    "give it with the target, `{stack} {k}=...`: each value of the key is \
                     its own deployment, with its own state"
                )),
            ),
        }
    }
    if !diags.is_empty() {
        return Err(Diagnostics(diags).into());
    }
    Ok(Instance {
        stack: stack.to_string(),
        key,
        defaulted,
    })
}

fn stack_config(c: &Config, out: &mut Stack, diags: &mut Vec<Diagnostic>) {
    for (k, v, span) in &c.config {
        match k.as_str() {
            "backend" => match backend(v) {
                Ok(b) => out.backend = Some(b),
                Err(e) if e == "unknown backend" => {
                    diags.push(Diagnostic::error(*span, e).with_help(
                        "the backends are `local(\"DIR\")` and \
                     `s3(\"BUCKET\", \"PREFIX\", {endpoint: \"URL\", region: \"R\"})`",
                    ))
                }
                Err(e) => diags.push(Diagnostic::error(*span, e)),
            },
            "role" => match v.as_str() {
                Some("bootstrap") => out.bootstrap = true,
                _ => diags.push(Diagnostic::error(*span, "role is \"bootstrap\"")),
            },
            "isolated" => match v {
                Term::Val(Value::Bool(b)) if !out.keys.is_empty() => out.isolated = *b,
                Term::Val(Value::Bool(_)) => diags.push(Diagnostic::error(
                    *span,
                    format!(
                        "isolated says each key value deploys apart; stack {} has no key",
                        c.name
                    ),
                )),
                _ => diags.push(Diagnostic::error(*span, "isolated is `true` or `false`")),
            },
            "approvals" => {
                let roots = match v {
                    Term::List(xs) => xs.iter().collect(),
                    one => vec![one],
                };
                for t in roots {
                    match trust_root(c.span, t) {
                        Some(r) => out.approvals.push(r),
                        None => diags.push(
                            Diagnostic::error(*span, "approvals is a JWKS trust root").with_help(
                                "`jwks(\"https://...\")` or `jwks_file(\"path\")`, each with \
                                     an optional issuer a JWT must name as a second argument; \
                                     or a list of them",
                            ),
                        ),
                    }
                }
            }
            "audit_sink" => match v.as_str() {
                Some(cmd) => out.audit_sink = Some(cmd.to_string()),
                None => diags.push(Diagnostic::error(
                    *span,
                    "audit_sink is a command string, run with `sh -c`",
                )),
            },
            other => diags.push(
                Diagnostic::error(*span, format!("stack {} has no setting {other}", c.name))
                    .with_note(format!(
                        "its settings: {}",
                        crate::project::STACK_SETTINGS.join(", ")
                    )),
            ),
        }
    }
}

/// `jwks("url")` or `jwks_file("path")` (from the project root of the
/// manifest that says it), each with an optional issuer.
fn trust_root(at: Span, t: &Term) -> Option<crate::approval::TrustRoot> {
    use crate::approval::{Jwks, TrustRoot};
    let Term::Func { name, args } = t else {
        return None;
    };
    let args: Vec<&str> = args.iter().map(Term::as_str).collect::<Option<_>>()?;
    let (src, issuer) = match args.as_slice() {
        [src] => (*src, None),
        [src, iss] => (*src, Some(iss.to_string())),
        _ => return None,
    };
    let jwks = match name.as_str() {
        "jwks" => Jwks::Url(src.to_string()),
        "jwks_file" => {
            let base = diag::location(at)
                .map(|(file, _, _)| crate::project::base_of(Path::new(&file)))
                .unwrap_or_default();
            Jwks::File(base.join(src))
        }
        _ => return None,
    };
    Some(TrustRoot { jwks, issuer })
}

/// A provider: `source = "path"`, from the project root of the file the
/// statement is in: an executable speaking the plugin protocol, a directory holding one
/// (`dform-provider*`), else a schema the mock plays (a directory holding
/// `schema.df`, or a `.df` file); without `source`, its name
/// (`plugin::source::resolve`).
fn provider(c: &Config, diags: &mut Vec<Diagnostic>) -> String {
    let mut spec = c.name.clone();
    for (k, v, span) in &c.config {
        match (k.as_str(), v.as_str()) {
            ("source", Some(src)) => {
                let base = diag::location(c.span)
                    .map(|(file, _, _)| crate::project::base_of(Path::new(&file)))
                    .unwrap_or_default();
                let mut path = base.join(src);
                if path.is_dir() {
                    path = crate::plugin::source::plugin_in(&path)
                        .unwrap_or_else(|| path.join("schema.df"));
                }
                spec = path.display().to_string();
                if !spec.contains('/') {
                    spec = format!("./{spec}");
                }
            }
            ("source", None) => diags.push(Diagnostic::error(*span, "source is a path string")),
            (other, _) => diags.push(
                Diagnostic::error(*span, format!("provider {} has no setting {other}", c.name))
                    .with_note("its settings: source"),
            ),
        }
    }
    spec
}

/// The registry of applied stacks: name -> state file, under the state root.
fn registry_path(root: &Path) -> PathBuf {
    root.join("stacks.json")
}

/// A registered stack: where its state is, whether it is a bootstrap stack,
/// and the backend it was handed over to (`handover`). An entry with
/// neither, of local state, is written as the bare state path; state in a
/// bucket is `s3://BUCKET/PREFIX/state.json` with the endpoint and region.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Written {
    Path(PathBuf),
    Entry {
        state: PathBuf,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        bootstrap: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Where the deployment's objects are.
    pub state: Location,
    pub bootstrap: bool,
    /// `k8s("ns/name")`, `local("dir")` or `s3(...)`: the stack was handed
    /// over there and its state is `state`, wherever its program's backend
    /// says.
    pub backend: Option<String>,
}

/// `s3://BUCKET/PREFIX/state.json` as a location.
fn s3_location(state: &str, endpoint: Option<String>, region: Option<String>) -> Option<Location> {
    let rest = state.strip_prefix("s3://")?;
    let (bucket, key) = rest.split_once('/')?;
    let prefix = match key.strip_suffix(crate::store::STATE)? {
        "" => "",
        p => p.strip_suffix('/')?,
    };
    Some(Location::S3(S3Spec {
        bucket: bucket.to_string(),
        prefix: prefix.to_string(),
        endpoint,
        region,
    }))
}

impl From<Written> for Entry {
    fn from(w: Written) -> Entry {
        let (state, bootstrap, backend, endpoint, region) = match w {
            Written::Path(state) => (state, false, None, None, None),
            Written::Entry {
                state,
                bootstrap,
                backend,
                endpoint,
                region,
            } => (state, bootstrap, backend, endpoint, region),
        };
        let text = state.to_string_lossy();
        let state = s3_location(&text, endpoint, region).unwrap_or_else(|| {
            Location::Local(state.parent().unwrap_or(Path::new("")).to_path_buf())
        });
        Entry {
            state,
            bootstrap,
            backend,
        }
    }
}

impl From<Entry> for Written {
    fn from(e: Entry) -> Written {
        match e.state {
            Location::Local(dir) if !e.bootstrap && e.backend.is_none() => {
                Written::Path(dir.join(crate::store::STATE))
            }
            Location::Local(dir) => Written::Entry {
                state: dir.join(crate::store::STATE),
                bootstrap: e.bootstrap,
                backend: e.backend,
                endpoint: None,
                region: None,
            },
            Location::S3(spec) => Written::Entry {
                state: PathBuf::from(match spec.prefix.as_str() {
                    "" => format!("s3://{}/{}", spec.bucket, crate::store::STATE),
                    p => format!("s3://{}/{p}/{}", spec.bucket, crate::store::STATE),
                }),
                bootstrap: e.bootstrap,
                backend: e.backend,
                endpoint: spec.endpoint,
                region: spec.region,
            },
        }
    }
}

pub fn registry(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let path = registry_path(root);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let r: BTreeMap<String, Written> =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(r.into_iter().map(|(k, w)| (k, w.into())).collect())
}

fn save_registry(root: &Path, r: BTreeMap<String, Entry>) -> Result<()> {
    let r: BTreeMap<String, Written> = r.into_iter().map(|(k, e)| (k, e.into())).collect();
    fs::create_dir_all(root).with_context(|| format!("mkdir {}", root.display()))?;
    let path = registry_path(root);
    fs::write(&path, serde_json::to_vec_pretty(&r)?)
        .with_context(|| format!("write {}", path.display()))
}

/// A location as the registry keeps it: a directory absolute.
fn absolute(loc: &Location) -> Result<Location> {
    Ok(match loc {
        Location::Local(dir) => Location::Local(
            fs::canonicalize(dir)
                .or_else(|_| std::path::absolute(dir))
                .with_context(|| format!("absolute path of {}", dir.display()))?,
        ),
        s3 => s3.clone(),
    })
}

/// Record that `stack`'s state is at `state`, so other stacks can read its
/// outputs (and `handover` can find a bootstrap stack). A handover's
/// backend is kept.
pub fn register(root: &Path, stack: &str, state: &Location, bootstrap: bool) -> Result<()> {
    let mut r = registry(root)?;
    let backend = r.get(stack).and_then(|e| e.backend.clone());
    let entry = Entry {
        state: absolute(state)?,
        bootstrap,
        backend,
    };
    if r.get(stack) == Some(&entry) {
        return Ok(());
    }
    r.insert(stack.to_string(), entry);
    save_registry(root, r)
}

/// The backend a stack was handed over to, and where its state now is.
pub fn handed_over(root: &Path, stack: &str) -> Result<Option<(String, Location)>> {
    Ok(registry(root)?
        .remove(stack)
        .and_then(|e| Some((e.backend?, e.state))))
}

/// Where the mock's world file of a deployment at `loc` is: in its
/// directory, or, for state in a bucket, in its default directory under
/// the state root (`home`): the world is the provider's, not state.
pub fn world_file(loc: &Location, home: &Path) -> PathBuf {
    match loc {
        Location::Local(dir) => dir.join(crate::state::WORLD),
        Location::S3(_) => home.join(crate::state::WORLD),
    }
}

/// One deployment's place for a move: its location, and its world file.
pub struct Place {
    pub location: Location,
    pub world: PathBuf,
}

/// Move a deployment's own objects (`store::own_key`) from `from` to `to`
/// under the source's lock, and its world file with them when it lives
/// elsewhere there: `commit` runs (the registry) once every object is
/// copied, then the source's are deleted. The target must hold none of a
/// deployment's objects; every copy is conditional on there being none.
fn transfer(
    what: &str,
    from: &Place,
    to: &Place,
    s3: OpenS3,
    times: crate::store::LeaseTimes,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use crate::store::{Cond, LOCK, own_key};
    let (src, dst) = (from.location.open(s3)?, to.location.open(s3)?);
    let lease_free = |k: &str, store: &dyn Store| -> Result<bool> {
        // A released lease is no deployment's.
        if k != LOCK || !store.fenced() {
            return Ok(false);
        }
        let Some(o) = store.get(k)? else {
            return Ok(true);
        };
        Ok(
            serde_json::from_slice::<crate::store::LeaseRecord>(&o.bytes)
                .is_ok_and(|r| r.holder.is_empty()),
        )
    };
    let mut there = Vec::new();
    for k in dst.list("")? {
        if own_key(&k) && !lease_free(&k, dst.as_ref())? {
            there.push(k);
        }
    }
    if to.world != from.world && to.world.exists() {
        there.push(to.world.display().to_string());
    }
    if let Some(k) = there.first() {
        bail!("{what}: {} is not empty: it holds {k}", to.location);
    }
    let lock = Deployment::new(src.clone(), what, times).lock()?;
    let keys: Vec<String> = src
        .list("")?
        .into_iter()
        .filter(|k| own_key(k) && k != LOCK)
        .collect();
    for k in &keys {
        let Some(o) = src.get(k)? else { continue };
        if dst.put(k, &o.bytes, &Cond::IfAbsent)?.is_none() {
            bail!(
                "{what}: {} appeared while the state was being moved; nothing was removed from {}",
                dst.locate(k),
                from.location
            );
        }
    }
    if let Location::Local(dir) = &to.location {
        fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    if to.world != from.world && from.world.exists() {
        if let Some(parent) = to.world.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
        fs::rename(&from.world, &to.world)
            .with_context(|| format!("move {} to {}", from.world.display(), to.world.display()))?;
    }
    commit()?;
    for k in &keys {
        src.delete(k)?;
    }
    drop(lock);
    if src.fenced() {
        src.delete(LOCK)?;
    }
    // A directory left empty goes (its drop directory first).
    for dir in [&from.location, &Location::Local(from.world.clone())] {
        if let Location::Local(d) = dir {
            let d = if d.ends_with(crate::state::WORLD) {
                d.parent().unwrap_or(Path::new("")).to_path_buf()
            } else {
                d.clone()
            };
            let _ = fs::remove_dir(d.join(crate::store::DROPS));
            let _ = fs::remove_dir(&d);
        }
    }
    Ok(())
}

/// `dform stack handover NAME --to BACKEND`: move the deployment's objects
/// (state, plan key, audit log, published outputs, the controller's memo)
/// from `from` to the backend and record it in the registry; every later
/// run of the stack uses it. `local("DIR")` is a directory, relative to
/// the one holding `root`; `s3("BUCKET", "PREFIX", {..})` the deployment's
/// own prefix; `k8s("ns/name")` stands in for the in-cluster backend: a
/// directory `k8s/ns/name` inside the state directory of the registered
/// bootstrap stack (the one that owns the cluster). `home` is the
/// deployment's default directory, where its world is when its state is in
/// a bucket. Returns the new location.
pub fn handover(
    root: &Path,
    stack: &str,
    from: &Place,
    home: &Path,
    to: &str,
    s3: OpenS3,
    times: crate::store::LeaseTimes,
) -> Result<Location> {
    let reg = registry(root)?;
    let target = match parse_target(to)? {
        Target::Local(dir) => Location::Local(crate::state::local_dir(root, &dir)),
        Target::S3(spec) => Location::S3(spec),
        Target::K8s(key) => {
            let boots: Vec<(&String, &Entry)> = reg.iter().filter(|(_, e)| e.bootstrap).collect();
            let (boot, e) = match boots.as_slice() {
                [one] => *one,
                [] => bail!(
                    "handover {stack} to {to}: no bootstrap stack is registered to own the \
                     cluster; apply the stack with `role = bootstrap` first"
                ),
                many => bail!(
                    "handover {stack} to {to}: {} bootstrap stacks are registered ({}); \
                     which cluster is meant is ambiguous",
                    many.len(),
                    many.iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            if boot == stack {
                bail!("handover {stack}: a bootstrap stack stays batch and is never handed over");
            }
            let Location::Local(dir) = &e.state else {
                bail!(
                    "handover {stack} to {to}: the bootstrap stack {boot}'s state is in {}; \
                     k8s(..) stands in as a directory beside local state only",
                    e.state
                );
            };
            Location::Local(dir.join("k8s").join(key))
        }
    };
    if reg.get(stack).is_some_and(|e| e.bootstrap) {
        bail!("handover {stack}: a bootstrap stack stays batch and is never handed over");
    }
    if target == from.location {
        bail!("handover {stack} to {to}: its state is there already");
    }
    let to_place = Place {
        world: world_file(&target, home),
        location: target.clone(),
    };
    let what = format!("handover {stack} to {to}");
    let mut saved = None;
    transfer(&what, from, &to_place, s3, times, || {
        let mut reg = reg;
        let state = absolute(&target)?;
        reg.insert(
            stack.to_string(),
            Entry {
                state: state.clone(),
                bootstrap: false,
                backend: Some(to.to_string()),
            },
        );
        saved = Some(state);
        save_registry(root, reg)
    })?;
    Ok(saved.unwrap_or(target))
}

/// `dform stack rekey`: move the objects of deployment `from` to
/// deployment `to`, and its registry entry with it. Nothing in the cloud
/// changes. An unkeyed `from` is the state the stack had before it was
/// keyed: the objects directly in the stack's location. Returns the new
/// location.
pub fn rekey(
    root: &Path,
    (from, from_place): (&Instance, &Place),
    (to, to_place): (&Instance, &Place),
    s3: OpenS3,
    times: crate::store::LeaseTimes,
) -> Result<Location> {
    let (old, new) = (from.name(), to.name());
    let reg = registry(root)?;
    if let Some(b) = reg.get(&old).and_then(|e| e.backend.clone()) {
        bail!(
            "rekey {old}: it was handed over to {b}; its state is not at {}",
            from_place.location
        );
    }
    let store = from_place.location.open(s3)?;
    if store.get(crate::store::STATE)?.is_none() {
        bail!(
            "rekey {old}: no state at {}",
            store.locate(crate::store::STATE)
        );
    }
    let what = format!("rekey {old} to {new}");
    transfer(&what, from_place, to_place, s3, times, || {
        let mut reg = reg;
        if let Some(e) = reg.remove(&old) {
            reg.insert(
                new.clone(),
                Entry {
                    state: absolute(&to_place.location)?,
                    bootstrap: e.bootstrap,
                    backend: None,
                },
            );
            save_registry(root, reg)?;
        }
        Ok(())
    })?;
    absolute(&to_place.location)
}

enum Target {
    Local(PathBuf),
    K8s(String),
    S3(S3Spec),
}

/// `local("DIR")`, `k8s("ns/name")` or `s3("BUCKET", "PREFIX", {..})`, as
/// the command line gives it.
fn parse_target(to: &str) -> Result<Target> {
    let bad = || {
        anyhow::anyhow!(
            "unknown backend {to}: the backends are local(\"DIR\"), k8s(\"ns/name\") and \
             s3(\"BUCKET\", \"PREFIX\", {{endpoint: \"URL\", region: \"R\"}})"
        )
    };
    let (kind, rest) = to.split_once('(').ok_or_else(bad)?;
    if kind.trim() == "s3" {
        return match parse_backend(to) {
            Ok(Backend::S3(spec)) => Ok(Target::S3(spec)),
            Ok(_) => Err(bad()),
            Err(_) => match backend_term(to) {
                Some(e) => bail!("{to}: {e}"),
                None => Err(bad()),
            },
        };
    }
    let arg = rest
        .strip_suffix(')')
        .map(|a| a.trim().trim_matches('"'))
        .filter(|a| !a.is_empty())
        .ok_or_else(bad)?;
    match kind.trim() {
        "local" => Ok(Target::Local(PathBuf::from(arg))),
        "k8s" => {
            let parts: Vec<&str> = arg.split('/').collect();
            let ok = |p: &str| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            };
            match parts.as_slice() {
                [ns, name] if ok(ns) && ok(name) => Ok(Target::K8s(format!("{ns}/{name}"))),
                _ => bail!("k8s(\"{arg}\"): the key is \"namespace/name\""),
            }
        }
        _ => Err(bad()),
    }
}

/// What a backend term that parses says is wrong with it, if anything.
fn backend_term(text: &str) -> Option<String> {
    let t = crate::syntax::resolve::data_term(text).ok()?;
    backend(&t).err()
}

/// A deployment's published outputs (`store::OUTPUTS`, beside its state and
/// apart from it): what other stacks read as `stack_output/3`, needing
/// read access to this object only. A secret output crosses as its label
/// and where a provider holds it, never its value: the reader's static
/// pass treats it as secret (E0304 where it reaches a public place), its
/// value is a secret null no plan resolves, and the reader's provider
/// reads it where it is held inside Apply (E DR-19). An output whose value
/// is not known yet is pending: the reader has an open null.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Published {
    /// The deployment, as its own project names it.
    pub deployment: String,
    #[serde(default)]
    pub outputs: BTreeMap<String, Value>,
    /// The public outputs not known yet.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pending: BTreeSet<String>,
    /// The outputs declared `secret(T)`; one not known yet has neither a
    /// digest nor a place it is held.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret: BTreeMap<String, SecretOutput>,
}

/// A secret output as state records it and `outputs.json` publishes it:
/// its label (`output/#K`), T's name, and the keyed digest of its value
/// (the deployment's plan key, `hmac-sha256:..`) when the run knew the
/// value; and where a provider holds it: the object whose attribute it is.
/// A secret the program has only as bytes (an input's) is held nowhere, and
/// no other stack can use it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SecretOutput {
    pub label: String,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<crate::provider::Held>,
}

impl SecretOutput {
    /// Not known yet: neither its value nor where it is held.
    pub fn pending(&self) -> bool {
        self.digest.is_empty() && self.held.is_none()
    }
}

impl Published {
    pub fn new(deployment: &str, outputs: &Outputs) -> Published {
        Published {
            deployment: deployment.to_string(),
            outputs: outputs.known.clone(),
            pending: outputs.pending.clone(),
            secret: outputs.secret.clone(),
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut b = serde_json::to_vec_pretty(self).expect("published outputs serialize");
        b.push(b'\n');
        b
    }

    /// `stack_output(Name, Key, Value)`, `Name` as the reader names the
    /// deployment; a secret one's value a secret null, a pending one's an
    /// open null, each labeled `stack_output/Name#Key`.
    fn facts(&self, name: &str) -> Vec<Atom> {
        let fact = |k: &str, v: Value| Atom {
            pred: "stack_output".into(),
            args: vec![
                Term::Val(Value::Str(name.to_string())),
                Term::Val(Value::Str(k.to_string())),
                Term::Val(v),
            ],
            record: None,
            span: Span::default(),
        };
        let null = |k: &str, class, ty: &str| Value::Null {
            label: output_label(name, k),
            class,
            ty: ty.to_string(),
        };
        let mut out: Vec<Atom> = self
            .outputs
            .iter()
            .map(|(k, v)| fact(k, v.clone()))
            .collect();
        for k in &self.pending {
            out.push(fact(k, null(k, crate::value::NullClass::Open, "")));
        }
        for (k, o) in &self.secret {
            let class = match o.pending() {
                true => crate::value::NullClass::Open,
                false => crate::value::NullClass::Secret,
            };
            out.push(fact(k, null(k, class, &o.ty)));
        }
        out
    }
}

/// The label of the null a reader has for the output `k` of the deployment
/// it names `name`.
fn output_label(name: &str, k: &str) -> String {
    crate::value::null_label("stack_output", name, k)
}

/// The stack's outputs after an apply (`output k = t` at the top), as its
/// state records them and `outputs.json` publishes them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outputs {
    /// The public outputs whose value is known; a ref to a configured
    /// attribute resolved to the program's value, else the world's.
    pub known: BTreeMap<String, Value>,
    /// The public outputs whose value is not known yet.
    pub pending: BTreeSet<String>,
    pub secret: BTreeMap<String, SecretOutput>,
}

impl Outputs {
    pub fn is_empty(&self) -> bool {
        self.known.is_empty() && self.pending.is_empty() && self.secret.is_empty()
    }
}

/// The stack's outputs in an evaluation (`facts`: `attr(output, "", k,
/// V)`), after an apply whose state is `state` and whose world's
/// configured attributes are `world` (by address). A ref to a configured
/// attribute (E DR-2 as amended keeps it a ref) is resolved to the
/// program's value of it, else the world's; one neither knows, and a null,
/// is pending. A secret output (`secret`: key -> T's name) records no
/// value: its label, its value's keyed digest (`digest`, the deployment's
/// plan key) when the run knows the value, and where a provider holds it
/// when it is a resource's attribute (a ref, or a sensitive computed
/// value's null): the object `state` maps, managed for `deployment`.
pub fn outputs(
    facts: &BTreeSet<Atom>,
    secret: &BTreeMap<String, String>,
    world: &BTreeMap<crate::ir::Address, serde_json::Value>,
    state: &crate::state::State,
    deployment: &str,
    digest: &dyn Fn(&[u8]) -> String,
) -> Outputs {
    let attrs: BTreeMap<(&str, &str, &str), &Value> = facts
        .iter()
        .filter(|a| a.pred == "attr")
        .filter_map(|a| match a.args.as_slice() {
            [
                Term::Val(Value::Str(t)),
                Term::Val(Value::Str(n)),
                Term::Val(Value::Str(p)),
                Term::Val(v),
            ] => Some(((t.as_str(), n.as_str(), p.as_str()), v)),
            _ => None,
        })
        .collect();
    let resolver = Resolver {
        attrs: &attrs,
        world,
    };
    let mut out = Outputs::default();
    for ((t, scope, k), v) in &attrs {
        if *t != crate::transform::OUTPUT || !scope.is_empty() {
            continue;
        }
        let known = resolver.resolve(v, 0);
        let Some(ty) = secret.get(*k) else {
            match known {
                Some(v) => {
                    out.known.insert(k.to_string(), v);
                }
                None => {
                    out.pending.insert(k.to_string());
                }
            }
            continue;
        };
        let at = match v {
            Value::Ref { typ, name, attr } => Some((typ.clone(), name.clone(), attr.clone())),
            Value::Null {
                label,
                class: crate::value::NullClass::Secret,
                ..
            } => crate::value::null_owner(label)
                .zip(label.split_once('#').map(|(_, p)| p.to_string()))
                .map(|((t, n), p)| (t, n, p)),
            _ => None,
        };
        let held = at.and_then(|(typ, name, path)| {
            let addr = crate::ir::Address {
                typ: typ.clone(),
                name: name.clone(),
            };
            // A provider's label names the object by its remote id.
            let e = state.get(&addr).or_else(|| {
                state.resources.iter().find_map(|(k, e)| {
                    let a = crate::state::parse_key(k)?;
                    (a.typ == typ && e.remote == name).then_some(e)
                })
            })?;
            Some(crate::provider::Held {
                provider: e.provider.clone(),
                deployment: deployment.to_string(),
                typ,
                remote: e.remote.clone(),
                path,
                digest: String::new(),
            })
        });
        let digest = known
            .map(|v| {
                let j = serde_json::to_value(&v).expect("a value serializes");
                format!(
                    "hmac-sha256:{}",
                    digest(crate::approval::canonical_json(&j).as_bytes())
                )
            })
            .unwrap_or_default();
        out.secret.insert(
            k.to_string(),
            SecretOutput {
                label: crate::value::null_label(crate::transform::OUTPUT, "", k),
                ty: ty.clone(),
                held,
                digest,
            },
        );
    }
    out
}

/// A value with its refs to configured attributes resolved: `None` while
/// any part of it is unknown.
struct Resolver<'a> {
    attrs: &'a BTreeMap<(&'a str, &'a str, &'a str), &'a Value>,
    world: &'a BTreeMap<crate::ir::Address, serde_json::Value>,
}

impl Resolver<'_> {
    fn resolve(&self, v: &Value, depth: usize) -> Option<Value> {
        // A chain of refs longer than this is a cycle.
        if depth > 32 {
            return None;
        }
        match v {
            Value::Null { .. } => None,
            Value::List(xs) => xs
                .iter()
                .map(|x| self.resolve(x, depth))
                .collect::<Option<_>>()
                .map(Value::List),
            Value::Obj(m) => m
                .iter()
                .map(|(k, x)| Some((k.clone(), self.resolve(x, depth)?)))
                .collect::<Option<_>>()
                .map(Value::Obj),
            Value::Ref { typ, name, attr } => {
                match self
                    .attrs
                    .get(&(typ.as_str(), name.as_str(), attr.as_str()))
                {
                    Some(v) => self.resolve(v, depth + 1),
                    None => {
                        let addr = crate::ir::Address {
                            typ: typ.clone(),
                            name: name.clone(),
                        };
                        let v = crate::provider::get_path(self.world.get(&addr)?, attr)?;
                        crate::provider::marker(v)
                            .is_none()
                            .then(|| crate::provider::json_to_value(v))
                    }
                }
            }
            v => Some(v.clone()),
        }
    }
}

/// Where the reader's provider finds each secret output it reads that a
/// provider holds, by the label of the reader's null
/// (`stack_output/NAME#K`); the held object's deployment as the reader
/// names it.
pub fn held(read: &[Read]) -> BTreeMap<String, crate::provider::Held> {
    let mut out = BTreeMap::new();
    for r in read {
        let Some(p) = &r.published else { continue };
        for (k, o) in &p.secret {
            if let Some(h) = &o.held {
                let h = crate::provider::Held {
                    deployment: r.name.clone(),
                    digest: o.digest.clone(),
                    ..h.clone()
                };
                out.insert(output_label(&r.name, k), h);
            }
        }
    }
    out
}

/// The outputs a program declares `secret(T)`, with T's name (the stack's
/// own, not a module's).
pub fn secret_output_types(program: &Program) -> BTreeMap<String, String> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Output(o) => match &o.ty {
                Some(crate::ast::TypeExpr::Apply(n, args)) if n == "secret" => {
                    let ty = match args.as_slice() {
                        [crate::ast::TypeExpr::Name(t)] => t.clone(),
                        _ => String::new(),
                    };
                    Some((o.name.clone(), ty))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// One deployment's outputs as a run read them.
#[derive(Debug, Clone)]
pub struct Read {
    /// The deployment as the reader names it: `app[env=prod]`, or
    /// `platform.cluster[env=prod]` of the remote `platform`.
    pub name: String,
    /// The object's digest, `absent` when there is none.
    pub digest: String,
    pub published: Option<Published>,
    /// The mock's world of the deployment, when it is known (a directory's,
    /// or the project's own bucket deployment's): where the reader's mock
    /// finds a secret the deployment's objects hold.
    pub world: Option<PathBuf>,
}

/// The digest of an outputs object as a plan records it.
fn outputs_digest(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(b) => format!("sha256:{}", crate::approval::sha256_hex(b)),
        None => "absent".into(),
    }
}

/// Read the outputs a deployment published at `loc`; `name` is the
/// reader's name of it, `own` the project's (checked against the object).
pub fn read_published(loc: &Location, s3: OpenS3, name: &str, own: &str) -> Result<Read> {
    let store = loc.open(s3)?;
    let at = store.locate(crate::store::OUTPUTS);
    let Some(o) = store.get(crate::store::OUTPUTS)? else {
        return Ok(Read {
            name: name.to_string(),
            digest: outputs_digest(None),
            published: None,
            world: None,
        });
    };
    let p: Published =
        serde_json::from_slice(&o.bytes).with_context(|| format!("parse the outputs {at}"))?;
    if p.deployment != own {
        bail!(
            "stack_output(\"{name}\", ..): {at} holds the outputs of {}, not {own}; check the \
             backend it was read through",
            p.deployment
        );
    }
    Ok(Read {
        name: name.to_string(),
        digest: outputs_digest(Some(&o.bytes)),
        published: Some(p),
        world: match loc {
            Location::Local(dir) => Some(dir.join(crate::state::WORLD)),
            Location::S3(_) => None,
        },
    })
}

/// Where a remote project's deployment `name` (`platform.cluster[env=prod]`)
/// is, from `[remotes] platform = { backend = "..." }` (`remotes`, name ->
/// backend term). The term's `{stack}` is the stack's name (`cluster`);
/// without one the stacks are under it by name, as under a state root. A
/// local directory is relative to the project root `project`. `None`: no
/// remote is named so.
pub fn remote_location(
    name: &str,
    remotes: &BTreeMap<String, String>,
    project: &Path,
) -> Result<Option<(Location, String)>> {
    let (base, seg) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
        Some((b, s)) => (b, Some(s)),
        None => (name, None),
    };
    let Some((remote, stack)) = base.split_once('.') else {
        return Ok(None);
    };
    let Some(term) = remotes.get(remote) else {
        return Ok(None);
    };
    let templated = term.contains("{stack}");
    let b = parse_backend(&term.replace("{stack}", stack))
        .with_context(|| format!("[remotes] {remote}: backend {term}"))?;
    let loc = match b {
        Backend::Local(dir) => Location::Local(project.join(dir)),
        Backend::S3(spec) => Location::S3(spec),
    };
    let loc = if templated {
        loc
    } else {
        loc.child(Some(stack))
    };
    let own = match seg {
        Some(s) => format!("{stack}[{s}]"),
        None => stack.to_string(),
    };
    Ok(Some((loc.child(seg), own)))
}

/// The deployments a program names in `stack_output(Name, ..)` with a
/// constant name, and whether one names it otherwise (a variable: it may
/// read any).
pub fn named_outputs(program: &Program) -> (std::collections::BTreeSet<String>, bool) {
    let mut named = std::collections::BTreeSet::new();
    let mut any = false;
    let mut see = |a: &Atom| {
        if a.pred == "stack_output" {
            match a.args.first() {
                Some(Term::Val(Value::Str(n))) => {
                    named.insert(n.clone());
                }
                _ => any = true,
            }
        }
    };
    for s in &program.statements {
        let body = match s {
            Stmt::Rule(r) => &r.body[..],
            _ => continue,
        };
        for l in body {
            if let crate::ast::Lit::Pos(a) | crate::ast::Lit::Not(a) = l {
                see(a);
            }
        }
    }
    (named, any)
}

/// The deployments of other stacks the deployment keyed by `key` reads
/// (`stack_output(Name, ..)`): a name written out, or interpolated from
/// the stack's own key inputs (`"platform[env=${env}]"`). A name built
/// from anything else is not known before the program runs, and is left
/// out. What `apply` applies first (R-30).
pub fn reads(program: &Program, key: &[(String, String)]) -> BTreeSet<String> {
    use crate::ast::Lit;
    fn name(t: &Term, body: &[Lit], key: &[(String, String)]) -> Option<String> {
        match t {
            Term::Val(Value::Str(s)) => Some(s.clone()),
            // A key input `k` is the relation `k(V)`.
            Term::Var(x) => body.iter().find_map(|l| match l {
                Lit::Pos(a) if matches!(a.args.as_slice(), [Term::Var(y)] if y == x) => key
                    .iter()
                    .find(|(k, _)| *k == a.pred)
                    .map(|(_, v)| v.clone()),
                _ => None,
            }),
            Term::Func { name: f, args } if f == "format" => {
                let (Some(Term::Val(Value::Str(fmt))), rest) = (args.first(), &args[1..]) else {
                    return None;
                };
                let mut parts = fmt.split("%s");
                let mut out = parts.next()?.to_string();
                for (part, arg) in parts.zip(rest) {
                    out.push_str(&name(arg, body, key)?);
                    out.push_str(part);
                }
                Some(out)
            }
            _ => None,
        }
    }
    let mut out = BTreeSet::new();
    for s in &program.statements {
        let body: &[Lit] = match s {
            Stmt::Rule(r) => &r.body,
            Stmt::Resource(r) => r.body.as_deref().unwrap_or_default(),
            _ => continue,
        };
        for l in body {
            if let Lit::Pos(a) | Lit::Not(a) = l
                && a.pred == "stack_output"
                && let Some(n) = a.args.first().and_then(|t| name(t, body, key))
            {
                out.insert(n);
            }
        }
    }
    out
}

/// What a run reads of other stacks' outputs: each deployment the program
/// names (`named`; `None`: it names one by a variable, and every
/// registered deployment of the project is read), but its own (`own`): a
/// registered one of the project where the registry has it, else one of a
/// remote project through the remote's backend. A name neither has has no
/// outputs yet.
pub fn stack_outputs(
    root: &Path,
    own: &str,
    named: Option<&std::collections::BTreeSet<String>>,
    remotes: &BTreeMap<String, String>,
    s3: OpenS3,
) -> Result<Vec<Read>> {
    let mut out = Vec::new();
    let reg = registry(root)?;
    for (name, e) in &reg {
        if name == own || named.is_some_and(|n| !n.contains(name)) {
            continue;
        }
        let mut r = read_published(&e.state, s3, name, name)?;
        // A bucket's deployment keeps its world in its directory under the
        // state root.
        if r.world.is_none() {
            let (stack, seg) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((b, s)) => (b, Some(s)),
                None => (name.as_str(), None),
            };
            let home = root.join(stack);
            r.world = Some(world_file(
                &e.state,
                &seg.map_or(home.clone(), |s| home.join(s)),
            ));
        }
        out.push(r);
    }
    let project = root.parent().unwrap_or(Path::new(""));
    for name in named.into_iter().flatten() {
        if name == own || reg.contains_key(name) {
            continue;
        }
        if let Some((loc, theirs)) = remote_location(name, remotes, project)? {
            out.push(read_published(&loc, s3, name, &theirs)?);
        }
    }
    Ok(out)
}

/// The `stack_output` facts of what was read.
pub fn output_facts(read: &[Read]) -> Vec<Atom> {
    read.iter()
        .filter_map(|r| Some(r.published.as_ref()?.facts(&r.name)))
        .flatten()
        .collect()
}

/// The secret outputs among what was read: (reader's name, key).
pub fn secret_outputs(read: &[Read]) -> std::collections::BTreeSet<(String, String)> {
    read.iter()
        .filter_map(|r| Some((r, r.published.as_ref()?)))
        .flat_map(|(r, p)| p.secret.keys().map(|k| (r.name.clone(), k.clone())))
        .collect()
}

/// Does the program give the stack an output (`output k = t` at the top)?
pub fn has_outputs(facts: &std::collections::BTreeSet<crate::ast::Atom>) -> bool {
    facts.iter().any(|a| {
        a.pred == "attr"
            && matches!(a.args.as_slice(), [Term::Val(Value::Str(t)), Term::Val(Value::Str(scope)), ..]
                if t == crate::transform::OUTPUT && scope.is_empty())
    })
}
