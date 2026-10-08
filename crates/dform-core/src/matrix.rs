//! The environment matrix (R-114): which deployments a project has is
//! code. A project module, `project.df` at the root (or any file that is
//! no stack and makes resources of stacks, named as a target), makes each
//! deployment a resource of its stack's type, its attributes the key's
//! values: `resource stacks.platform lab { env = "lab" }`, with clauses
//! and ranges as anywhere (`resource stacks.apps "${e}" { env = e } where
//! e in environment`). The module is evaluated on its own, with no
//! provider and no state: what it wants is the matrix.
//!
//! `plan` and `apply` with no target run on the matrix, in dependency
//! order; `test` enumerates it. What the project's applies made is kept
//! under the state root ([`Made`]): a deployment an apply of the matrix
//! made that the module no longer lists is removed, destroyed by the
//! project's next apply. A deployment the module does not list is still
//! a target of its own: nothing is forced.

use crate::ast::{Program, Span, Stmt, Term};
use crate::diag::{Diagnostic, Diagnostics};
use crate::spell;
use crate::value::Value;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One deployment the project module lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The stack's name, `platform`.
    pub stack: String,
    /// The resource's name in the module, `lab`.
    pub resource: String,
    /// The key values the module gives, in the stack's key order; a key it
    /// leaves out is its input's default, as a target's.
    pub key: Vec<(String, String)>,
    /// The statement that lists it.
    pub span: Span,
}

impl Listed {
    /// The deployment as a target names it, `platform[env=lab]`, with the
    /// key values the module gives.
    pub fn target(&self) -> String {
        match self.key.is_empty() {
            true => self.stack.clone(),
            false => {
                let kv: Vec<String> = self.key.iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!("{}[{}]", self.stack, kv.join(","))
            }
        }
    }
}

impl Listed {
    /// The line of the module that lists it.
    pub fn line(&self) -> Option<usize> {
        crate::diag::location(self.span).map(|(_, line, _)| line)
    }
}

/// A project module's deployments, in the order it lists them.
#[derive(Debug, Clone)]
pub struct Matrix {
    /// The module's file.
    pub file: PathBuf,
    pub listed: Vec<Listed>,
}

/// The project module at `root`, if the project has one.
pub fn module_at(root: &Path) -> Option<PathBuf> {
    let f = root.join(crate::project::PROJECT_MODULE);
    f.is_file().then_some(f)
}

impl Matrix {
    /// Load and evaluate the project module `file`: each resource it makes
    /// of a stack is a deployment.
    pub fn load(file: &Path) -> Result<Matrix> {
        let (program, _, deployed) =
            crate::loader::load_program_files_with(&[file.to_path_buf()], &|p| {
                std::fs::read_to_string(p)
            })?;
        let (res, violations) = crate::engine::eval(&program, &[])?;
        if !violations.is_empty() {
            bail!(
                "{}: the project module is refused:\n- {}",
                file.display(),
                violations.join("\n- ")
            );
        }
        let mut listed = Vec::new();
        for f in &res.facts {
            let [Term::Val(Value::Str(typ)), Term::Val(Value::Str(name))] = f.args.as_slice()
            else {
                continue;
            };
            if f.pred != "want" {
                continue;
            }
            let Some(d) = deployed.iter().find(|d| d.path == *typ) else {
                continue;
            };
            let span = site(&program, typ, name);
            if d.name.contains('.') {
                return Err(Diagnostics(vec![
                    Diagnostic::error(
                        span,
                        format!(
                            "{typ} is another project's stack: its deployments are that project's"
                        ),
                    )
                    .with_help("list it in that project's own project module"),
                ])
                .into());
            }
            // The resource's attributes are the key's values.
            let mut given: BTreeMap<String, String> = BTreeMap::new();
            for a in res.facts.iter().filter(|a| {
                a.pred == "attr"
                    && matches!(a.args.as_slice(),
                        [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), ..] if t == typ && n == name)
            }) {
                let [_, _, Term::Val(Value::Str(path)), Term::Val(v)] = a.args.as_slice() else {
                    continue;
                };
                if !d.keys.contains(path) {
                    let keys = match d.keys.is_empty() {
                        true => format!("the stack {} has no key", d.name),
                        false => format!("its key: {}", d.keys.join(", ")),
                    };
                    return Err(Diagnostics(vec![
                        Diagnostic::error(
                            span,
                            format!(
                                "resource {typ} {name}: `{path}` is not a key of the stack {} \
                                 ({keys})",
                                d.name
                            ),
                        )
                        .with_help(
                            "a deployment is named by its key alone; its other inputs are its \
                             program's (`set from` in the stack)",
                        ),
                    ])
                    .into());
                }
                given.insert(path.clone(), spell::bare(v));
            }
            if let Some(c) = res.facts.iter().find(|a| {
                a.pred == "attr_conflict"
                    && matches!(a.args.as_slice(),
                        [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), ..] if t == typ && n == name)
            }) {
                let path = match c.args.get(2) {
                    Some(Term::Val(v)) => spell::bare(v),
                    _ => String::new(),
                };
                return Err(Diagnostics(vec![
                    Diagnostic::error(
                        span,
                        format!("resource {typ} {name}: two values of {path} disagree"),
                    )
                    .with_help("give each key one value"),
                ])
                .into());
            }
            let key = d
                .keys
                .iter()
                .filter_map(|k| given.get(k).map(|v| (k.clone(), v.clone())))
                .collect();
            listed.push(Listed {
                stack: d.name.clone(),
                resource: name.clone(),
                key,
                span,
            });
        }
        // In the order the module states them, a statement's own by name.
        listed.sort_by(|a, b| (a.span.start, &a.resource).cmp(&(b.span.start, &b.resource)));
        for (i, a) in listed.iter().enumerate() {
            if let Some(b) = listed[..i]
                .iter()
                .find(|b| b.stack == a.stack && b.key == a.key)
            {
                return Err(Diagnostics(vec![
                    Diagnostic::error(
                        a.span,
                        format!(
                            "resource stacks.{} {} lists {} again: {} is the same deployment",
                            a.stack,
                            a.resource,
                            a.target(),
                            b.resource
                        ),
                    )
                    .with_help("a deployment is listed once; remove one"),
                ])
                .into());
            }
        }
        Ok(Matrix {
            file: file.to_path_buf(),
            listed,
        })
    }
}

