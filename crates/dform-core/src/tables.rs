//! Tables: rows from CSV, JSON, YAML and TOML files as typed input
//! relations. `input relation p(col: type, ...) from FORMAT(SOURCE)`
//! lowers (`syntax::resolve`) to an extern, `table.FORMAT.p(+path, -at,
//! -col, ...)`, and the rule `p(cols) :- reads, table.FORMAT.p(Path, _,
//! cols)`: the source is the extern's bound input, so a rule may compute
//! it, a table whose source reads its own rows is an extern in a recursive
//! rule (a compile error), and the rows are answers like any extern's:
//! recorded in the plan file and replayed by `apply PLAN`.
//!
//! A `git(repo, ref, path)` source is two externs: `table.git.p(+repo,
//! +ref, -commit)` resolves the ref, and the rows are read at the commit,
//! `table.FORMAT.p(+repo, +commit, +path, -at, ...)`. The plan file holds
//! the commit, so `apply PLAN` reads what plan read though the branch has
//! moved since; state keeps the commit each deployment was last applied
//! from (`record`), and plan says when the ref has moved from it
//! (`moved`).
//!
//! A row is typed column by column (`inputs::has_type`; a CSV cell is read
//! as its column's type). The loader never reshapes: a JSON or YAML table
//! is a list of objects, a TOML one the `[[p]]` array of tables, a CSV one
//! has a header naming the columns; every column is in every row, and
//! nothing else is. Each row's `at` is where it is, `file:line`
//! (`repo@commit:file:line` from git).
//!
//! A stack's `config = FORMAT(SOURCE)` is the table `stack.config(path,
//! value)`: every leaf of a mapping (a `path,value` CSV) is a settings
//! contribution of the deployment.

use crate::ast::{Atom, ExternFn, Lit, Program, RuleStmt, Span, Stmt, Term, TypeExpr};
use crate::externs::{self, Answer};
use crate::inputs::{has_type, type_text};
use crate::partition::fmt_value;
use crate::value::Value;
use crate::watch::{self, Relation, Source};
use anyhow::{Context, Result, anyhow, bail};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const FORMATS: &[&str] = &["csv", "json", "yaml", "toml"];

/// The table a stack's `config` is.
pub const STACK_CONFIG: &str = "stack.config";

const PREFIX: &str = "table.";

/// The extern that reads `table` as `format` (`git`: resolves its ref).
pub fn extern_name(format: &str, table: &str) -> String {
    format!("{PREFIX}{format}.{table}")
}

/// A table extern's format and table.
fn parse_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(PREFIX)?.split_once('.')
}

/// What a table extern reads, for messages: `input relation p`, `the
/// stack's config`.
pub fn describe(name: &str) -> Option<String> {
    Some(match parse_name(name)? {
        (_, STACK_CONFIG) => "the stack's config".into(),
        (_, t) => format!("input relation {t}"),
    })
}

/// Where a table's row is, `file:line`, when `a` is one: its `at`, after
/// a path (`path:line`) or a repository, commit and path
/// (`repo@commit:path:line`).
pub fn at(a: &Atom) -> Option<String> {
    let ("csv" | "json" | "yaml" | "toml", _) = parse_name(&a.pred)? else {
        return None;
    };
    let s = |i: usize| match a.args.get(i) {
        Some(Term::Val(Value::Str(s))) => Some(s.as_str()),
        _ => None,
    };
    let first = s(0)?;
    [s(1), s(3)]
        .into_iter()
        .flatten()
        .find(|at| at.starts_with(&format!("{first}:")) || at.starts_with(&format!("{first}@")))
        .map(str::to_string)
}

/// The sources a run's tables read, for the controller (`sources`).
#[derive(Default)]
pub struct Tables {
    /// (repository, commit) -> the ref that resolved to it.
    refs: RefCell<BTreeMap<(PathBuf, String), String>>,
    read: RefCell<BTreeMap<(String, Source), String>>,
}

