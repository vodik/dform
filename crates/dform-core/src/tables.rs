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
//! `set from FORMAT(SOURCE)` (R-38) is the table `set(path,
//! value)`: every leaf of a mapping (a `path,value` CSV) is a contribution
//! to the input at its path ([`expand_set_from`]).

use crate::ast::{Atom, ExternFn, Lit, Program, RuleStmt, Span, Stmt, Term, TypeExpr, atom};
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

/// The table a `set from` document is (`set` is a keyword: no relation
/// has its name).
pub const SET_DOC: &str = "set";

/// The table a loader call is, `yaml(path)` as a value: one row, the
/// whole document (`document.git` read at a commit). A keyword too.
pub const DOCUMENT: &str = "document";

/// The format of a table read from a value (an input, a `let`, a
/// selection into a document) rather than a file: `table.value.p(+doc,
/// -at, ..)`.
pub const VALUE: &str = "value";

/// A table's name with the selector its rows are at: `p|.teams[*].services`.
pub fn selected(table: &str, selector: &str) -> String {
    match selector.is_empty() {
        true => table.to_string(),
        false => format!("{table}|{selector}"),
    }
}

/// One step of a selector (R-39): `.name` a field, `[*]` every element.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Field(String),
    Each,
}

fn steps(selector: &str) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let mut rest = selector;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("[*]") {
            out.push(Step::Each);
            rest = r;
        } else if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            out.push(Step::Field(r[..end].to_string()));
            rest = &r[end..];
        } else {
            bail!("internal: selector {selector}");
        }
    }
    Ok(out)
}

/// The rows a selector names in `doc`, each with the objects that enclose
/// it, innermost last: `.name` takes a field, `[*]` every element, and a
/// list at the end is its elements (R-39).
fn select(doc: Value, selector: &str) -> Result<Vec<(Value, Vec<Value>)>> {
    let mut at: Vec<(Value, Vec<Value>)> = vec![(doc, Vec::new())];
    let mut path = String::new();
    for step in steps(selector)? {
        let mut next = Vec::new();
        for (v, ctx) in at {
            match (&step, v) {
                (Step::Field(f), Value::Obj(mut m)) => {
                    let Some(x) = m.remove(f) else {
                        bail!("{} has no field {f}", shown_path(&path));
                    };
                    let mut ctx = ctx;
                    ctx.push(Value::Obj(m));
                    next.push((x, ctx));
                }
                (Step::Each, Value::List(xs)) => {
                    next.extend(xs.into_iter().map(|x| (x, ctx.clone())));
                }
                (Step::Field(f), _) => bail!("{} is no object: no field {f}", shown_path(&path)),
                (Step::Each, _) => bail!("{} is no list: `[*]` takes a list", shown_path(&path)),
            }
        }
        path.push_str(&match &step {
            Step::Field(f) => format!(".{f}"),
            Step::Each => "[*]".to_string(),
        });
        at = next;
    }
    Ok(at
        .into_iter()
        .flat_map(|(v, ctx)| match v {
            Value::List(xs) => xs.into_iter().map(|x| (x, ctx.clone())).collect(),
            v => vec![(v, ctx)],
        })
        .collect())
}

fn shown_path(path: &str) -> String {
    match path.is_empty() {
        true => "the document".to_string(),
        false => format!("`{path}`"),
    }
}

const PREFIX: &str = "table.";

/// The extern that reads `table` as `format` (`git`: resolves its ref).
pub fn extern_name(format: &str, table: &str) -> String {
    format!("{PREFIX}{format}.{table}")
}

/// A table extern's format and table.
fn parse_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(PREFIX)?.split_once('.')
}

