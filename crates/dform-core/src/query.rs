//! `dform query` and the value spelling every printer shares: a pattern or
//! a conjunction over the final fact store, one column per variable,
//! secrets redacted.
//!
//! Redaction is by value. A value at a `sensitive` schema path of `arg`,
//! `attr`, `world_attr`, `cloud_attr` or `cloud_computed`, or of an input or
//! output declared `secret(T)` (`secret_cell/3`), is a secret, and so is
//! every scalar inside it; any printed value equal to a secret, or a
//! string containing a secret string, prints as `(sensitive T["A"].p)`, or
//! in a result set's cell as `secret(SIZE)`. A `secret` null prints as its
//! label the same way. So a rule that forwards a secret into another
//! predicate does not leak it either. The result set itself is
//! `report::table`'s.

use crate::ast::{Atom, Lit, Term};
use crate::engine;
use crate::partition;
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// What `dform query ARG` asks.
#[derive(Debug, Clone)]
pub enum Query {
    /// A bare predicate name: every fact of it.
    Pred(String),
    /// A conjunction of body literals; `vars` in order of first appearance.
    Body { body: Vec<Lit>, vars: Vec<String> },
}

/// An address as `plan` prints it (H-16, `ir::parse_address`), as a
/// pattern: `T["A"].p` is its attribute, `attr(T, A, "p", value)`; `T["A"]`
/// is the resource, `want(T, A)` for `why` and every `attr(T, A, path,
/// value)` for `query`.
pub fn address(src: &str, why: bool) -> Option<Query> {
    let (addr, path) = crate::ir::parse_address(src).ok()?;
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let v = |x: &str| Term::Var(x.to_string());
    let (pred, args) = match (path, why) {
        (Some(p), _) => ("attr", vec![s(&addr.typ), s(&addr.name), s(&p), v("value")]),
        (None, true) => ("want", vec![s(&addr.typ), s(&addr.name)]),
        (None, false) => (
            "attr",
            vec![s(&addr.typ), s(&addr.name), v("path"), v("value")],
        ),
    };
    let atom = Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let mut vars = Vec::new();
    atom.args.iter().for_each(|t| term_vars(t, &mut vars));
    Some(Query::Body {
        body: vec![Lit::Pos(atom)],
        vars,
    })
}

/// Parse an address (`address`), `pred`, or body literals such as
/// `attr(t, a, .cidr, c), want(t, a)`, with the program's own parser.
pub fn parse(src: &str) -> Result<Query> {
    if let Some(q) = address(src, false) {
        return Ok(q);
    }
    let src = src.trim().trim_end_matches('.').trim();
    if !src.is_empty()
        && src
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Ok(Query::Pred(src.to_string()));
    }
    let program = crate::parser::parse_pattern(&format!("query__(0) where {src}"))
        .map_err(|e| anyhow::anyhow!("cannot parse query '{src}': {e:#}"))?;
    let [crate::ast::Stmt::Rule(r)] = program.statements.as_slice() else {
        bail!("cannot parse query '{src}': expected body literals");
    };
    let mut vars = Vec::new();
    for lit in &r.body {
        lit_vars(lit, &mut vars);
    }
    Ok(Query::Body {
        body: r.body.clone(),
        vars,
    })
}

fn lit_vars(l: &Lit, out: &mut Vec<String>) {
    match l {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| term_vars(t, out)),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            term_vars(a, out);
            term_vars(b, out);
        }
    }
}

fn term_vars(t: &Term, out: &mut Vec<String>) {
    match t {
        Term::Var(v) => {
            if !v.starts_with('_') && !out.contains(v) {
                out.push(v.clone());
            }
        }
        Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| term_vars(a, out)),
        Term::Obj(m) => m.values().for_each(|a| term_vars(a, out)),
        Term::Val(_) | Term::Wildcard | Term::ListComp { .. } => {}
    }
}