fn string(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

/// Paths resolve from the project root of the file the declaration is in
/// (`project::base_of`).
fn base(span: Span) -> PathBuf {
    crate::diag::location(span)
        .map(|(file, _, _)| crate::project::base_of(Path::new(&file)))
        .unwrap_or_default()
}

impl Tables {
    /// The answer to a table extern's call; `None` for any other extern.
    pub fn answer(&self, f: &ExternFn, inputs: &[Value]) -> Option<Result<Vec<Vec<Value>>>> {
        let (format, table) = parse_name(&f.name)?;
        let strs: Option<Vec<&str>> = inputs.iter().map(string).collect();
        let base = base(f.span);
        Some(match (format, strs.as_deref()) {
            ("git", Some([repo, rev])) => self.resolve(&base, repo, rev),
            (_, Some([path])) => self.rows(f, format, table, inputs, || {
                let p = base.join(path);
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("table {table}: read {path}"))?;
                Ok((text, path.to_string(), Source::File(p)))
            }),
            (_, Some([repo, commit, path])) => self.rows(f, format, table, inputs, || {
                let dir = base.join(repo);
                let text = git(&dir, &["show", &format!("{commit}:{path}")])
                    .with_context(|| format!("table {table}: read {repo}@{commit}:{path}"))?;
                let rev = self
                    .refs
                    .borrow()
                    .get(&(dir.clone(), commit.to_string()))
                    .cloned();
                let shown = format!("{repo}@{}:{path}", short(commit));
                let source = Source::Git {
                    repo: dir,
                    rev: rev.unwrap_or_else(|| commit.to_string()),
                    path: path.to_string(),
                };
                Ok((text, shown, source))
            }),
            _ => Err(anyhow!("{}: the source is a string", f.name)),
        })
    }

    /// `table.git.p(repo, ref)`: the commit the ref names now.
    fn resolve(&self, base: &Path, repo: &str, rev: &str) -> Result<Vec<Vec<Value>>> {
        let dir = base.join(repo);
        let commit = git(
            &dir,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )
        .map(|c| c.trim().to_string())
        .map_err(|e| anyhow!("git repository {repo}: ref {rev} does not name a commit ({e})"))?;
        self.refs
            .borrow_mut()
            .insert((dir, commit.clone()), rev.to_string());
        let s = |x: &str| Value::Str(x.to_string());
        Ok(vec![vec![s(repo), s(rev), s(&commit)]])
    }

    /// The rows of a table, read by `read` (its text, how rows name the
    /// file, the source).
    fn rows(
        &self,
        f: &ExternFn,
        format: &str,
        table: &str,
        inputs: &[Value],
        read: impl FnOnce() -> Result<(String, String, Source)>,
    ) -> Result<Vec<Vec<Value>>> {
        let (text, shown, source) = read()?;
        let stamp = match &source {
            Source::File(_) => watch::digest(text.as_bytes()),
            Source::Git { .. } => string(&inputs[1]).unwrap_or_default().to_string(),
        };
        self.read
            .borrow_mut()
            .insert((table.to_string(), source), stamp);
        let cols = &f.args[inputs.len() + 1..];
        let at = |line: Option<usize>, n: usize| match line {
            Some(l) => format!("{shown}:{l}"),
            None => format!("{shown}:row {n}"),
        };
        let mut out = Vec::new();
        if table == STACK_CONFIG {
            for (line, path, value) in leaves(format, &text).with_context(|| shown.clone())? {
                let at = line.map_or(shown.clone(), |l| format!("{shown}:{l}"));
                let outs = vec![Value::Str(at), Value::Str(path), value];
                out.push(externs::row(f, inputs, outs));
            }
            return Ok(out);
        }
        for (i, r) in rows(format, table, &text)
            .with_context(|| shown.clone())?
            .into_iter()
            .enumerate()
        {
            let at = at(r.line, i + 1);
            let mut cells = r.cells;
            let mut outs = vec![Value::Str(at.clone())];
            for c in cols {
                let Some(cell) = cells.remove(&c.name) else {
                    bail!("{at}: no column {}", c.name);
                };
                let v = typed(c.ty.as_ref(), cell);
                if let Some(t) = &c.ty
                    && !has_type(t, &v)
                {
                    bail!(
                        "{at}: column {}: {} is not {}",
                        c.name,
                        fmt_value(&v),
                        type_text(t)
                    );
                }
                outs.push(v);
            }
            if let Some(extra) = cells.keys().next() {
                bail!(
                    "{at}: {extra} is not a column of {table} (its columns: {})",
                    cols.iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            out.push(externs::row(f, inputs, outs));
        }
        Ok(out)
    }

    /// The sources the run's tables read, stamped as read: for the
    /// controller, a changed stamp (a moved ref) is an input event.
    pub fn sources(&self) -> Vec<(Relation, String)> {
        self.read
            .borrow()
            .iter()
            .map(|((table, source), stamp)| {
                let r = Relation {
                    pred: table.clone(),
                    arity: 0,
                    source: source.clone(),
                    span: Span::default(),
                };
                (r, stamp.clone())
            })
            .collect()
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A cell as the file holds it: CSV's text, or a document's value.
enum Cell {
    Text(String),
    Value(Value),
}

/// A cell read as its column's type: CSV text as an `int` or a `bool`
/// when it spells one, a string as an `inet` when it parses as one;
/// anything else as it is, for `has_type` to judge (a number is not a
/// string).
fn typed(ty: Option<&TypeExpr>, cell: Cell) -> Value {
    let name = match ty {
        Some(TypeExpr::Name(n)) => n.as_str(),
        _ => "",
    };
    let v = match cell {
        Cell::Value(v) => v,
        Cell::Text(s) => match name {
            "int" => s.parse().map(Value::Int).unwrap_or(Value::Str(s)),
            "bool" if s == "true" || s == "false" => Value::Bool(s == "true"),
            _ => Value::Str(s),
        },
    };
    match (name, v) {
        ("inet", Value::Str(s)) => match crate::value::parse_ipnet(&s) {
            Some((addr, prefix)) => Value::IpNet { addr, prefix },
            None => Value::Str(s),
        },
        (_, v) => v,
    }
}

/// One row: its line when the format says, and its cells by column.
struct Row {
    line: Option<usize>,
    cells: BTreeMap<String, Cell>,
}

/// The line holding byte `at` of `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

fn rows(format: &str, table: &str, text: &str) -> Result<Vec<Row>> {
    match format {
        "csv" => {
            let mut r = csv::Reader::from_reader(text.as_bytes());
            let header = r.headers()?.clone();
            let mut out = Vec::new();
            for rec in r.records() {
                let rec = rec?;
                out.push(Row {
                    line: rec.position().map(|p| p.line() as usize),
                    cells: header
                        .iter()
                        .zip(rec.iter())
                        .map(|(k, v)| (k.to_string(), Cell::Text(v.to_string())))
                        .collect(),
                });
            }
            Ok(out)
        }
        "json" => {
            let items: Vec<&serde_json::value::RawValue> = serde_json::from_str(text)
                .context("a JSON table is a list of objects, one per row")?;
            items
                .into_iter()
                .map(|raw| {
                    let line = line_of(text, raw.get().as_ptr() as usize - text.as_ptr() as usize);
                    let v: serde_json::Value = serde_json::from_str(raw.get())?;
                    let serde_json::Value::Object(m) = v else {
                        bail!("{line}: a row is an object");
                    };
                    let cells = m
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), Cell::Value(json(v)?))))
                        .collect::<Result<_>>()
                        .with_context(|| format!("line {line}"))?;
                    Ok(Row {
                        line: Some(line),
                        cells,
                    })
                })
                .collect()
        }
        "yaml" => {
            let doc: serde_yaml::Value = serde_yaml::from_str(text)?;
            let serde_yaml::Value::Sequence(items) = doc else {
                bail!("a YAML table is a list of mappings, one per row");
            };
            // Block style: an item's line is the line its `- ` is on.
            let dashes: Vec<usize> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| l == &"-" || l.starts_with("- "))
                .map(|(i, _)| i + 1)
                .collect();
            let lines = (dashes.len() == items.len()).then_some(dashes);
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    let line = lines.as_ref().map(|ls| ls[i]);
                    let serde_yaml::Value::Mapping(m) = item else {
                        bail!("row {}: a row is a mapping", i + 1);
                    };
                    let cells = m
                        .into_iter()
                        .map(|(k, v)| Ok((yaml_key(&k)?, Cell::Value(yaml(v)?))))
                        .collect::<Result<_>>()
                        .with_context(|| format!("row {}", i + 1))?;
                    Ok(Row { line, cells })
                })
                .collect()
        }
        "toml" => {
            type Rows = BTreeMap<String, Vec<toml::Spanned<toml::Table>>>;
            let mut doc: Rows = toml::from_str(text).with_context(|| {
                format!("a TOML table is its rows as `[[{table}]]`, and nothing else")
            })?;
            let items = doc.remove(table).unwrap_or_default();
            if let Some(k) = doc.keys().next() {
                bail!("{k} is not {table}: a TOML table is its rows as `[[{table}]]`");
            }
            items
                .into_iter()
                .map(|item| {
                    let line = line_of(text, item.span().start);
                    let cells = item
                        .into_inner()
                        .into_iter()
                        .map(|(k, v)| Ok((k, Cell::Value(toml_value(v)))))
                        .collect::<Result<_>>()?;
                    Ok(Row {
                        line: Some(line),
                        cells,
                    })
                })
                .collect()
        }
        f => bail!("unknown format {f}"),
    }
}