/// What a table extern reads, for messages: `input relation p`, `set`
/// (`set from DOC`), `yaml document` (a loader's call, R-129).
pub fn describe(name: &str) -> Option<String> {
    Some(match parse_name(name)? {
        (_, SET_DOC) => "set".into(),
        (f, t) if t.strip_suffix(".git").unwrap_or(t) == DOCUMENT => format!("{f} document"),
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
    let s = |i: usize| a.args.get(i).and_then(Term::as_str);
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
        if format == VALUE {
            return Some(self.value_rows(f, table, inputs));
        }
        let strs: Option<Vec<&str>> = inputs.iter().map(Value::as_str).collect();
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
            Source::Git { .. } => inputs[1].as_str().unwrap_or_default().to_string(),
        };
        let (name, selector) = table.split_once('|').unwrap_or((table, ""));
        self.read
            .borrow_mut()
            .insert((name.to_string(), source), stamp);
        let at = |line: Option<usize>, n: usize| match line {
            Some(l) => format!("{shown}:{l}"),
            None => format!("{shown}:row {n}"),
        };
        // A loader call: the whole document, one row.
        if name == DOCUMENT || name.starts_with("document.") {
            let doc = document_of(format, &text).with_context(|| shown.clone())?;
            let outs = vec![Value::Str(shown.clone()), doc];
            return Ok(vec![externs::row(f, inputs, outs)]);
        }
        if name == SET_DOC {
            let leaves = match selector {
                "" => leaves(format, &text).with_context(|| shown.clone())?,
                _ => {
                    let doc = document_of(format, &text).with_context(|| shown.clone())?;
                    selected_leaves(doc, selector).with_context(|| shown.clone())?
                }
            };
            return Ok(leaf_rows(f, inputs, &shown, leaves));
        }
        let rows = match selector {
            "" => rows(format, name, &text).with_context(|| shown.clone())?,
            _ => {
                let doc = document_of(format, &text).with_context(|| shown.clone())?;
                selected_rows(doc, selector, format == "csv").with_context(|| shown.clone())?
            }
        };
        typed_rows(f, inputs, name, rows, at)
    }

    /// A table read from a value, `table.value.p(+doc, -at, ..)`: the rows
    /// its selector names, each at its position.
    fn value_rows(&self, f: &ExternFn, table: &str, inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        let (name, selector) = table.split_once('|').unwrap_or((table, ""));
        let doc = inputs.first().cloned().unwrap_or(Value::List(Vec::new()));
        let shown = format!("{name} from a value");
        if name == SET_DOC {
            let leaves = selected_leaves(doc, selector)?;
            return Ok(leaf_rows(f, inputs, &shown, leaves));
        }
        let rows = selected_rows(doc, selector, false)?;
        typed_rows(f, inputs, name, rows, |_, n| format!("{shown}: row {n}"))
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

/// The rows of a document of inputs, `(at, path, value)` each.
fn leaf_rows(
    f: &ExternFn,
    inputs: &[Value],
    shown: &str,
    leaves: Vec<(Option<usize>, String, Value)>,
) -> Vec<Vec<Value>> {
    leaves
        .into_iter()
        .map(|(line, path, value)| {
            let at = line.map_or(shown.to_string(), |l| format!("{shown}:{l}"));
            externs::row(f, inputs, vec![Value::Str(at), Value::Str(path), value])
        })
        .collect()
}

/// Each row read by the table's columns, the extern's outputs after `at`:
/// a cell as its column's type, a column the row lacks from the nearest
/// enclosing object that has it, and anything else an error naming the
/// row.
fn typed_rows(
    f: &ExternFn,
    inputs: &[Value],
    table: &str,
    rows: Vec<Row>,
    at: impl Fn(Option<usize>, usize) -> String,
) -> Result<Vec<Vec<Value>>> {
    let cols = &f.args[inputs.len() + 1..];
    let mut out = Vec::new();
    for (i, r) in rows.into_iter().enumerate() {
        let at = at(r.line, i + 1);
        let mut cells = r.cells;
        let mut outs = vec![Value::Str(at.clone())];
        for c in cols {
            let cell = match cells.remove(&c.name) {
                Some(cell) => cell,
                None => match r.ctx.iter().rev().find_map(|o| o.get(&c.name)) {
                    Some(v) => Cell::Value(v.clone()),
                    None => bail!("{at}: no column {}", c.name),
                },
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

/// The rows a selector names in a document: each an object; a CSV
/// document's cells are text.
fn selected_rows(doc: Value, selector: &str, text: bool) -> Result<Vec<Row>> {
    select(doc, selector)?
        .into_iter()
        .enumerate()
        .map(|(i, (v, ctx))| {
            let Value::Obj(m) = v else {
                bail!("row {}: a row is an object, not {}", i + 1, fmt_value(&v));
            };
            let cells = m
                .into_iter()
                .map(|(k, v)| match (text, v) {
                    (true, Value::Str(s)) => (k, Cell::Text(s)),
                    (_, v) => (k, Cell::Value(v)),
                })
                .collect();
            let ctx = ctx
                .into_iter()
                .filter_map(|o| match o {
                    Value::Obj(m) => Some(m),
                    _ => None,
                })
                .collect();
            Ok(Row {
                line: None,
                cells,
                ctx,
            })
        })
        .collect()
}

/// The leaves under what a selector names in a document, by their paths
/// from there.
fn selected_leaves(doc: Value, selector: &str) -> Result<Vec<(Option<usize>, String, Value)>> {
    let mut out = Vec::new();
    for (v, _) in select(doc, selector)? {
        let Value::Obj(m) = v else {
            bail!("a document of inputs is a mapping, not {}", fmt_value(&v));
        };
        for (k, v) in m {
            flatten(&mut out, None, k, v);
        }
    }
    Ok(out)
}

/// A whole document as one value (a loader call, `yaml(path)`): a CSV one
/// is a list of objects by its header, every cell text.
pub fn document_of(format: &str, text: &str) -> Result<Value> {
    if format != "csv" {
        return document(format, text);
    }
    let mut r = csv::Reader::from_reader(text.as_bytes());
    let header = r.headers()?.clone();
    let mut out = Vec::new();
    for rec in r.records() {
        let rec = rec?;
        out.push(Value::Obj(
            header
                .iter()
                .zip(rec.iter())
                .map(|(k, v)| (k.to_string(), Value::Str(v.to_string())))
                .collect(),
        ));
    }
    Ok(Value::List(out))
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

/// A cell read as its column's type: CSV text as an `int`, a `float` or a
/// `bool` when it spells one, a whole float as an `int` and an int as a
/// `float`, a string as an `inet` when it parses as one;
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
            "float" | "number" => match (s.parse().map(Value::Int), crate::value::Float::parse(&s))
            {
                (Ok(i), _) if name == "number" => i,
                (_, Ok(f)) => Value::Float(f),
                _ => Value::Str(s),
            },
            "bool" if s == "true" || s == "false" => Value::Bool(s == "true"),
            _ => Value::Str(s),
        },
    };
    match (name, v) {
        // A whole float is the int it names, an int the float.
        ("int", Value::Float(f)) if f.get().fract() == 0.0 && f.get().abs() < 9.0e15 => {
            Value::Int(f.get() as i64)
        }
        ("float", Value::Int(i)) => {
            crate::value::Float::new(i as f64).map_or(Value::Int(i), Value::Float)
        }
        ("inet", Value::Str(s)) => match crate::value::parse_ipnet(&s) {
            Some((addr, prefix)) => Value::IpNet { addr, prefix },
            None => Value::Str(s),
        },
        (_, v) => v,
    }
}

/// One row: its line when the format says, its cells by column, and the
/// objects that enclose it in its document, innermost last (a selector's).
struct Row {
    line: Option<usize>,
    cells: BTreeMap<String, Cell>,
    ctx: Vec<BTreeMap<String, Value>>,
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
                    ctx: Vec::new(),
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
                        .filter_map(|(k, v)| {
                            json(v)
                                .map(|v| v.map(|v| (k.clone(), Cell::Value(v))))
                                .transpose()
                        })
                        .collect::<Result<_>>()
                        .with_context(|| format!("line {line}"))?;
                    Ok(Row {
                        line: Some(line),
                        cells,
                        ctx: Vec::new(),
                    })
                })
                .collect()
        }
        "yaml" => {
            let (items, lines) = match yaml_stream(text)? {
                // A stream of documents (`---`): a row per document, at
                // the line it starts on.
                Stream::Many(docs, starts) => (docs, starts),
                Stream::One(serde_yaml::Value::Sequence(items)) => {
                    // Block style: an item's line is the line its `- ` is on.
                    let dashes: Vec<usize> = text
                        .lines()
                        .enumerate()
                        .filter(|(_, l)| l == &"-" || l.starts_with("- "))
                        .map(|(i, _)| i + 1)
                        .collect();
                    let lines = (dashes.len() == items.len()).then_some(dashes);
                    (items, lines)
                }
                Stream::One(_) => bail!(
                    "a YAML table is a list of mappings, one per row, or a stream of \
                     mappings (`---`), one per document"
                ),
            };
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    let line = lines.as_ref().map(|ls| ls[i]);
                    let serde_yaml::Value::Mapping(m) = item else {
                        bail!("row {}: a row is a mapping", i + 1);
                    };
                    let row = yaml(serde_yaml::Value::Mapping(m), text)
                        .with_context(|| format!("row {}", i + 1))?;
                    let Some(Value::Obj(row)) = row else {
                        unreachable!("a mapping reads as an object")
                    };
                    let cells = row.into_iter().map(|(k, v)| (k, Cell::Value(v))).collect();
                    Ok(Row {
                        line,
                        cells,
                        ctx: Vec::new(),
                    })
                })
                .collect()
        }
        "toml" => {
            // A TOML document's rows of `p` are its `[[p]]` tables; the
            // document may hold other relations' too (R-39).
            let mut doc: toml::Table = toml::from_str(text)?;
            let items = match doc.remove(table) {
                None => Vec::new(),
                Some(toml::Value::Array(xs)) => xs,
                Some(_) => bail!("{table} is not `[[{table}]]`: a TOML table is its rows"),
            };
            let header = format!("[[{table}]]");
            let lines: Vec<usize> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| l.trim() == header)
                .map(|(i, _)| i + 1)
                .collect();
            let lines = (lines.len() == items.len()).then_some(lines);
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    let toml::Value::Table(t) = item else {
                        bail!("row {}: a row is a table", i + 1);
                    };
                    let cells = t
                        .into_iter()
                        .map(|(k, v)| Ok((k, Cell::Value(toml_value(v)?))))
                        .collect::<Result<_>>()
                        .with_context(|| format!("row {}", i + 1))?;
                    Ok(Row {
                        line: lines.as_ref().map(|ls| ls[i]),
                        cells,
                        ctx: Vec::new(),
                    })
                })
                .collect()
        }
        f => bail!("unknown format {f}"),
    }
}

