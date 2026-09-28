//! Typed inputs (DESIGN.org "Typed stack inputs", E §7.1): `input k: T [=
//! D] [where R]` at the top of a program is the stack's interface, in a
//! module the instance's (`modules`).
//!
//! An input is a cell `(input, Scope, k)` of the attribute aggregate. The
//! declared default contributes at `@default`; `--set k=v` (an `input(k, v)`
//! fact) and an `--input-file`'s `k(v).` contribute at the normal rank, so
//! either wins over the default and two different values conflict. A
//! required input with no value is an error naming it; a value of the wrong
//! type is one too (from the command line, before evaluation) or a
//! violation (a value a rule computed).

use crate::ast::{Atom, InputDecl, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::value::Value;
use anyhow::Result;
use std::collections::BTreeSet;

/// A declared input: `scope` is `""` for the stack's own, `m.i` for a
/// module instance's.
#[derive(Debug, Clone)]
pub struct Declared {
    pub scope: String,
    pub decl: InputDecl,
}

impl Declared {
    fn who(&self) -> String {
        if self.scope.is_empty() {
            format!("input {}", self.decl.name)
        } else {
            format!("input {} of {}", self.decl.name, self.scope)
        }
    }
}

pub fn type_text(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Name(n) => n.clone(),
        TypeExpr::Apply(n, args) => format!(
            "{n}({})",
            args.iter().map(type_text).collect::<Vec<_>>().join(", ")
        ),
        TypeExpr::Object(fs) => format!(
            "{{ {} }}",
            fs.iter()
                .map(|(k, t)| format!("{k}: {}", type_text(t)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeExpr::Str(s) => format!("{s:?}"),
    }
}

/// The type names an input may have. `addr`, `ref(...)` and `any` are
/// accepted unchecked.
pub fn check_type(t: &TypeExpr) -> Result<(), String> {
    match t {
        TypeExpr::Name(n) => match n.as_str() {
            "int" | "string" | "bool" | "inet" | "symbol" | "addr" | "any" => Ok(()),
            _ => Err(format!("unknown type {n}")),
        },
        TypeExpr::Apply(n, args) => match (n.as_str(), args.as_slice()) {
            ("enum", xs) if !xs.is_empty() => {
                for x in xs {
                    if !matches!(x, TypeExpr::Name(_) | TypeExpr::Str(_)) {
                        return Err(format!("enum takes names or strings, not {}", type_text(x)));
                    }
                }
                Ok(())
            }
            ("list" | "set", [x]) => check_type(x),
            ("ref", _) => Ok(()),
            _ => Err(format!("unknown type {}", type_text(t))),
        },
        TypeExpr::Object(fs) => fs.iter().try_for_each(|(_, t)| check_type(t)),
        TypeExpr::Str(s) => Err(format!("a type is a name, not the string {s:?}")),
    }
}

/// Does `v` have type `t`? A null is not known yet, so it has every type.
pub fn has_type(t: &TypeExpr, v: &Value) -> bool {
    if matches!(v, Value::Null { .. }) {
        return true;
    }
    match t {
        TypeExpr::Name(n) => match n.as_str() {
            "int" => matches!(v, Value::Int(_)),
            "string" | "symbol" | "addr" => matches!(v, Value::Str(_)),
            "bool" => matches!(v, Value::Bool(_)),
            "inet" => matches!(v, Value::IpNet { .. }),
            _ => true,
        },
        TypeExpr::Apply(n, args) => match (n.as_str(), args.as_slice()) {
            ("enum", xs) => xs.iter().any(|x| match (x, v) {
                (TypeExpr::Name(a) | TypeExpr::Str(a), Value::Str(s)) => a == s,
                _ => false,
            }),
            ("list" | "set", [x]) => match v {
                Value::List(xs) => xs.iter().all(|e| has_type(x, e)),
                _ => false,
            },
            _ => true,
        },
        TypeExpr::Object(fs) => match v {
            Value::Obj(m) => fs
                .iter()
                .all(|(k, t)| m.get(k).is_none_or(|e| has_type(t, e))),
            _ => false,
        },
        TypeExpr::Str(_) => true,
    }
}

/// A command-line value read as the input's type: `--set` gives a string,
/// an int or a bool; an `inet` input parses its string, a `string` input
/// takes the text of an int or a bool.
pub fn coerce(t: &TypeExpr, v: Value) -> Value {
    match (t, v) {
        (TypeExpr::Name(n), Value::Str(s)) if n == "inet" => match crate::value::parse_ipnet(&s) {
            Some((addr, prefix)) => Value::IpNet { addr, prefix },
            None => Value::Str(s),
        },
        (TypeExpr::Name(n), Value::Int(i)) if n == "string" => Value::Str(i.to_string()),
        (TypeExpr::Name(n), Value::Bool(b)) if n == "string" => Value::Str(b.to_string()),
        (_, v) => v,
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => crate::partition::fmt_value(v),
    }
}

/// The type of every `attr(input, Scope, k, V)` of an evaluation: one
/// violation per value that is not its input's type.
pub fn violations(facts: &BTreeSet<Atom>, declared: &[Declared]) -> Vec<String> {
    let mut out = Vec::new();
    for a in facts.iter().filter(|a| a.pred == "attr") {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(scope)),
            Term::Val(Value::Str(k)),
            Term::Val(v),
        ] = a.args.as_slice()
        else {
            continue;
        };
        if t != crate::modules::INPUT {
            continue;
        }
        let Some(d) = declared
            .iter()
            .find(|d| &d.scope == scope && &d.decl.name == k)
        else {
            continue;
        };
        if !has_type(&d.decl.ty, v) {
            out.push(format!(
                "{}: {} is not {}",
                d.who(),
                shown(v),
                type_text(&d.decl.ty)
            ));
        }
    }
    out
}

/// The stack's inputs given on the command line, `--set k=v`, as `input(k,
/// v)` facts read as their declared types. A key the program does not
/// declare, when it declares inputs, is an error; so is a value of the
/// wrong type.
pub fn set_facts(declared: &[Declared], set: &[(String, Value)]) -> Result<Vec<Atom>> {
    let own: Vec<&Declared> = declared.iter().filter(|d| d.scope.is_empty()).collect();
    let mut out = Vec::new();
    for (k, v) in set {
        let v = if own.is_empty() {
            v.clone()
        } else {
            let Some(d) = own.iter().find(|d| &d.decl.name == k) else {
                let names: Vec<&str> = own.iter().map(|d| d.decl.name.as_str()).collect();
                anyhow::bail!(
                    "--set {k}: the program declares no input {k} (its inputs: {})",
                    names.join(", ")
                );
            };
            let v = coerce(&d.decl.ty, v.clone());
            if !has_type(&d.decl.ty, &v) {
                anyhow::bail!(
                    "--set {k}={}: input {k} is {}",
                    shown(&v),
                    type_text(&d.decl.ty)
                );
            }
            v
        };
        out.push(Atom {
            pred: "input".into(),
            args: vec![Term::Val(Value::Str(k.clone())), Term::Val(v)],
            record: None,
            span: Span::default(),
        });
    }
    Ok(out)
}

/// An `--input-file`: facts `k(v).`, one per stack input, each a
/// normal-rank contribution to that input, stated where the file states it.
pub fn file_stmts(program: &Program, declared: &[Declared]) -> Result<Vec<Stmt>> {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for s in &program.statements {
        let (a, span) = match s {
            Stmt::Fact(a) => (a, a.span),
            Stmt::Rule(r) => {
                diags.push(Diagnostic::error(
                    r.head.span,
                    "an input file holds facts, `name(value).`, not rules",
                ));
                continue;
            }
            _ => {
                diags.push(Diagnostic::error(
                    Span::default(),
                    "an input file holds facts, `name(value).`",
                ));
                continue;
            }
        };
        let known = declared
            .iter()
            .any(|d| d.scope.is_empty() && d.decl.name == a.pred);
        if !known || a.args.len() != 1 {
            diags.push(Diagnostic::error(
                span,
                format!("{}/{} is not an input of the program", a.pred, a.args.len()),
            ));
            continue;
        }
        out.push(Stmt::Fact(Atom {
            pred: "arg".into(),
            args: vec![
                Term::Val(Value::Str(crate::modules::INPUT.into())),
                Term::Val(Value::Str(String::new())),
                Term::Val(Value::Str(a.pred.clone())),
                a.args[0].clone(),
                Term::Val(Value::Str(crate::transform::NORMAL.into())),
            ],
            record: None,
            span,
        }));
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// Every required stack input (no default) that nothing gives a value: an
/// error at its declaration.
pub fn check_required(declared: &[Declared], given: &BTreeSet<String>) -> Result<()> {
    let diags: Vec<Diagnostic> = declared
        .iter()
        .filter(|d| d.scope.is_empty() && d.decl.default.is_none())
        .filter(|d| !given.contains(&d.decl.name))
        .map(|d| {
            Diagnostic::error(
                d.decl.span,
                format!("input {} is required and has no value", d.decl.name),
            )
            .with_help(format!(
                "give it with `--set {}=...` or in an `--input-file`",
                d.decl.name
            ))
        })
        .collect();
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}