/// A stack config's leaves: (line, dotted path, value). A mapping's nested
/// mappings are walked to their leaves, as a settings block's objects are;
/// a CSV one has the columns `path` and `value`.
fn leaves(format: &str, text: &str) -> Result<Vec<(Option<usize>, String, Value)>> {
    let mut out = Vec::new();
    match format {
        "csv" => {
            for r in rows("csv", STACK_CONFIG, text)? {
                let mut cells = r.cells;
                let (Some(Cell::Text(p)), Some(Cell::Text(v)), true) = (
                    cells.remove("path"),
                    cells.remove("value"),
                    cells.is_empty(),
                ) else {
                    bail!("a CSV config has the columns path and value");
                };
                flatten(&mut out, r.line, p, Value::Str(v));
            }
        }
        "json" => {
            let m: BTreeMap<String, &serde_json::value::RawValue> =
                serde_json::from_str(text).context("a JSON config is an object")?;
            for (k, raw) in m {
                let line = line_of(text, raw.get().as_ptr() as usize - text.as_ptr() as usize);
                let v: serde_json::Value = serde_json::from_str(raw.get())?;
                let v = json(&v).with_context(|| format!("line {line}"))?;
                flatten(&mut out, Some(line), k, v);
            }
        }
        "yaml" => {
            let serde_yaml::Value::Mapping(m) = serde_yaml::from_str(text)? else {
                bail!("a YAML config is a mapping");
            };
            let lines = yaml_key_lines(text);
            for (k, v) in m {
                let k = yaml_key(&k)?;
                let v = yaml(v).with_context(|| k.clone())?;
                let mut leaves = Vec::new();
                flatten(&mut leaves, None, k, v);
                for (_, path, v) in leaves {
                    // The line of the longest prefix the scan found.
                    let mut p = path.as_str();
                    let line = loop {
                        if let Some(l) = lines.get(p) {
                            break Some(*l);
                        }
                        match p.rsplit_once('.') {
                            Some((head, _)) => p = head,
                            None => break None,
                        }
                    };
                    out.push((line, path, v));
                }
            }
        }
        "toml" => {
            let m: BTreeMap<String, toml::Spanned<toml::Value>> = toml::from_str(text)?;
            for (k, v) in m {
                let line = line_of(text, v.span().start);
                flatten(&mut out, Some(line), k, toml_value(v.into_inner()));
            }
        }
        f => bail!("unknown format {f}"),
    }
    Ok(out)
}

