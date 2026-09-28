//! Stacks (DESIGN.org L6, E §7.1): `stack name { backend = local("dir"),
//! unknowns = strict | permissive, role = bootstrap }.` One program owns
//! one stack. The name scopes the state (the entry file's basename when
//! there is no `stack` statement), the backend is the directory it lives in, and a lock file
//! there makes a second concurrent apply fail cleanly. `provider name {
//! source = "path" }.` selects the provider schemas the mock plays.
//!
//! Cross-stack values: an apply records the stack's outputs in its state
//! and the stack's state path in the registry `.dform/stacks.json`; every
//! other program reads them as `stack_output(Stack, Key, Value)` facts, a
//! fact provider over local state.
//!
//! `role = bootstrap` marks the stack that creates what a controller runs
//! in: the controller refuses it. `handover` moves another stack's state to
//! a new backend and records it in the registry, where every later run
//! finds it.

use crate::ast::{Config, Program, Span, Stmt, Term};
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
    /// live. `None`: `.dform/<name>/`.
    pub backend: Option<PathBuf>,
    pub unknowns: Unknowns,
    /// `role = bootstrap`: the stack creates what a controller runs in. It
    /// stays batch: `dform controller` refuses it.
    pub bootstrap: bool,
    /// Provider schemas, as `--provider` takes them: a name or a path.
    pub providers: Vec<String>,
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
    for s in &program.statements {
        match s {
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
                stack_config(c, &mut out, &mut diags);
            }
            Stmt::Provider(c) => out.providers.push(provider(c, &mut diags)),
            _ => {}
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

fn stack_config(c: &Config, out: &mut Stack, diags: &mut Vec<Diagnostic>) {
    for (k, v, span) in &c.config {
        match k.as_str() {
            "backend" => match v {
                Term::Func { name, args } if name == "local" && args.len() == 1 => {
                    match string(&args[0]) {
                        Some(dir) => out.backend = Some(PathBuf::from(dir)),
                        None => diags.push(Diagnostic::error(
                            *span,
                            "backend local(DIR) takes a directory string",
                        )),
                    }
                }
                _ => diags.push(
                    Diagnostic::error(*span, "unknown backend")
                        .with_help("the one backend is `local(\"DIR\")`"),
                ),
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
            other => diags.push(
                Diagnostic::error(*span, format!("stack {} has no setting {other}", c.name))
                    .with_note("its settings: backend, unknowns, role"),
            ),
        }
    }
}

/// A provider's schema: `source = "path"` (a directory holding
/// `schema.df`, or a `.df` file, relative to the file the statement is in),
/// else its name.
fn provider(c: &Config, diags: &mut Vec<Diagnostic>) -> String {
    let mut spec = c.name.clone();
    for (k, v, span) in &c.config {
        match (k.as_str(), string(v)) {
            ("source", Some(src)) => {
                let base = diag::location(c.span)
                    .map(|(file, _, _)| {
                        Path::new(&file)
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let mut path = base.join(src);
                if path.is_dir() {
                    path = path.join("schema.df");
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

/// An apply's hold on a stack: the file `<state>.lock`, created exclusively
/// and removed when dropped. A lock whose holder is gone (a killed apply)
/// is taken over, with a note.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire(state: &Path, stack: &str) -> Result<Lock> {
        let path = state.with_extension("lock");
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    writeln!(f, "{}", std::process::id())
                        .with_context(|| format!("write {}", path.display()))?;
                    // Tests hold the lock until a file appears, to run a
                    // second apply against a held stack.
                    if let Some(release) = std::env::var_os("DFORM_TEST_HOLD_LOCK") {
                        while !Path::new(&release).exists() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let pid: Option<u32> = holder.trim().parse().ok();
                    if let Some(pid) = pid
                        && !alive(pid)
                    {
                        eprintln!(
                            "note: stack {stack}: taking over the lock of pid {pid}, which is gone ({})",
                            path.display()
                        );
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    bail!(
                        "stack {stack} is locked by another apply (pid {}): {}; \
                         wait for it, or remove the file if no apply is running",
                        pid.map(|p| p.to_string())
                            .unwrap_or_else(|| "unknown".into()),
                        path.display()
                    );
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
        bail!("stack {stack}: could not take the lock {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Is process `pid` running? (Linux: `/proc/<pid>` exists.) Elsewhere
/// every holder is taken as alive.
fn alive(pid: u32) -> bool {
    if !Path::new("/proc").exists() {
        return true;
    }
    Path::new(&format!("/proc/{pid}")).exists()
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
    let abs = fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf());
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
/// directory; `k8s("ns/name")` stands in for the in-cluster backend: a
/// directory `k8s/ns/name` inside the state directory of the registered
/// bootstrap stack (the one that owns the cluster). The stack's state is
/// found in the registry, else at `<root>/<NAME>`. Returns the new
/// directory.
pub fn handover(root: &Path, stack: &str, to: &str) -> Result<PathBuf> {
    let reg = registry(root)?;
    let target = match parse_backend(to)? {
        Backend::Local(dir) => dir,
        Backend::K8s(key) => {
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
        None => root.join(stack),
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

enum Backend {
    Local(PathBuf),
    K8s(String),
}

/// `local("DIR")` or `k8s("ns/name")`, as the command line gives it.
fn parse_backend(to: &str) -> Result<Backend> {
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
        "local" => Ok(Backend::Local(PathBuf::from(arg))),
        "k8s" => {
            let parts: Vec<&str> = arg.split('/').collect();
            let ok = |p: &str| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            };
            match parts.as_slice() {
                [ns, name] if ok(ns) && ok(name) => Ok(Backend::K8s(format!("{ns}/{name}"))),
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

/// Why a strict stack refuses a plan: every pending group, undetermined
/// policy and deformation held on a null, and, when none of those explains
/// it, every stuck derivation. Empty: the plan is one tick, fresh nulls and
/// all.
pub fn strict_refusals(
    stuck: &[crate::stuck::Stuck],
    sections: &crate::stuck::Sections,
) -> Vec<String> {
    let mut out: Vec<String> = sections
        .pending_groups
        .iter()
        .map(|g| format!("pending group {g}"))
        .collect();
    out.extend(
        sections
            .undetermined
            .iter()
            .map(|u| format!("undetermined {u}")),
    );
    out.extend(sections.pending.iter().map(|((t, a), on)| {
        let on: Vec<String> = on.iter().map(|n| format!("?{n}")).collect();
        format!("{t}.{a} waits on {}", on.join(" "))
    }));
    if out.is_empty() {
        out.extend(
            stuck
                .iter()
                .map(|s| format!("stuck on {}: {}", s.nulls_text(), s.text)),
        );
    }
    out
}

/// The refusal as the plan prints it.
pub fn refusal_text(stack: &str, reasons: &[String]) -> String {
    let mut out = format!(
        "refused: stack {stack} is strict (unknowns = strict) and this plan needs a phase boundary:\n"
    );
    for r in reasons {
        out.push_str(&format!("- {r}\n"));
    }
    out
}
