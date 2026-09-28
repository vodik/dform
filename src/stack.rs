//! Stacks (DESIGN.org L6, E §7.1): `stack name { backend = local("dir"),
//! unknowns = strict | permissive }.` One program owns one stack. The name
//! scopes the state (the entry file's basename when there is no `stack`
//! statement), the backend is the directory it lives in, and a lock file
//! there makes a second concurrent apply fail cleanly. `provider name {
//! source = "path" }.` selects the provider schemas the mock plays.
//!
//! Cross-stack values: an apply records the stack's outputs in its state
//! and the stack's state path in the registry `.dform/stacks.json`; every
//! other program reads them as `stack_output(Stack, Key, Value)` facts, a
//! fact provider over local state.

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
            other => diags.push(
                Diagnostic::error(*span, format!("stack {} has no setting {other}", c.name))
                    .with_note("its settings: backend, unknowns"),
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

fn registry(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let path = registry_path(root);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Record that `stack`'s state is `state`, so other stacks can read its
/// outputs.
pub fn register(root: &Path, stack: &str, state: &Path) -> Result<()> {
    let mut r = registry(root)?;
    let abs = fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf());
    if r.get(stack) == Some(&abs) {
        return Ok(());
    }
    r.insert(stack.to_string(), abs);
    fs::create_dir_all(root).with_context(|| format!("mkdir {}", root.display()))?;
    let path = registry_path(root);
    fs::write(&path, serde_json::to_vec_pretty(&r)?)
        .with_context(|| format!("write {}", path.display()))
}

/// `stack_output(Stack, Key, Value)` for every output of every other
/// registered stack, read from its state.
pub fn stack_outputs(root: &Path, own: &str) -> Result<Vec<crate::ast::Atom>> {
    let mut out = Vec::new();
    for (name, path) in registry(root)? {
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