/// The columns of the core's relations a bare predicate may name.
const CORE_COLUMNS: &[(&str, &[&str])] = &[
    ("want", &["type", "address"]),
    ("input", &["name", "value"]),
    ("attr", &["type", "address", "path", "value"]),
    ("arg", &["type", "address", "path", "value", "rank"]),
    ("world_attr", &["type", "address", "path", "value"]),
    ("cloud_attr", &["type", "address", "path", "value"]),
    ("deformation", &["kind", "resource", "before"]),
    ("world_digest", &["resource", "now"]),
    ("type_attr", &["type", "path", "ty", "flags"]),
    ("type_provider", &["type", "provider"]),
];

/// The columns `dform query PRED` prints `pred`'s facts of `arity` under:
/// its `decl`'s fields, a core relation's, else `a`, `b`, .. as an
/// undeclared relation's columns are named (`transform::columns`).
pub fn columns(pred: &str, arity: usize, program: &crate::ast::Program) -> Vec<String> {
    let declared = program.statements.iter().find_map(|s| match s {
        crate::ast::Stmt::Decl(d) if d.pred == pred && d.fields.len() == arity => {
            Some(d.fields.clone())
        }
        _ => None,
    });
    let core = || {
        CORE_COLUMNS
            .iter()
            .find(|(p, cs)| *p == pred && cs.len() == arity)
            .map(|(_, cs)| cs.iter().map(|c| c.to_string()).collect())
    };
    declared.or_else(core).unwrap_or_else(|| {
        crate::transform::columns(arity)
            .split(", ")
            .filter(|c| !c.is_empty())
            .map(String::from)
            .collect()
    })
}

/// One row per distinct binding of the variables, sorted.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub vars: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// Evaluate body literals against the final fact store.
pub fn table(body: &[Lit], vars: &[String], facts: &BTreeSet<Atom>) -> Result<Table> {
    let rows: BTreeSet<Vec<Value>> = engine::query(body, facts)?
        .into_iter()
        .map(|(b, _)| {
            vars.iter()
                .map(|v| b.get(v).cloned().unwrap_or(Value::Str("_".into())))
                .collect()
        })
        .collect();
    Ok(Table {
        vars: vars.to_vec(),
        rows: rows.into_iter().collect(),
    })
}

impl Table {
    /// The rows as a result set (`report::table`): one column per
    /// variable, values in surface spelling, secrets redacted.
    pub fn result(&self, r: &Redactor) -> crate::report::table::Table {
        use crate::report::table::{Cell, Table};
        let mut t = Table::new(self.vars.iter().cloned());
        for row in &self.rows {
            t.push(row.iter().map(|v| Cell::value(v, r)).collect());
        }
        t
    }
}

/// Positions `(type, addr, path, value)` of the predicates that carry an
/// attribute value.
const VALUE_PREDS: [&str; 5] = ["arg", "attr", "world_attr", "cloud_attr", "cloud_computed"];

/// Prints values with every secret replaced by its label.
#[derive(Debug, Default, Clone)]
pub struct Redactor {
    /// Secret value -> label `T/A#P`.
    secrets: BTreeMap<Value, String>,
}

