//! Input relations (DESIGN.org "Reactive inputs and controller mode"):
//! `input relation p/N from file("path")` or `from git("repo", "ref",
//! "path")` declares that the facts of `p/N` come from outside the program.
//! Every run reads them where they are now: `plan` and `apply` once, the
//! controller whenever a source's stamp changes (`stamp`), by polling.
//!
//! A source is a `.df` file of facts (`edition 2026` first, then `p(...)`)
//! of the relations declared from it: a file may feed several relations, and a fact of any other predicate is an
//! error naming it. Paths resolve from the project root (`project::base_of`).
//! A `git` source is read at the ref (`git show REF:PATH`, so a bare
//! repository works) and stamped by the commit the ref names.

use crate::ast::{Extern, InputRelation, Program, Span, Stmt, Term};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::value::Value;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where an input relation's facts are.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Source {
    File(PathBuf),
    Git {
        repo: PathBuf,
        rev: String,
        path: String,
    },
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(p) => write!(f, "file {}", p.display()),
            Source::Git { repo, rev, path } => {
                write!(f, "git {} {rev}:{path}", repo.display())
            }
        }
    }
}

/// One `input relation` declaration, its source resolved.
#[derive(Debug, Clone)]
pub struct Relation {
    pub pred: String,
    pub arity: usize,
    pub source: Source,
    pub span: Span,
}

fn string(t: &Term) -> Option<&str> {
    match t {
        Term::Val(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// Take the program's `input relation` declarations out of it: each becomes
/// a declaration of its predicate (`decl p/N`, defined whether or not its
/// source has facts yet). The facts are stated by `read`.
pub fn take(program: &mut Program) -> Result<Vec<Relation>> {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for s in program.statements.iter_mut() {
        let Stmt::InputRelation(r) = s else {
            nested(s, &mut diags);
            continue;
        };
        match source(r) {
            Ok(source) => out.push(Relation {
                pred: r.pred.clone(),
                arity: r.arity,
                source,
                span: r.span,
            }),
            Err(d) => diags.push(*d),
        }
        *s = Stmt::Extern(Extern {
            pred: r.pred.clone(),
            arity: r.arity,
            span: r.span,
        });
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// An `input relation` anywhere but the top of the program is an error.
fn nested(s: &Stmt, diags: &mut Vec<Diagnostic>) {
    let body = match s {
        Stmt::Module(m) => &m.body,
        Stmt::PolicyPack(p) => &p.body,
        Stmt::Scenario(sc) => &sc.body,
        _ => return,
    };
    for s in body {
        if let Stmt::InputRelation(r) = s {
            diags.push(Diagnostic::error(
                r.span,
                "an input relation belongs at the top of the program",
            ));
        }
        nested(s, diags);
    }
}

fn source(r: &InputRelation) -> Result<Source, Box<Diagnostic>> {
    // From the project root (`project::base_of`).
    let base = diag::location(r.span)
        .map(|(file, _, _)| crate::project::base_of(Path::new(&file)))
        .unwrap_or_default();
    let bad = || {
        Box::new(
            Diagnostic::error(
                r.span,
                format!("input relation {}/{}: unknown source", r.pred, r.arity),
            )
            .with_help("the sources are `file(\"path\")` and `git(\"repo\", \"ref\", \"path\")`"),
        )
    };
    let Term::Func { name, args } = &r.source else {
        return Err(bad());
    };
    let args: Option<Vec<&str>> = args.iter().map(string).collect();
    match (name.as_str(), args.as_deref()) {
        ("file", Some([p])) => Ok(Source::File(base.join(p))),
        ("git", Some([repo, rev, path])) => Ok(Source::Git {
            repo: base.join(repo),
            rev: rev.to_string(),
            path: path.to_string(),
        }),
        _ => Err(bad()),
    }
}

/// The text a source holds now.
fn contents(s: &Source) -> Result<String> {
    match s {
        Source::File(p) => {
            std::fs::read_to_string(p).with_context(|| format!("input relation: read {s}"))
        }
        Source::Git { repo, rev, path } => {
            let out = git(repo, &["show", &format!("{rev}:{path}")])
                .with_context(|| format!("input relation: read {s}"))?;
            Ok(out)
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What changes when a source's facts may have: a digest of a file's
/// contents (a missing file stamps as missing), the commit a git ref names.
pub fn stamp(s: &Source) -> String {
    match s {
        Source::File(p) => match std::fs::read(p) {
            Ok(bytes) => digest(&bytes),
            Err(_) => "missing".into(),
        },
        Source::Git { repo, rev, .. } => {
            match git(
                repo,
                &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            ) {
                Ok(c) => c.trim().to_string(),
                Err(_) => "missing".into(),
            }
        }
    }
}

/// A file's stamp: a digest of its contents.
pub fn digest(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The facts every relation's source holds now, stated where the source
/// states them. A fact of a predicate not declared from that source, or of
/// the wrong arity, is an error naming it.
pub fn read(relations: &[Relation]) -> Result<Vec<Stmt>> {
    let mut sources: Vec<&Source> = relations.iter().map(|r| &r.source).collect();
    sources.sort();
    sources.dedup();
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for s in sources {
        let text = contents(s)?;
        let name = match s {
            Source::File(p) => p.display().to_string(),
            Source::Git { .. } => s.to_string(),
        };
        let facts = crate::parser::parse_file(&name, &text)?;
        for st in facts.statements {
            let Stmt::Fact(a) = st else {
                bail!("input relation: {s} holds facts, `p(...).`, and nothing else");
            };
            let declared = relations
                .iter()
                .any(|r| &r.source == s && r.pred == a.pred && r.arity == a.args.len());
            if !declared {
                diags.push(Diagnostic::error(
                    a.span,
                    format!(
                        "{}/{} is not an input relation declared from {s}",
                        a.pred,
                        a.args.len()
                    ),
                ));
                continue;
            }
            out.push(Stmt::Fact(a));
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}
