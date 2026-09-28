use crate::ast::Term;
use crate::ast::{Atom, Constraint, Lit, Program, Resource, RuleStmt, Settings, Stmt, When};
use crate::parser;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn load_program(entry_files: &[PathBuf]) -> Result<Program> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut statements = Vec::new();
    for f in entry_files {
        let abs = absolutize(f)?;
        let p = load_file(&abs, &mut seen)?;
        statements.extend(p.statements);
    }
    Ok(Program { statements })
}

fn load_file(path: &Path, seen: &mut BTreeSet<PathBuf>) -> Result<Program> {
    let abs = absolutize(path)?;
    if !seen.insert(abs.clone()) {
        // already loaded
        return Ok(Program { statements: vec![] });
    }
    let src = fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;
    let mut prog = parser::parse_file(&display_name(&abs), &src)?;

    let base_dir = abs.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for s in prog.statements.drain(..) {
        match s {
            Stmt::Import(i) => {
                let import_path = base_dir.join(&i.path);
                let mut child = load_file(&import_path, seen)?;
                if let Some(alias) = i.alias {
                    child = prefix_program(&child, &alias);
                }
                out.extend(child.statements);
            }
            other => out.push(other),
        }
    }
    Ok(Program { statements: out })
}

fn prefix_program(program: &Program, alias: &str) -> Program {
    let mut out = Vec::new();
    for s in &program.statements {
        out.push(prefix_stmt(s.clone(), alias));
    }
    Program { statements: out }
}

fn prefix_stmt(stmt: Stmt, alias: &str) -> Stmt {
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(prefix_atom(a, alias)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: prefix_atom(r.head, alias),
            body: r.body.into_iter().map(|l| prefix_lit(l, alias)).collect(),
        }),
        Stmt::Constraint(c) => Stmt::Constraint(Constraint {
            body: c.body.into_iter().map(|l| prefix_lit(l, alias)).collect(),
            ..c
        }),
        Stmt::When(w) => Stmt::When(When {
            guard: prefix_lit(w.guard, alias),
            body: w.body.into_iter().map(|s| prefix_stmt(s, alias)).collect(),
            span: w.span,
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: prefix_term(r.typ, alias),
            name: prefix_term(r.name, alias),
            rank: r.rank,
            fields: r
                .fields
                .into_iter()
                .map(|f| crate::ast::FieldAssign {
                    value: prefix_term(f.value, alias),
                    ..f
                })
                .collect(),
            body: r
                .body
                .map(|xs| xs.into_iter().map(|l| prefix_lit(l, alias)).collect()),
            span: r.span,
        }),
        Stmt::Component(mut c) => {
            c.body = c.body.into_iter().map(|s| prefix_stmt(s, alias)).collect();
            Stmt::Component(c)
        }
        Stmt::ComponentDef(mut c) => {
            c.body = c.body.into_iter().map(|s| prefix_stmt(s, alias)).collect();
            Stmt::ComponentDef(c)
        }
        Stmt::Use(mut u) => {
            u.params = u
                .params
                .into_iter()
                .map(|(k, v)| (k, prefix_term(v, alias)))
                .collect();
            u.body = u
                .body
                .map(|xs| xs.into_iter().map(|l| prefix_lit(l, alias)).collect());
            Stmt::Use(u)
        }
        Stmt::PolicyPack(mut p) => {
            p.body = p.body.into_iter().map(|s| prefix_stmt(s, alias)).collect();
            Stmt::PolicyPack(p)
        }
        Stmt::ApplyPolicy(a) => Stmt::ApplyPolicy(a),
        Stmt::Settings(s) => Stmt::Settings(Settings {
            env: prefix_term(s.env, alias),
            rank: s.rank,
            fields: s
                .fields
                .into_iter()
                .map(|f| crate::ast::FieldAssign {
                    value: prefix_term(f.value, alias),
                    ..f
                })
                .collect(),
            body: s
                .body
                .map(|xs| xs.into_iter().map(|l| prefix_lit(l, alias)).collect()),
            span: s.span,
        }),
        Stmt::Decl(mut d) => {
            if !is_core_pred(&d.pred) {
                d.pred = format!("{alias}.{}", d.pred);
            }
            Stmt::Decl(d)
        }
        Stmt::Extern(mut e) => {
            if !is_core_pred(&e.pred) {
                e.pred = format!("{alias}.{}", e.pred);
            }
            Stmt::Extern(e)
        }
        // Do not prefix component statements or metadata; they are local structure.
        other => other,
    }
}

fn prefix_lit(lit: Lit, alias: &str) -> Lit {
    match lit {
        Lit::Pos(a) => Lit::Pos(prefix_atom(a, alias)),
        Lit::Not(a) => Lit::Not(prefix_atom(a, alias)),
        Lit::Eq(a, b) => Lit::Eq(prefix_term(a, alias), prefix_term(b, alias)),
        Lit::Neq(a, b) => Lit::Neq(prefix_term(a, alias), prefix_term(b, alias)),
        Lit::Gt(a, b) => Lit::Gt(prefix_term(a, alias), prefix_term(b, alias)),
        Lit::Ge(a, b) => Lit::Ge(prefix_term(a, alias), prefix_term(b, alias)),
        Lit::Lt(a, b) => Lit::Lt(prefix_term(a, alias), prefix_term(b, alias)),
        Lit::Le(a, b) => Lit::Le(prefix_term(a, alias), prefix_term(b, alias)),
    }
}

fn prefix_atom(mut atom: Atom, alias: &str) -> Atom {
    if !is_core_pred(&atom.pred) {
        atom.pred = format!("{alias}.{}", atom.pred);
    }
    atom.args = atom
        .args
        .into_iter()
        .map(|t| prefix_term(t, alias))
        .collect();

    if let Some(rec) = atom.record.take() {
        atom.record = Some(
            rec.into_iter()
                .map(|(k, v)| (k, prefix_term(v, alias)))
                .collect(),
        );
    }
    atom
}

fn prefix_term(term: Term, alias: &str) -> Term {
    match term {
        Term::Val(v) => Term::Val(v),
        Term::Var(v) => Term::Var(v),
        Term::Wildcard => Term::Wildcard,
        Term::List(xs) => Term::List(xs.into_iter().map(|t| prefix_term(t, alias)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, prefix_term(v, alias)))
                .collect(),
        ),
        Term::Func { name, args } => Term::Func {
            name,
            args: args.into_iter().map(|t| prefix_term(t, alias)).collect(),
        },
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(prefix_term(*item, alias)),
            body: body.into_iter().map(|l| prefix_lit(l, alias)).collect(),
        },
    }
}

/// Predicates the provider or the CLI injects as facts (discovery, world,
/// schema, inputs). They are defined even when a run has no rows for them.
pub const PROVIDER_PREDS: &[&str] = &[
    "input",
    "data",
    "cloud_exists",
    "cloud_attr",
    "cloud_computed",
    "world_attr",
    "identity",
    "deformation",
    "world_digest",
    "type_attr",
    "type_list_key",
    "type_provider",
    "type_retry",
    "type_replace",
    "capability",
    "tag_path",
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

/// Compiler-owned predicates: never namespaced by `import ... as`, and
/// defined whether or not the program writes them.
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
            | "component_scope"
            | "param"
            | "merge_rule"
            | "warn"
            | "deny"
            | "cloud_exists"
            | "cloud_attr"
            | "cloud_computed"
            | "member"
            | "env"
            | "has_env"
            | "attr"
            | "attr_conflict"
            | "attr_stuck"
            | "type_lattice"
            | "type_mint"
            | "ignore_changes"
            | "lifecycle"
            | "moved"
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