impl Redactor {
    pub fn new(facts: &BTreeSet<Atom>, schema: &Schema) -> Redactor {
        let mut r = Redactor::default();
        // Inputs and outputs declared `secret(T)`: (type, scope, key).
        let cells: BTreeSet<(&Value, &Value, &Value)> = facts
            .iter()
            .filter(|a| a.pred == crate::transform::SECRET_CELL)
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(t), Term::Val(s), Term::Val(k)] => Some((t, s, k)),
                _ => None,
            })
            .collect();
        for a in facts {
            if !VALUE_PREDS.contains(&a.pred.as_str()) || a.args.len() < 4 {
                continue;
            }
            let [
                Term::Val(tv),
                Term::Val(addr),
                Term::Val(pv),
                Term::Val(v),
                ..,
            ] = a.args.as_slice()
            else {
                continue;
            };
            let (Some(t), Some(p)) = (tv.as_str(), pv.as_str()) else {
                continue;
            };
            if schema.is_sensitive(t, p) || cells.contains(&(tv, addr, pv)) {
                let addr = match addr {
                    Value::Str(s) => s.clone(),
                    v => partition::fmt_value(v),
                };
                r.add(v, &crate::value::null_label(t, &addr, p));
            }
        }
        // `env.var(NAME)` answers a `secret(string)` (R-60): its value is
        // `env.var/NAME`, as the plan file records it, wherever it goes.
        for a in facts
            .iter()
            .filter(|a| a.pred == crate::syntax::resolve::ENV_VAR)
        {
            if let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() {
                r.add(v, &format!("{}/{name}", a.pred));
            }
        }
        // A memo that keeps a secret: its candidate is one too, of the
        // same label (a new master's password, say, not kept).
        for a in facts.iter().filter(|a| a.pred == crate::memo::FIRST) {
            if let [_, Term::Val(c), Term::Val(v)] = a.args.as_slice()
                && let Some(l) = r.secret(v)
            {
                r.add(c, &l);
            }
        }
        r
    }

    fn add(&mut self, v: &Value, label: &str) {
        match v {
            Value::Null { .. } | Value::Bool(_) => {}
            Value::Str(s) if s.is_empty() => {}
            Value::List(xs) => xs.iter().for_each(|x| self.add(x, label)),
            Value::Obj(m) => m.values().for_each(|x| self.add(x, label)),
            _ => {
                self.secrets
                    .entry(v.clone())
                    .or_insert_with(|| label.to_string());
            }
        }
    }

    /// The label a value prints as, if it is (or contains) a secret.
    fn secret(&self, v: &Value) -> Option<String> {
        if let Value::Null {
            label,
            class: NullClass::Secret,
            ..
        } = v
        {
            return Some(label.clone());
        }
        if let Some(l) = self.secrets.get(v) {
            return Some(l.clone());
        }
        let Value::Str(s) = v else { return None };
        self.secrets.iter().find_map(|(k, l)| match k {
            Value::Str(k) if s.contains(k.as_str()) => Some(l.clone()),
            _ => None,
        })
    }

    pub fn is_secret(&self, v: &Value) -> bool {
        self.secret(v).is_some()
    }

    /// `partition::fmt_value`, with secrets as `(sensitive T["A"].p)` and
    /// nulls as `?T["A"].p` (`ir::label`).
    pub fn fmt(&self, v: &Value) -> String {
        self.spell(v, Spelling::Core)
    }

    /// A value as the program would write it: `fmt`, with a reference as
    /// the address it names, `T["A"]` or `T["A"].p`.
    pub fn surface(&self, v: &Value) -> String {
        self.spell(v, Spelling::Surface)
    }

    /// A value in a result set's cell (`report::table`): `surface`, with a
    /// secret as `secret(SIZE)`, its size and never its label or bytes;
    /// `secret(?)` while its value is unknown.
    pub fn cell(&self, v: &Value) -> String {
        self.spell(v, Spelling::Cell)
    }

    fn spell(&self, v: &Value, how: Spelling) -> String {
        if let Some(l) = self.secret(v) {
            return match (how, v) {
                (Spelling::Cell, Value::Null { .. }) => "secret(?)".into(),
                (Spelling::Cell, Value::Str(s)) => format!("secret({})", size(s.len())),
                (Spelling::Cell, v) => format!("secret({})", size(partition::fmt_value(v).len())),
                _ => format!("(sensitive {})", crate::ir::label(&l)),
            };
        }
        let surface = how != Spelling::Core;
        match v {
            Value::List(xs) => format!(
                "[{}]",
                xs.iter()
                    .map(|x| self.spell(x, how))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Obj(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, x)| format!("{k}: {}", self.spell(x, how)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Null { label, .. } => format!("?{}", crate::ir::label(label)),
            Value::Ref { typ, name, attr } if surface => crate::ir::Address {
                typ: typ.clone(),
                name: name.clone(),
            }
            .attr(attr),
            Value::CloudRef { typ, name, attr } if surface => format!(
                "cloud_ref({}, {}, {})",
                crate::ir::string_literal(typ),
                crate::ir::string_literal(name),
                crate::ir::string_literal(attr)
            ),
            v => partition::fmt_value(v),
        }
    }

    pub fn fmt_atom(&self, a: &Atom) -> String {
        let args: Vec<String> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => self.fmt(v),
                t => partition::fmt_term(t),
            })
            .collect();
        format!("{}({})", a.pred, args.join(", "))
    }

    /// `fmt_atom` with values as the program would write them (`surface`).
    pub fn surface_atom(&self, a: &Atom) -> String {
        let args: Vec<String> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => self.surface(v),
                t => partition::fmt_term(t),
            })
            .collect();
        format!("{}({})", a.pred, args.join(", "))
    }

    /// Any text that may quote a program literal (a rule's text): every
    /// secret string in it replaced.
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (k, l) in &self.secrets {
            if let Value::Str(k) = k {
                let shown = format!("(sensitive {})", crate::ir::label(l));
                out = out.replace(&format!("{k:?}"), &shown);
                out = out.replace(k.as_str(), &shown);
            }
        }
        out
    }

    pub fn json(&self, v: &Value) -> serde_json::Value {
        if let Some(l) = self.secret(v) {
            return serde_json::json!({ "sensitive": crate::ir::label(&l) });
        }
        match v {
            Value::List(xs) => serde_json::Value::Array(xs.iter().map(|x| self.json(x)).collect()),
            Value::Obj(m) => serde_json::Value::Object(
                m.iter().map(|(k, x)| (k.clone(), self.json(x))).collect(),
            ),
            Value::Null { label, class, .. } => {
                serde_json::json!({"null": crate::ir::label(label), "class": class.name()})
            }
            v => engine::value_to_json(v),
        }
    }
}

