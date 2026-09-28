//! Stacks (DESIGN.org L6, E §7.1): `stack name { backend = local("dir"),
//! unknowns = strict | permissive, role = bootstrap }.` One program owns
//! one stack. The name scopes the state (the entry file's basename when
//! there is no `stack` statement), the backend is the directory it lives in
//! (or a bucket, `store`), and a lock there makes a second concurrent apply
//! fail cleanly.
//!
//! Keyed stacks: `stack app[env, region] { .. }` names the inputs that are
//! deployment identity. Each value of the key is its own deployment
//! ([`Instance`]), `app[env=prod,region=us-east1]`, with its own state
//! directory under the stack's (`dform.state/app/env=prod,region=us-east1/`),
//! lock, registry entry and controller; the other inputs are parameters of
//! a deployment and change it in place. `rekey` moves one deployment's
//! state to another key value. `provider name {
//! source = "path" }.` selects a provider: a plugin executable, or a schema
//! the mock provider plays; the block's other settings configure it
//! (`provider_config`, lowered by the resolver).
//!
//! Cross-stack values: an apply records the stack's outputs in its state
//! and the stack's absolute state path in the registry `stacks.json` under
//! the state root (`dform.state/` at the project root); every
//! other program reads them as `stack_output(Stack, Key, Value)` facts, a
//! fact provider over local state.
//!
//! `role = bootstrap` marks the stack that creates what a controller runs
//! in: the controller refuses it. `handover` moves another stack's state to
//! a new backend and records it in the registry, where every later run
//! finds it.

use crate::ast::{Atom, Config, Program, Span, Stmt, Term};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::value::Value;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// What an unknown may do at plan time (DESIGN.org "Strict mode is
/// first-class").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unknowns {
    /// Stuck derivations and pending groups are reported and wait for a
    /// phase boundary.
    #[default]
    Permissive,
    /// A stuck derivation or a pending group is a plan error; fresh nulls
    /// still flow.
    Strict,
}