/// The statement of the module that makes the resource `name` of `typ`:
/// the one naming it literally, else the first of the type whose name is
/// computed.
fn site(program: &Program, typ: &str, name: &str) -> Span {
    let of_type =
        |r: &&crate::ast::Resource| matches!(&r.typ, Term::Val(Value::Str(t)) if t == typ);
    let resources: Vec<&crate::ast::Resource> = program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Resource(r) => Some(r),
            _ => None,
        })
        .filter(of_type)
        .collect();
    resources
        .iter()
        .find(|r| matches!(&r.name, Term::Val(Value::Str(n)) if n == name))
        .or_else(|| {
            resources
                .iter()
                .find(|r| !matches!(&r.name, Term::Val(Value::Str(_))))
        })
        .map(|r| r.span)
        .unwrap_or_default()
}

/// What the applies of a project module made (R-114), under the state
/// root: per module (its path from the root), each deployment an apply
/// of the matrix applied, by name, and its stack and key. A deployment
/// here the module no longer lists is removed: the project's next apply
/// destroys it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Made(BTreeMap<String, BTreeMap<String, Kept>>);

/// One deployment a project's apply made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Kept {
    pub stack: String,
    pub key: Vec<(String, String)>,
}

/// The file [`Made`] is kept in, under the state root.
const MADE: &str = "project.json";

impl Made {
    pub fn load(state_root: &Path) -> Result<Made> {
        let path = state_root.join(MADE);
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Made::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// What the module `module` made, by deployment.
    pub fn of(&self, module: &str) -> BTreeMap<String, Kept> {
        self.0.get(module).cloned().unwrap_or_default()
    }

    /// Record that the module made `name` (`Some`) or that it is gone.
    pub fn record(state_root: &Path, module: &str, name: &str, kept: Option<Kept>) -> Result<()> {
        let mut made = Made::load(state_root)?;
        let of = made.0.entry(module.to_string()).or_default();
        let changed = match kept {
            Some(k) => of.insert(name.to_string(), k.clone()) != Some(k),
            None => of.remove(name).is_some(),
        };
        if of.is_empty() {
            made.0.remove(module);
        }
        if !changed {
            return Ok(());
        }
        std::fs::create_dir_all(state_root)
            .with_context(|| format!("mkdir {}", state_root.display()))?;
        crate::store::write_atomic(&state_root.join(MADE), &serde_json::to_vec_pretty(&made)?)
    }
}