fn flatten(
    out: &mut Vec<(Option<usize>, String, Value)>,
    line: Option<usize>,
    key: String,
    v: Value,
) {
    match v {
        Value::Obj(m) if !m.is_empty() => {
            for (k, v) in m {
                flatten(out, line, format!("{key}.{k}"), v);
            }
        }
        v => out.push((line, key, v)),
    }
}

/// The line of each block-style key of a YAML document, by dotted path.
fn yaml_key_lines(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let body = l.trim_start();
        if body.is_empty() || body.starts_with('#') || body.starts_with('-') {
            continue;
        }
        let indent = l.len() - body.len();
        let Some((key, _)) = body.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches(['"', '\'']).to_string();
        while stack.last().is_some_and(|(n, _)| *n >= indent) {
            stack.pop();
        }
        let path = match stack.last() {
            Some((_, p)) => format!("{p}.{key}"),
            None => key,
        };
        out.insert(path.clone(), i + 1);
        stack.push((indent, path));
    }
    out
}

/// A JSON value: an integer or not (a float is its text); `null` is not a
/// value.
fn json(j: &serde_json::Value) -> Result<Value> {
    Ok(match j {
        serde_json::Value::Null => bail!("null is not a value"),
        serde_json::Value::Array(xs) => Value::List(xs.iter().map(json).collect::<Result<_>>()?),
        serde_json::Value::Object(m) => Value::Obj(
            m.iter()
                .map(|(k, v)| Ok((k.clone(), json(v)?)))
                .collect::<Result<_>>()?,
        ),
        j => externs::from_json(j),
    })
}