/// The program's `stack` and `provider` statements.
#[derive(Debug, Clone, Default)]
pub struct Stack {
    pub name: Option<String>,
    /// `backend = local("dir")`: where the state, the world and the lock
    /// live; `s3(...)`: the state, plan key, audit log and lease in a
    /// bucket. `None`: `dform.state/<name>/`.
    pub backend: Option<Backend>,
    pub unknowns: Unknowns,
    /// `role = bootstrap`: the stack creates what a controller runs in. It
    /// stays batch: `dform controller run` refuses it.
    pub bootstrap: bool,
    /// Provider schemas, as `--provider` takes them: a name or a path.
    pub providers: Vec<String>,
    /// Each `provider NAME { .. }` block's spec (as in `providers`) ->
    /// NAME: the block's settings configure the provider it selects.
    pub provider_blocks: BTreeMap<String, String>,
    /// `stack app[env, region]`: the inputs that key the stack, in order,
    /// each a stack input (checked here).
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
        ("local", [dir]) => match string(dir) {
            Some(dir) => Ok(Backend::Local(PathBuf::from(dir))),
            None => Err("backend local(DIR) takes a directory string".into()),
        },
        ("s3", [bucket, prefix, rest @ ..]) if rest.len() <= 1 => {
            let usage = "backend s3(\"BUCKET\", \"PREFIX\", {endpoint: \"URL\", region: \"R\"}) \
                         takes a bucket and a prefix string, and optionally a record of \
                         endpoint and region strings";
            let (Some(bucket), Some(prefix)) = (string(bucket), string(prefix)) else {
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
                    let v = string(v).ok_or_else(|| format!("backend s3: {k} is a string"))?;
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

/// A backend as the manifest's `[defaults] backend` writes it, a term of
/// the language in a string.
pub fn parse_backend(text: &str) -> Result<Backend> {
    let program = crate::parser::parse_program(&format!("stack _ {{ backend = {text} }}"))
        .map_err(|_| anyhow::anyhow!("not a backend term"))?;
    program
        .statements
        .iter()
        .find_map(|s| match s {
            Stmt::Stack(c) => c.config.iter().find(|(k, _, _)| k == "backend"),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("not a backend term"))
        .and_then(|(_, v, _)| backend(v).map_err(|e| anyhow::anyhow!(e)))
}

fn string(t: &Term) -> Option<&str> {
    match t {
        Term::Val(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// Read the `stack` and `provider` statements of a loaded program.
pub fn config(program: &Program) -> Result<Stack> {
    let mut out = Stack::default();
    let mut diags = Vec::new();
    let mut first: Option<Span> = None;
    let mut inputs: Vec<&crate::ast::InputDecl> = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::Input(i) => inputs.push(i),
            Stmt::Stack(c) => {
                if let Some(at) = first {
                    diags.push(
                        Diagnostic::error(
                            c.span,
                            "a second stack statement: one program owns one stack",
                        )
                        .with_label(at, "the program's stack"),
                    );
                    continue;
                }
                first = Some(c.span);
                out.name = Some(c.name.clone());
                out.keys = c.keys.clone();
                stack_config(c, &mut out, &mut diags);
            }
            Stmt::Provider(c) => {
                let spec = provider(c, &mut diags);
                out.provider_blocks.insert(spec.clone(), c.name.clone());
                out.providers.push(spec);
            }
            _ => {}
        }
    }
    check_keys(&out, &inputs, &mut diags);
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// Each key of a keyed stack is one of its own inputs, named once, and not
/// a secret (its value names a directory and a registry entry).
fn check_keys(out: &Stack, inputs: &[&crate::ast::InputDecl], diags: &mut Vec<Diagnostic>) {
    let name = out.name.as_deref().unwrap_or_default();
    for (i, (k, span)) in out.keys.iter().enumerate() {
        if let Some((_, at)) = out.keys[..i].iter().find(|(x, _)| x == k) {
            diags.push(
                Diagnostic::error(*span, format!("stack {name} is keyed by {k} twice"))
                    .with_label(*at, "first here"),
            );
            continue;
        }
        let Some(decl) = inputs.iter().find(|d| &d.name == k) else {
            let names: Vec<&str> = inputs.iter().map(|d| d.name.as_str()).collect();
            let d = Diagnostic::error(
                *span,
                format!("stack {name} is keyed by {k}, which is not an input of the stack"),
            );
            diags.push(if names.is_empty() {
                d.with_help(format!("declare it: `input {k}: string`"))
            } else {
                d.with_note(format!("its inputs: {}", names.join(", ")))
            });
            continue;
        };
        if matches!(&decl.ty, crate::ast::TypeExpr::Apply(n, _) if n == "secret") {
            diags.push(
                Diagnostic::error(
                    *span,
                    format!(
                        "stack {name} is keyed by {k}, a secret input: a key's value names \
                         the deployment's state directory and registry entry"
                    ),
                )
                .with_label(decl.span, "declared secret here"),
            );
        }
    }
}

/// One deployment of a stack: the stack's name and, when it is keyed, the
/// value of each key input, in the header's order (as the value prints,
/// a string bare).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub stack: String,
    pub key: Vec<(String, String)>,
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

/// A value as a key prints it: a string bare, anything else as a value.
pub fn key_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => crate::partition::fmt_value(v),
    }
}

/// The deployment a run is of: the stack, and each key input's value as
/// this run gives it: `--set` (`set`, the `input(k, v)` facts), else an
/// input fact or an `--input-file` contribution of the program, else the
/// input's default. A key with none is an error naming the input.
pub fn instance(cfg: &Stack, stack: &str, program: &Program, set: &[Atom]) -> Result<Instance> {
    let mut key = Vec::new();
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
        match set
            .iter()
            .rev()
            .find_map(given)
            .or_else(from_program)
            .or_else(default)
        {
            Some(v) => key.push((k.clone(), key_text(&v))),
            None => diags.push(
                Diagnostic::error(
                    *span,
                    format!("stack {stack} is keyed by input {k}, which has no value"),
                )
                .with_help(format!(
                    "give it with `--set {k}=...`: each value of the key is its own \
                     deployment, with its own state"
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
            "unknowns" => match string(v) {
                Some("strict") => out.unknowns = Unknowns::Strict,
                Some("permissive") => out.unknowns = Unknowns::Permissive,
                _ => diags.push(Diagnostic::error(
                    *span,
                    "unknowns is `strict` or `permissive`",
                )),
            },
            "role" => match string(v) {
                Some("bootstrap") => out.bootstrap = true,
                _ => diags.push(Diagnostic::error(*span, "role is `bootstrap`")),
            },
            "isolated" => match v {
                Term::Val(Value::Bool(b)) if !c.keys.is_empty() => out.isolated = *b,
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
            "audit_sink" => match string(v) {
                Some(cmd) => out.audit_sink = Some(cmd.to_string()),
                None => diags.push(Diagnostic::error(
                    *span,
                    "audit_sink is a command string, run with `sh -c`",
                )),
            },
            other => diags.push(
                Diagnostic::error(*span, format!("stack {} has no setting {other}", c.name))
                    .with_note(
                        "its settings: backend, unknowns, role, isolated, approvals, audit_sink",
                    ),
            ),
        }
    }
}

/// `jwks("url")` or `jwks_file("path")` (from the project root of the
/// file the stack statement is in), each with an optional issuer.
fn trust_root(at: Span, t: &Term) -> Option<crate::approval::TrustRoot> {
    use crate::approval::{Jwks, TrustRoot};
    let Term::Func { name, args } = t else {
        return None;
    };
    let args: Vec<&str> = args.iter().map(string).collect::<Option<_>>()?;
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
        match (k.as_str(), string(v)) {
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
/// neither is written as the bare state path.
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
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub state: PathBuf,
    pub bootstrap: bool,
    /// `k8s("ns/name")` or `local("dir")`: the stack was handed over there
    /// and its state is `state`, wherever its program's backend says.
    pub backend: Option<String>,
}

impl From<Written> for Entry {
    fn from(w: Written) -> Entry {
        match w {
            Written::Path(state) => Entry {
                state,
                bootstrap: false,
                backend: None,
            },
            Written::Entry {
                state,
                bootstrap,
                backend,
            } => Entry {
                state,
                bootstrap,
                backend,
            },
        }
    }
}

impl From<Entry> for Written {
    fn from(e: Entry) -> Written {
        if !e.bootstrap && e.backend.is_none() {
            Written::Path(e.state)
        } else {
            Written::Entry {
                state: e.state,
                bootstrap: e.bootstrap,
                backend: e.backend,
            }
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

/// Record that `stack`'s state is `state`, so other stacks can read its
/// outputs (and `handover` can find a bootstrap stack). A handover's
/// backend is kept.
pub fn register(root: &Path, stack: &str, state: &Path, bootstrap: bool) -> Result<()> {
    let mut r = registry(root)?;
    let abs = fs::canonicalize(state)
        .or_else(|_| std::path::absolute(state))
        .with_context(|| format!("absolute path of {}", state.display()))?;
    let backend = r.get(stack).and_then(|e| e.backend.clone());
    let entry = Entry {
        state: abs,
        bootstrap,
        backend,
    };
    if r.get(stack) == Some(&entry) {
        return Ok(());
    }
    r.insert(stack.to_string(), entry);
    save_registry(root, r)
}

/// The backend a stack was handed over to, and the directory its state now
/// lives in.
pub fn handed_over(root: &Path, stack: &str) -> Result<Option<(String, PathBuf)>> {
    Ok(registry(root)?.remove(stack).and_then(|e| {
        let dir = e.state.parent()?.to_path_buf();
        Some((e.backend?, dir))
    }))
}

/// `dform stack handover NAME --to BACKEND`: move the stack's state
/// directory (state, world, externs) to the backend and record it in the
/// registry; every later run of the stack uses it. `local("DIR")` is a
/// directory, relative to the one holding `root`; `k8s("ns/name")` stands in for the in-cluster backend: a
/// directory `k8s/ns/name` inside the state directory of the registered
/// bootstrap stack (the one that owns the cluster). The stack's state is
/// found in the registry, else at `<root>/<NAME>` (`<root>/app/env=prod`
/// for the deployment `app[env=prod]` of a keyed stack). Returns the new
/// directory.
pub fn handover(root: &Path, stack: &str, to: &str) -> Result<PathBuf> {
    let reg = registry(root)?;
    let target = match parse_target(to)? {
        Target::Local(dir) => crate::state::local_dir(root, &dir),
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
            let dir = e.state.parent().unwrap_or(Path::new("")).to_path_buf();
            dir.join("k8s").join(key)
        }
    };
    if reg.get(stack).is_some_and(|e| e.bootstrap) {
        bail!("handover {stack}: a bootstrap stack stays batch and is never handed over");
    }
    let from = match reg.get(stack) {
        Some(e) => e.state.parent().unwrap_or(Path::new("")).to_path_buf(),
        None => instance_dir(root, stack),
    };
    let state = from.join("state.json");
    if state.with_extension("lock").exists() {
        bail!(
            "handover {stack}: the stack is locked ({}); wait for the apply to finish",
            state.with_extension("lock").display()
        );
    }
    if target.exists() && fs::read_dir(&target)?.next().is_some() {
        bail!(
            "handover {stack} to {to}: {} is not empty",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if from.exists() {
        let _ = fs::remove_dir(&target);
        fs::rename(&from, &target)
            .with_context(|| format!("move {} to {}", from.display(), target.display()))?;
    } else {
        fs::create_dir_all(&target).with_context(|| format!("mkdir {}", target.display()))?;
    }
    let target = fs::canonicalize(&target).unwrap_or(target);
    let mut reg = reg;
    reg.insert(
        stack.to_string(),
        Entry {
            state: target.join("state.json"),
            bootstrap: false,
            backend: Some(to.to_string()),
        },
    );
    save_registry(root, reg)?;
    Ok(target)
}

/// `dform stack rekey`: move the state of deployment `from` (its
/// directory under `base`, the stack's directory) to deployment `to`, and
/// its registry entry with it. Nothing in the cloud changes. An unkeyed
/// `from` is the state the stack had before it was keyed: the files
/// directly in `base`. Returns the new directory.
pub fn rekey(root: &Path, base: &Path, from: &Instance, to: &Instance) -> Result<PathBuf> {
    let (old, new) = (from.name(), to.name());
    let mut reg = registry(root)?;
    if let Some(b) = reg.get(&old).and_then(|e| e.backend.clone()) {
        bail!(
            "rekey {old}: it was handed over to {b}; its state is not under {}",
            base.display()
        );
    }
    let (src, dst) = (from.dir(base), to.dir(base));
    let state = src.join("state.json");
    if !state.exists() {
        bail!("rekey {old}: no state at {}", state.display());
    }
    if state.with_extension("lock").exists() {
        bail!(
            "rekey {old}: the stack is locked ({}); wait for the apply to finish",
            state.with_extension("lock").display()
        );
    }
    if dst.exists() && fs::read_dir(&dst)?.next().is_some() {
        bail!("rekey {old} to {new}: {} is not empty", dst.display());
    }
    if from.segment().is_some() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let _ = fs::remove_dir(&dst);
        fs::rename(&src, &dst)
            .with_context(|| format!("move {} to {}", src.display(), dst.display()))?;
    } else {
        // The unkeyed state: every file of the stack's directory (the
        // deployments' directories stay).
        fs::create_dir_all(&dst).with_context(|| format!("mkdir {}", dst.display()))?;
        for e in fs::read_dir(&src).with_context(|| format!("read {}", src.display()))? {
            let e = e?;
            if e.file_type()?.is_file() {
                let to = dst.join(e.file_name());
                fs::rename(e.path(), &to)
                    .with_context(|| format!("move {} to {}", e.path().display(), to.display()))?;
            }
        }
    }
    if let Some(e) = reg.remove(&old) {
        let dst = fs::canonicalize(&dst).unwrap_or(dst.clone());
        reg.insert(
            new,
            Entry {
                state: dst.join("state.json"),
                bootstrap: e.bootstrap,
                backend: None,
            },
        );
        save_registry(root, reg)?;
    }
    Ok(dst)
}

enum Target {
    Local(PathBuf),
    K8s(String),
}

/// `local("DIR")` or `k8s("ns/name")`, as the command line gives it.
fn parse_target(to: &str) -> Result<Target> {
    let bad = || {
        anyhow::anyhow!(
            "unknown backend {to}: the backends are local(\"DIR\") and k8s(\"ns/name\")"
        )
    };
    let (kind, rest) = to.split_once('(').ok_or_else(bad)?;
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

/// `stack_output(Stack, Key, Value)` for every output of every other
/// registered stack, read from its state.
pub fn stack_outputs(root: &Path, own: &str) -> Result<Vec<crate::ast::Atom>> {
    let mut out = Vec::new();
    for (name, e) in registry(root)? {
        let path = e.state;
        if name == own || !path.exists() {
            continue;
        }
        let st = crate::state::State::load(&path)?;
        for (k, v) in st.outputs {
            out.push(crate::ast::Atom {
                pred: "stack_output".into(),
                args: vec![
                    Term::Val(Value::Str(name.clone())),
                    Term::Val(Value::Str(k)),
                    Term::Val(v),
                ],
                record: None,
                span: Span::default(),
            });
        }
    }
    Ok(out)
}

/// Does the program give the stack an output (`output k = t` at the top)?
pub fn has_outputs(facts: &std::collections::BTreeSet<crate::ast::Atom>) -> bool {
    facts.iter().any(|a| {
        a.pred == "attr"
            && matches!(a.args.as_slice(), [Term::Val(Value::Str(t)), Term::Val(Value::Str(scope)), ..]
                if t == crate::transform::OUTPUT && scope.is_empty())
    })
}

/// The stack's own outputs in an evaluation: `attr(output, "", k, V)`,
/// those whose value is known (a null is not recorded).
pub fn outputs(facts: &std::collections::BTreeSet<crate::ast::Atom>) -> BTreeMap<String, Value> {
    fn known(v: &Value) -> bool {
        match v {
            Value::Null { .. } => false,
            Value::List(xs) => xs.iter().all(known),
            Value::Obj(m) => m.values().all(known),
            _ => true,
        }
    }
    facts
        .iter()
        .filter(|a| a.pred == "attr")
        .filter_map(|a| match a.args.as_slice() {
            [
                Term::Val(Value::Str(t)),
                Term::Val(Value::Str(scope)),
                Term::Val(Value::Str(k)),
                Term::Val(v),
            ] if t == crate::transform::OUTPUT && scope.is_empty() && known(v) => {
                Some((k.clone(), v.clone()))
            }
            _ => None,
        })
        .collect()
}
