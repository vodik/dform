//! `dform query` and the printing `dform why` shares with it: a pattern or a
//! conjunction over the final fact store, one column per variable, secrets
//! redacted.
//!
//! Redaction is by value. A value at a `sensitive` schema path of `arg`,
//! `attr`, `world_attr`, `cloud_attr` or `cloud_computed`, or of an input or
//! output declared `secret(T)` (`secret_cell/3`), is a secret, and so is
//! every scalar inside it; any printed value equal to a secret, or a
//! string containing a secret string, prints as `(sensitive T/A#P)`. A
//! `secret` null prints as its label the same way. So a rule that forwards a
//! secret into another predicate does not leak it either.

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

/// Parse `pred`, or body literals such as `attr(T, A, cidr, C), want(T, A)`,
/// with the program's own parser.
pub fn parse(src: &str) -> Result<Query> {
    let src = src.trim().trim_end_matches('.').trim();
    if !src.is_empty()
        && src
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Ok(Query::Pred(src.to_string()));
    }
    let program = crate::parser::parse_program(&format!("query__(0) :- {src} ."))
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
    /// Columns padded to their widest cell; a ground query prints `yes` or
    /// `no`.
    pub fn render(&self, r: &Redactor) -> String {
        if self.vars.is_empty() {
            return if self.rows.is_empty() {
                "no\n"
            } else {
                "yes\n"
            }
            .into();
        }
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| row.iter().map(|v| r.fmt(v)).collect())
            .collect();
        let mut width: Vec<usize> = self.vars.iter().map(|v| v.chars().count()).collect();
        for row in &cells {
            for (w, c) in width.iter_mut().zip(row) {
                *w = (*w).max(c.chars().count());
            }
        }
        let line = |cols: Vec<&str>| {
            let mut s = String::new();
            for (i, (c, w)) in cols.iter().zip(&width).enumerate() {
                if i + 1 == cols.len() {
                    s.push_str(c);
                } else {
                    s.push_str(&format!("{c:<w$}  "));
                }
            }
            s.push('\n');
            s
        };
        let mut out = line(self.vars.iter().map(String::as_str).collect());
        for row in &cells {
            out.push_str(&line(row.iter().map(String::as_str).collect()));
        }
        let n = self.rows.len();
        out.push_str(&format!("({n} row{})\n", if n == 1 { "" } else { "s" }));
        out
    }

    /// A JSON array of objects keyed by variable, secrets as
    /// `{"sensitive": label}`, nulls as `{"null": label, "class": c}`.
    pub fn json(&self, r: &Redactor) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    serde_json::Value::Object(
                        self.vars
                            .iter()
                            .zip(row)
                            .map(|(k, v)| (k.clone(), r.json(v)))
                            .collect(),
                    )
                })
                .collect(),
        )
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

    /// `partition::fmt_value`, with secrets as `(sensitive T/A#P)` and
    /// nulls as `?label`.
    pub fn fmt(&self, v: &Value) -> String {
        if let Some(l) = self.secret(v) {
            return format!("(sensitive {l})");
        }
        match v {
            Value::List(xs) => format!(
                "[{}]",
                xs.iter()
                    .map(|x| self.fmt(x))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Obj(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, x)| format!("{k}: {}", self.fmt(x)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Null { label, .. } => format!("?{label}"),
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

    /// Any text that may quote a program literal (a rule's text): every
    /// secret string in it replaced.
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (k, l) in &self.secrets {
            if let Value::Str(k) = k {
                out = out.replace(&format!("{k:?}"), &format!("(sensitive {l})"));
                out = out.replace(k.as_str(), &format!("(sensitive {l})"));
            }
        }
        out
    }

    pub fn json(&self, v: &Value) -> serde_json::Value {
        if let Some(l) = self.secret(v) {
            return serde_json::json!({ "sensitive": l });
        }
        match v {
            Value::List(xs) => serde_json::Value::Array(xs.iter().map(|x| self.json(x)).collect()),
            Value::Obj(m) => serde_json::Value::Object(
                m.iter().map(|(k, x)| (k.clone(), self.json(x))).collect(),
            ),
            Value::Null { label, class, .. } => {
                serde_json::json!({"null": label, "class": class.name()})
            }
            v => engine::value_to_json(v),
        }
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
        let f = facts("p(a, 1). p(b, 2). q(b).");
        let Query::Body { body, vars } = parse("p(X, N), q(X)").unwrap() else {
            panic!()
        };
        assert_eq!(vars, ["X", "N"]);
        let t = table(&body, &vars, &f).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b".into()), Value::Int(2)]]);
        assert_eq!(
            t.render(&Redactor::default()),
            "X    N\n\"b\"  2\n(1 row)\n"
        );
        assert!(matches!(parse("p").unwrap(), Query::Pred(p) if p == "p"));
        assert_eq!(
            table(&body, &[], &f).unwrap().render(&Redactor::default()),
            "yes\n"
        );
    }

    #[test]
    fn json_is_an_array_of_objects_keyed_by_variable() {
        let t = Table {
            vars: vec!["N".into(), "C".into()],
            rows: vec![vec![Value::Str("a".into()), Value::Int(1)]],
        };
        assert_eq!(
            t.json(&Redactor::default()),
            serde_json::json!([{"N": "a", "C": 1}])
        );
    }

    #[test]
    fn a_secret_prints_as_its_label_wherever_it_is_forwarded() {
        let schema = Schema::from_facts(
            &crate::parser::parse_program("type_attr(v, pw, string, [sensitive]).")
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
            r#"want(v, a). arg(v, a, pw, "hunter22", normal).
               leak(S) :- attr(v, a, pw, P), S = concat("pw=", P)."#,
        );
        let r = Redactor::new(&f, &schema);
        let Query::Body { body, vars } = parse("leak(S)").unwrap() else {
            panic!()
        };
        let out = table(&body, &vars, &f).unwrap().render(&r);
        assert!(!out.contains("hunter22"), "{out}");
        assert!(out.contains("(sensitive v/a#pw)"), "{out}");
    }
}