fn yaml_key(k: &serde_yaml::Value) -> Result<String> {
    match k {
        serde_yaml::Value::String(s) => Ok(s.clone()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("a key is a string"),
    }
}

fn yaml(v: serde_yaml::Value) -> Result<Value> {
    use serde_yaml::Value as Y;
    Ok(match v {
        Y::Null => bail!("null is not a value"),
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Str(n.to_string()),
        },
        Y::String(s) => Value::Str(s),
        Y::Sequence(xs) => Value::List(xs.into_iter().map(yaml).collect::<Result<_>>()?),
        Y::Mapping(m) => Value::Obj(
            m.into_iter()
                .map(|(k, v)| Ok((yaml_key(&k)?, yaml(v)?)))
                .collect::<Result<_>>()?,
        ),
        Y::Tagged(t) => bail!("a tag ({}) is not a value; convert in a rule", t.tag),
    })
}

fn toml_value(v: toml::Value) -> Value {
    use toml::Value as T;
    match v {
        T::String(s) => Value::Str(s),
        T::Integer(i) => Value::Int(i),
        T::Float(f) => Value::Str(f.to_string()),
        T::Boolean(b) => Value::Bool(b),
        T::Datetime(d) => Value::Str(d.to_string()),
        T::Array(xs) => Value::List(xs.into_iter().map(toml_value).collect()),
        T::Table(m) => Value::Obj(m.into_iter().map(|(k, v)| (k, toml_value(v))).collect()),
    }
}

/// A stack's config, lowered (`transform::lower`): the rule the resolver
/// wrote, `arg("settings", Row, P, V, normal) :- ..., table.F.stack.config(..,
/// At, P, V)`, contributes at a path only a row knows, which would make
/// every settings cell one partition (`partition`). So it becomes one rule
/// per settings path the program knows (writes or reads), `P` that path;
/// and a leaf at any other path is a deny naming it, where the file has it.
pub fn expand_config(program: Program) -> Program {
    let is_config = |l: &Lit| matches!(l, Lit::Pos(a) if parse_name(&a.pred).is_some_and(|(f, t)| f != "git" && t == STACK_CONFIG));
    let (config, mut out): (Vec<Stmt>, Vec<Stmt>) = program
        .statements
        .into_iter()
        .partition(|s| matches!(s, Stmt::Rule(r) if r.body.iter().any(is_config)));
    if config.is_empty() {
        return Program { statements: out };
    }
    let mut known = BTreeSet::new();
    let mut note = |a: &Atom, pred: &str, path: usize| {
        if a.pred == pred
            && matches!(a.args.first(), Some(Term::Val(Value::Str(t))) if t == "settings")
            && let Some(Term::Val(Value::Str(p))) = a.args.get(path)
        {
            known.insert(p.clone());
        }
    };
    for s in &out {
        let (head, body): (Option<&Atom>, &[Lit]) = match s {
            Stmt::Fact(a) => (Some(a), &[]),
            Stmt::Rule(r) => (Some(&r.head), &r.body),
            _ => continue,
        };
        if let Some(h) = head {
            note(h, "arg", 2);
        }
        for l in body {
            if let Lit::Pos(a) | Lit::Not(a) = l {
                note(a, "attr", 2);
            }
        }
    }
    const KNOWN: &str = "__config_path";
    for s in config {
        let Stmt::Rule(r) = s else { continue };
        let (Some(Term::Var(p)), Some(Lit::Pos(ext))) =
            (r.head.args.get(2), r.body.iter().find(|l| is_config(l)))
        else {
            continue;
        };
        let at = ext.args[ext.args.len() - 3].clone();
        for k in &known {
            let path = Term::Val(Value::Str(k.clone()));
            out.push(Stmt::Rule(RuleStmt {
                head: subst(&r.head, p, &path),
                body: r
                    .body
                    .iter()
                    .map(|l| match l {
                        Lit::Pos(a) => Lit::Pos(subst(a, p, &path)),
                        l => l.clone(),
                    })
                    .collect(),
            }));
        }
        let mut body = r.body.clone();
        body.push(Lit::Not(atom(
            KNOWN,
            vec![Term::Var(p.clone())],
            r.head.span,
        )));
        let message = Term::Func {
            name: "format".into(),
            args: vec![
                Term::Val(Value::Str(
                    "%s: %s is not a setting the program writes or reads".into(),
                )),
                at,
                Term::Var(p.clone()),
            ],
        };
        out.push(Stmt::Rule(RuleStmt {
            head: atom("deny", vec![message], r.head.span),
            body,
        }));
    }
    for k in known {
        out.push(Stmt::Fact(atom(
            KNOWN,
            vec![Term::Val(Value::Str(k))],
            Span::default(),
        )));
    }
    Program { statements: out }
}