/// A whole document as one value (`--set k=@FILE`, `json.decode`):
/// `yaml`, `json` or `toml`.
pub fn document(format: &str, text: &str) -> Result<Value> {
    match format {
        "json" => present(json(&serde_json::from_str(text)?)?),
        "yaml" => match yaml_stream(text)? {
            Stream::One(doc) => present(yaml(doc, text)?),
            // A stream of documents (`---`, a manifest): the list of them.
            Stream::Many(docs, _) => Ok(Value::List(
                docs.into_iter()
                    .map(|d| present(yaml(d, text)?))
                    .collect::<Result<_>>()?,
            )),
        },
        "toml" => toml_value(toml::from_str(text)?),
        f => bail!("unknown format {f}"),
    }
}

/// A document of inputs's leaves: (line, dotted path, value). A mapping's
/// nested mappings are walked to their leaves; a CSV one has the columns
/// `path` and `value`.
fn leaves(format: &str, text: &str) -> Result<Vec<(Option<usize>, String, Value)>> {
    let mut out = Vec::new();
    match format {
        "csv" => {
            for r in rows("csv", SET_DOC, text)? {
                let mut cells = r.cells;
                let (Some(Cell::Text(p)), Some(Cell::Text(v)), true) = (
                    cells.remove("path"),
                    cells.remove("value"),
                    cells.is_empty(),
                ) else {
                    bail!("a CSV document of inputs has the columns path and value");
                };
                flatten(&mut out, r.line, p, Value::Str(v));
            }
        }
        "json" => {
            let m: BTreeMap<String, &serde_json::value::RawValue> =
                serde_json::from_str(text).context("a JSON document of inputs is an object")?;
            for (k, raw) in m {
                let line = line_of(text, raw.get().as_ptr() as usize - text.as_ptr() as usize);
                let v: serde_json::Value = serde_json::from_str(raw.get())?;
                if let Some(v) = json(&v).with_context(|| format!("line {line}"))? {
                    flatten(&mut out, Some(line), k, v);
                }
            }
        }
        "yaml" => {
            let Stream::One(serde_yaml::Value::Mapping(m)) = yaml_stream(text)? else {
                bail!("a YAML document of inputs is a mapping");
            };
            let lines = yaml_key_lines(text);
            for (k, v) in m {
                let k = yaml_key(&k)?;
                let Some(v) = yaml(v, text).with_context(|| k.clone())? else {
                    continue;
                };
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
                let v = toml_value(v.into_inner()).with_context(|| format!("line {line}"))?;
                flatten(&mut out, Some(line), k, v);
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

// One reading of a document's values, for a table's rows, `set from`,
// a loader call and `json.decode`, `yaml.decode`, `toml.decode` alike
// (the url ticket's decisions): a number written as an integer is an
// int, one with a fraction or an exponent a float (R-75: `2` is an int,
// `1.5` and `2.0` floats; NaN and the infinities are errors); `null` is no value,
// so an object's member that is null is absent and a null anywhere else
// is an error; a YAML tag is an error naming its line; a TOML datetime is
// a `time` (one with an offset: a local one is an error); a YAML key that
// is a number or a bool is its text.

/// A number as a value: an int, or a float.
fn number(text: String, int: Option<i64>, float: Option<f64>) -> Result<Value> {
    if let Some(i) = int {
        return Ok(Value::Int(i));
    }
    match float.and_then(crate::value::Float::new) {
        Some(f) => Ok(Value::Float(f)),
        None => bail!("{text} is not a number a value holds (a finite int or float)"),
    }
}

/// A null where a value must be (a list's element, the document itself).
fn present(v: Option<Value>) -> Result<Value> {
    v.ok_or_else(|| anyhow!("null is not a value (a null member of an object is absent)"))
}

/// A JSON value; `None` for `null`.
fn json(j: &serde_json::Value) -> Result<Option<Value>> {
    use serde_json::Value as J;
    Ok(Some(match j {
        J::Null => return Ok(None),
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => number(n.to_string(), n.as_i64(), n.as_f64())?,
        J::String(s) => Value::Str(s.clone()),
        J::Array(xs) => Value::List(
            xs.iter()
                .map(|x| present(json(x)?))
                .collect::<Result<_>>()?,
        ),
        J::Object(m) => Value::Obj(
            m.iter()
                .filter_map(|(k, v)| json(v).map(|v| v.map(|v| (k.clone(), v))).transpose())
                .collect::<Result<_>>()?,
        ),
    }))
}

fn yaml_key(k: &serde_yaml::Value) -> Result<String> {
    match k {
        serde_yaml::Value::String(s) => Ok(s.clone()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("a key is a string"),
    }
}

/// A YAML text: one document, or a stream of several (`---`), each with
/// the line it starts on when the separators say. An empty document of a
/// stream (`---` twice, a trailing `---`) is no document.
pub enum Stream {
    One(serde_yaml::Value),
    Many(Vec<serde_yaml::Value>, Option<Vec<usize>>),
}

/// Read `text` as a YAML stream ([`Stream`]).
pub fn yaml_stream(text: &str) -> Result<Stream> {
    use serde::Deserialize;
    let docs = serde_yaml::Deserializer::from_str(text)
        .map(serde_yaml::Value::deserialize)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // Where each document starts: the first line, and the line after each
    // `---`.
    let mut starts = vec![1];
    starts.extend(
        text.lines()
            .enumerate()
            .filter(|(_, l)| *l == "---" || l.starts_with("--- "))
            .map(|(i, _)| i + 2),
    );
    if text.lines().next().is_some_and(|l| l.starts_with("---")) {
        starts.remove(0);
    }
    let starts = (starts.len() == docs.len()).then_some(starts);
    let (docs, starts): (Vec<_>, Option<Vec<_>>) = match starts {
        Some(s) => {
            let (d, s): (Vec<_>, Vec<_>) = docs
                .into_iter()
                .zip(s)
                .filter(|(d, _)| !d.is_null())
                .unzip();
            (d, Some(s))
        }
        None => (docs.into_iter().filter(|d| !d.is_null()).collect(), None),
    };
    Ok(match <[_; 1]>::try_from(docs) {
        Ok([doc]) => Stream::One(doc),
        Err(docs) if docs.is_empty() => Stream::One(serde_yaml::Value::Null),
        Err(docs) => Stream::Many(docs, starts),
    })
}

/// A YAML value; `None` for `null`. `text` is the document, for the line
/// of a tag.
fn yaml(v: serde_yaml::Value, text: &str) -> Result<Option<Value>> {
    use serde_yaml::Value as Y;
    Ok(Some(match v {
        Y::Null => return Ok(None),
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => number(n.to_string(), n.as_i64(), n.as_f64())?,
        Y::String(s) => Value::Str(s),
        Y::Sequence(xs) => Value::List(
            xs.into_iter()
                .map(|x| present(yaml(x, text)?))
                .collect::<Result<_>>()?,
        ),
        Y::Mapping(m) => Value::Obj(
            m.into_iter()
                .filter_map(|(k, v)| match (yaml_key(&k), yaml(v, text)) {
                    (Ok(k), Ok(Some(v))) => Some(Ok((k, v))),
                    (_, Ok(None)) => None,
                    (Err(e), _) | (_, Err(e)) => Some(Err(e)),
                })
                .collect::<Result<_>>()?,
        ),
        Y::Tagged(t) => {
            let tag = t.tag.to_string();
            match text.find(&tag) {
                Some(at) => bail!(
                    "line {}: a tag ({tag}) is not a value; convert in a rule",
                    line_of(text, at)
                ),
                None => bail!("a tag ({tag}) is not a value; convert in a rule"),
            }
        }
    }))
}

/// A TOML value (TOML has no null).
fn toml_value(v: toml::Value) -> Result<Value> {
    use toml::Value as T;
    Ok(match v {
        T::String(s) => Value::Str(s),
        T::Integer(i) => Value::Int(i),
        T::Float(f) => number(f.to_string(), None, Some(f))?,
        T::Boolean(b) => Value::Bool(b),
        T::Datetime(d) => {
            Value::Time(crate::time::Time::parse(&d.to_string()).map_err(|_| {
                anyhow!("{d} is not a time: a time has an offset (`{d}Z`) or a zone")
            })?)
        }
        T::Array(xs) => Value::List(xs.into_iter().map(toml_value).collect::<Result<_>>()?),
        T::Table(m) => Value::Obj(
            m.into_iter()
                .map(|(k, v)| Ok((k, toml_value(v)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// `set from DOC`, lowered (`transform::lower`): the rule the
/// resolver wrote, `arg(input, S, P, V, Rank) :- ..., table.F.set(..,
/// At, P, V)`, contributes at a path only a row knows, which would make
/// every input cell one partition (`partition`). So it becomes one rule per
/// input the scope gives, `P` its path and the head its cell (R-38): in the
/// stack (`S` is `""`) every input it addresses but a key, its own and its used
/// modules' (`db.backup_days`, `traefik.acme_email`), in a module its own;
/// a string read as the input's type by its constructor (`inet(V)`). A
/// map input (`labels: map(string)`) has a second rule, for the leaves
/// under it: the key's entry at the input's path (`inputs::map_entry_lits`),
/// so it stays one cell. A leaf at any other path is a deny naming it,
/// where the file has it, and the inputs there are.
/// Each of those inputs is one the program gives (`Declared::given`).
pub fn expand_set_from(program: Program, declared: &mut [crate::inputs::Declared]) -> Program {
    let is_doc = |l: &Lit| {
        matches!(l, Lit::Pos(a) if parse_name(&a.pred).is_some_and(|(f, t)| {
            f != "git" && t.split('|').next() == Some(SET_DOC)
        }))
    };
    let stack = program.stack;
    let (docs, mut out): (Vec<Stmt>, Vec<Stmt>) = program
        .statements
        .into_iter()
        .partition(|s| matches!(s, Stmt::Rule(r) if r.body.iter().any(is_doc)));
    const KNOWN: &str = "__set_path";
    let mut known = BTreeSet::new();
    for s in docs {
        let Stmt::Rule(r) = s else { continue };
        let (
            Some(Term::Val(Value::Str(scope))),
            Some(Term::Var(p)),
            Some(Term::Var(v)),
            Some(Lit::Pos(ext)),
        ) = (
            r.head.args.get(1),
            r.head.args.get(2),
            r.head.args.get(3),
            r.body.iter().find(|l| is_doc(l)),
        )
        else {
            continue;
        };
        // (the path the document gives it by, the cell's scope, its leaf).
        let given = |d: &crate::inputs::Declared| {
            !d.decl.key
                && if scope.is_empty() {
                    d.address.is_some()
                } else {
                    &d.scope == scope
                }
        };
        for d in declared.iter_mut().filter(|d| given(d)) {
            d.given = true;
        }
        let inputs: Vec<(&str, &str, &crate::inputs::Declared)> = declared
            .iter()
            .filter(|d| given(d))
            .filter_map(|d| match scope.is_empty() {
                true => Some((d.address.as_deref()?, d.scope.as_str(), d)),
                false => Some((d.decl.name.as_str(), scope.as_str(), d)),
            })
            .collect();
        let at = ext.args[ext.args.len() - 3].clone();
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        for (path, cell, d) in &inputs {
            let mut head = subst(&r.head, p, &s(path));
            head.args[1] = s(cell);
            let mut body: Vec<Lit> = r
                .body
                .iter()
                .map(|l| match l {
                    Lit::Pos(a) => Lit::Pos(subst(a, p, &s(path))),
                    l => l.clone(),
                })
                .collect();
            let csv = parse_name(&ext.pred).is_some_and(|(f, _)| f == "csv");
            if let Some((parsed, l)) = read_leaf(&d.decl.ty, v, csv) {
                body.push(l);
                head.args[3] = Term::Var(parsed);
            }
            // The core form's path, as `transform::lower_contributions`
            // normalizes every other contribution's.
            let (top, value) = crate::transform::normalize_contribution(
                crate::modules::INPUT,
                &d.decl.name,
                head.args[3].clone(),
            );
            head.args[2] = s(&top);
            head.args[3] = value;
            out.push(Stmt::Rule(RuleStmt { head, body }));
            known.insert((scope.clone(), path.to_string()));
            // A leaf under a map input (`labels.team`) is its key: the
            // entry `{team: V}` at the input's own path, so the input
            // stays one cell (the path is not the row's).
            if let Some(elem) = crate::inputs::map_values(&d.decl.ty) {
                let mut head = r.head.clone();
                head.args[1] = s(cell);
                let read = read_leaf(elem, v, csv);
                let leaf = read
                    .as_ref()
                    .map_or(v.clone(), |(parsed, _)| parsed.clone());
                let entry = format!("{v}__entry");
                let [under, at] = crate::inputs::map_entry_lits(
                    &Term::Var(p.clone()),
                    path,
                    Term::Var(leaf),
                    &entry,
                );
                // The key's test before its value is read as the type.
                let mut body = r.body.clone();
                body.push(under);
                body.extend(read.map(|(_, l)| l));
                body.push(at);
                let (top, value) = crate::transform::normalize_contribution(
                    crate::modules::INPUT,
                    &d.decl.name,
                    Term::Var(entry),
                );
                head.args[2] = s(&top);
                head.args[3] = value;
                out.push(Stmt::Rule(RuleStmt { head, body }));
            }
        }
        let mut body = r.body.clone();
        // A map input's keys are no typo.
        for (path, _, d) in &inputs {
            if crate::inputs::is_map(&d.decl.ty) {
                body.push(Lit::Not(atom(
                    "str.starts_with",
                    vec![Term::Var(p.clone()), s(&format!("{path}."))],
                    r.head.span,
                )));
            }
        }
        body.push(Lit::Not(atom(
            KNOWN,
            vec![s(scope), Term::Var(p.clone())],
            r.head.span,
        )));
        let names: Vec<&str> = inputs.iter().map(|(p, _, _)| *p).collect();
        let message = Term::Func {
            name: "format".into(),
            args: vec![
                s(&format!(
                    "%s: %s is not an input{} ({})",
                    if scope.is_empty() {
                        String::new()
                    } else {
                        format!(" of {scope}")
                    },
                    match names.is_empty() {
                        true => "it declares none".to_string(),
                        false => format!("its inputs: {}", names.join(", ")),
                    }
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
    for (scope, path) in known {
        out.push(Stmt::Fact(atom(
            KNOWN,
            vec![Term::Val(Value::Str(scope)), Term::Val(Value::Str(path))],
            Span::default(),
        )));
    }
    Program {
        statements: out,
        stack,
    }
}

/// A document's leaf `v` read as the input's type `ty` by its
/// constructor: a CIDR, a quantity, a time, a float (from an int too);
/// and a CSV cell, all text, as an int too. The variable it binds and
/// the literal that binds it; none where the leaf is taken as it is.
fn read_leaf(ty: &TypeExpr, v: &str, csv: bool) -> Option<(String, Lit)> {
    let TypeExpr::Name(n) = ty else {
        return None;
    };
    let read = matches!(
        n.as_str(),
        "inet" | "ip" | "float" | "bytes" | "cpu" | "duration" | "time"
    ) || (csv && n == "int");
    read.then(|| {
        let parsed = format!("{v}__{n}");
        let l = Lit::Eq(
            Term::Var(parsed.clone()),
            Term::Func {
                name: n.clone(),
                args: vec![Term::Var(v.to_string())],
            },
        );
        (parsed, l)
    })
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
                a.inputs[0].as_str()?,
                a.inputs[1].as_str()?,
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