/// How `Redactor::spell` writes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Core,
    Surface,
    Cell,
}

/// A byte count as a person reads it: `812 B`, `5.1 KB`, `2.0 MB`.
pub fn size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> BTreeSet<Atom> {
        let program = crate::parser::parse_program(src).unwrap();
        engine::eval(&program, &[]).unwrap().0.facts
    }

    #[test]
    fn a_pattern_binds_one_column_per_variable() {
        let f = facts("p(\"a\", 1)\np(\"b\", 2)\nq(\"b\")");
        let Query::Body { body, vars } = parse("p(X, N), q(X)").unwrap() else {
            panic!()
        };
        assert_eq!(vars, ["X", "N"]);
        let t = table(&body, &vars, &f).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b".into()), Value::Int(2)]]);
        assert_eq!(
            t.result(&Redactor::default()).render(&Default::default()),
            "X    N\n\"b\"  2\n"
        );
        assert!(matches!(parse("p").unwrap(), Query::Pred(p) if p == "p"));
        assert_eq!(table(&body, &[], &f).unwrap().rows, vec![Vec::new()]);
    }

    #[test]
    fn json_is_an_array_of_objects_keyed_by_variable() {
        let t = Table {
            vars: vec!["N".into(), "C".into()],
            rows: vec![vec![Value::Str("a".into()), Value::Int(1)]],
        };
        assert_eq!(
            t.result(&Redactor::default()).json(),
            serde_json::json!([{"N": "a", "C": 1}])
        );
    }

    #[test]
    fn a_secret_prints_as_its_size_wherever_it_is_forwarded() {
        let schema = Schema::from_facts(
            &crate::parser::parse_program("type_attr(\"v\", \"pw\", \"string\", [\"sensitive\"])")
                .unwrap()
                .statements
                .iter()
                .filter_map(|s| match s {
                    crate::ast::Stmt::Fact(a) => Some(a.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let f = facts(
            r#"want("v", "a")
arg("v", "a", "pw", "hunter22", "normal")
               leak(s) where attr("v", "a", "pw", p), s = format("pw=%s", p)"#,
        );
        let r = Redactor::new(&f, &schema);
        let Query::Body { body, vars } = parse("leak(S)").unwrap() else {
            panic!()
        };
        let t = table(&body, &vars, &f).unwrap().result(&r);
        let out = t.render(&Default::default());
        assert_eq!(out, "S\nsecret(11 B)\n");
        assert_eq!(
            t.json(),
            serde_json::json!([{"S": {"sensitive": "v[\"a\"].pw"}}])
        );
    }
}