fn atom(pred: &str, args: Vec<Term>, span: Span) -> Atom {
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span,
    }
}

/// `a` with variable `v` replaced by `t` in its arguments.
fn subst(a: &Atom, v: &str, t: &Term) -> Atom {
    let args = a
        .args
        .iter()
        .map(|x| match x {
            Term::Var(w) if w == v => t.clone(),
            x => x.clone(),
        })
        .collect();
    Atom { args, ..a.clone() }
}

/// The commit of each git table call among `answers`: (table, repo, ref,
/// commit).
fn commits(answers: &[Answer]) -> Vec<(&str, &str, &str, &str)> {
    answers
        .iter()
        .filter_map(|a| {
            let ("git", table) = parse_name(&a.pred)? else {
                return None;
            };
            let [_, _, Value::Str(c)] = a.rows.first()?.as_slice() else {
                return None;
            };
            Some((
                table,
                string(&a.inputs[0])?,
                string(&a.inputs[1])?,
                c.as_str(),
            ))
        })
        .collect()
}

/// Each git table whose ref names another commit now than when the
/// deployment was last applied (`applied`, from state):
/// `peering: ops.git env/prod 3b1c7e0 -> a9d0f11`.
pub fn moved(applied: &[Answer], now: &[Answer]) -> Vec<String> {
    let was = commits(applied);
    commits(now)
        .into_iter()
        .filter_map(|(t, repo, rev, c)| {
            let (_, _, _, old) = was
                .iter()
                .find(|(t2, r2, v2, _)| (*t2, *r2, *v2) == (t, repo, rev))?;
            (*old != c).then(|| format!("{t}: {repo} {rev} {} -> {}", short(old), short(c)))
        })
        .collect()
}

/// Keep in `applied` (state's extern answers) the commits the apply read,
/// replacing the ones before. They are not replayed: a table extern is not
/// `persist`.
pub fn record(applied: &mut Vec<Answer>, now: &[Answer]) {
    for a in now
        .iter()
        .filter(|a| parse_name(&a.pred).is_some_and(|(f, _)| f == "git"))
    {
        applied.retain(|b| !(b.pred == a.pred && b.inputs == a.inputs));
        applied.push(a.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_keys_are_found_by_path() {
        let lines = yaml_key_lines("a:\n  b: 1\n  c:\n    d: 2\nnet.x: 3\n");
        assert_eq!(lines["a.b"], 2);
        assert_eq!(lines["a.c.d"], 4);
        assert_eq!(lines["net.x"], 5);
    }
}
