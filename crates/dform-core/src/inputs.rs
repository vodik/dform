//! Typed inputs (DESIGN.org "Typed stack inputs", E §7.1): `input k: T [=
//! D] [check R]` at the top of a program is the stack's interface, in a
//! module the instance's (`modules`). `key k: T` is an input the target
//! gives and whose value names the deployment (R-29, `stack`); an input in
//! every other respect.
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
/// module instance's. An object input is declared once per leaf (R-54):
/// `decl.name` is the leaf's path, `nodes.count`.
#[derive(Debug, Clone)]
pub struct Declared {
    pub scope: String,
    pub decl: InputDecl,
    /// The name the stack gives it by (R-55): `--set`, `why`, `dform
    /// test`. The stack's own input's is its name, a used module's
    /// `m.name`; a component copy's input has none, its instance block
    /// gives it.
    pub address: Option<String>,
    /// A `use` block gives it a value.
    pub bound: bool,
    /// The `use` that declares it, for a used module's input.
    pub used_at: Option<Span>,
}

impl Declared {
    /// The stack's own input, or one a `use` of a module declares.
    pub fn new(scope: &str, decl: InputDecl, address: Option<String>, bound: bool) -> Declared {
        Declared {
            scope: scope.to_string(),
            decl,
            address,
            bound,
            used_at: None,
        }
    }

    fn who(&self) -> String {
        match (&self.address, self.scope.is_empty()) {
            (Some(a), _) => format!("input {a}"),
            (None, true) => format!("input {}", self.decl.name),
            (None, false) => format!("input {} of {}", self.decl.name, self.scope),
        }
    }
}

/// Is `i` an object input, one cell per leaf (R-54): the block form, or
/// a type that is an object with fields.
pub fn is_object(i: &InputDecl) -> bool {
    !i.fields.is_empty() || matches!(&i.ty, TypeExpr::Object(fs) if !fs.is_empty())
}

/// An object input's leaves, each an input named by its path (`nodes.count`)
/// with the field's type, default and check (the check reads the field by
/// its name or its path); the block form and the alias form `input k: T =
/// { .. }` give the same leaves. Any other input is its own one leaf.
pub fn leaves(i: &InputDecl) -> Vec<InputDecl> {
    let mut out = Vec::new();
    leaves_into(i, &i.name, &mut out);
    out
}

fn leaves_into(i: &InputDecl, path: &str, out: &mut Vec<InputDecl>) {
    if !is_object(i) {
        let mut leaf = i.clone();
        leaf.name = path.to_string();
        leaf.fields = Vec::new();
        if path != i.name {
            leaf.refinement = i
                .refinement
                .iter()
                .map(|l| crate::modules::subst_lit(l, &i.name, &Term::Val(Value::Str(path.into()))))
                .collect();
        }
        out.push(leaf);
        return;
    }
    let fields: Vec<InputDecl> = if i.fields.is_empty() {
        let TypeExpr::Object(fs) = &i.ty else {
            unreachable!("is_object")
        };
        fs.iter()
            .map(|(k, t)| InputDecl {
                name: k.clone(),
                ty: t.clone(),
                default: match &i.default {
                    Some(Term::Obj(m)) => m.get(k).cloned(),
                    _ => None,
                },
                refinement: Vec::new(),
                key: false,
                fields: Vec::new(),
                span: i.span,
            })
            .collect()
    } else {
        i.fields.clone()
    };
    for f in &fields {
        leaves_into(f, &format!("{path}.{}", f.name), out);
    }
}

/// The type of an object input's block form: the object of its fields'.
pub fn fields_type(fields: &[InputDecl]) -> TypeExpr {
    TypeExpr::Object(
        fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect(),
    )
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
/// accepted unchecked; `secret(T)` is `T`, labeled secret (`secrets`).
pub fn check_type(t: &TypeExpr) -> Result<(), String> {
    match t {
        TypeExpr::Name(n) => match n.as_str() {
            "int" | "string" | "bool" | "inet" | "symbol" | "addr" | "any" | "bytes" | "cpu"
            | "duration" | "time" => Ok(()),
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
            ("list" | "set" | "secret", [x]) => check_type(x),
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
            "bytes" | "cpu" | "duration" => {
                matches!(v, Value::Quantity(q) if q.dim().name() == n.as_str())
            }
            "time" => matches!(v, Value::Time(_)),
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
            ("secret", [x]) => has_type(x, v),
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
/// an int or a bool, `--set k=@FILE` a document; an `inet` input parses its
/// string, a `string` input takes the text of an int or a bool, and an
/// object's fields and a list's elements are read as theirs.
pub fn coerce(t: &TypeExpr, v: Value) -> Value {
    match (t, v) {
        (TypeExpr::Name(n), Value::Str(s)) if n == "inet" => match crate::value::parse_ipnet(&s) {
            Some((addr, prefix)) => Value::IpNet { addr, prefix },
            None => Value::Str(s),
        },
        // A quantity or a time from its text, as a literal reads (R-66).
        (TypeExpr::Name(n), v @ (Value::Str(_) | Value::Int(_)))
            if matches!(n.as_str(), "bytes" | "cpu" | "duration" | "time") =>
        {
            match crate::types::literal(
                &crate::types::Ty::parse(n),
                crate::ast::Term::Val(v.clone()),
            ) {
                Ok(crate::ast::Term::Val(r)) => r,
                _ => v,
            }
        }
        (TypeExpr::Name(n), Value::Int(i)) if n == "string" => Value::Str(i.to_string()),
        (TypeExpr::Name(n), Value::Bool(b)) if n == "string" => Value::Str(b.to_string()),
        (TypeExpr::Apply(n, xs), v) if n == "secret" && xs.len() == 1 => coerce(&xs[0], v),
        (TypeExpr::Apply(n, xs), Value::List(vs))
            if (n == "list" || n == "set") && xs.len() == 1 =>
        {
            Value::List(vs.into_iter().map(|v| coerce(&xs[0], v)).collect())
        }
        (TypeExpr::Object(fs), Value::Obj(m)) => Value::Obj(
            m.into_iter()
                .map(|(k, v)| match fs.iter().find(|(f, _)| *f == k) {
                    Some((_, t)) => {
                        let v = coerce(t, v);
                        (k, v)
                    }
                    None => (k, v),
                })
                .collect(),
        ),
        (_, v) => v,
    }
}

/// A constant term's value: a fact's argument, as `--set k=@FILE.df` and
/// an `--input-file` give it.
pub fn ground(t: &Term) -> Option<Value> {
    match t {
        Term::Val(v) => Some(v.clone()),
        Term::List(xs) => xs
            .iter()
            .map(ground)
            .collect::<Option<_>>()
            .map(Value::List),
        Term::Obj(m) => m
            .iter()
            .map(|(k, x)| Some((k.clone(), ground(x)?)))
            .collect::<Option<_>>()
            .map(Value::Obj),
        _ => None,
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => crate::partition::fmt_value(v),
    }
}

/// The type of every `attr(input, Scope, k, V)` of an evaluation: one
/// violation per value that is not its input's type. An object input's
/// cell holds its leaves (R-54): each is checked, and a field no leaf
/// declares is one too.
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
        let leaves: Vec<&Declared> = declared
            .iter()
            .filter(|d| &d.scope == scope)
            .filter(|d| {
                &d.decl.name == k
                    || d.decl
                        .name
                        .strip_prefix(k.as_str())
                        .is_some_and(|r| r.starts_with('.'))
            })
            .collect();
        for d in &leaves {
            let x = match d.decl.name[k.len()..].strip_prefix('.') {
                None => Some(v),
                Some(rest) => at_path(v, rest),
            };
            if let Some(x) = x
                && !has_type(&d.decl.ty, x)
            {
                out.push(format!(
                    "{}: {} is not {}",
                    d.who(),
                    shown(x),
                    type_text(&d.decl.ty)
                ));
            }
        }
        if leaves.iter().any(|d| d.decl.name != *k) {
            let mut paths = Vec::new();
            value_leaves(v, k, &mut paths);
            for p in paths {
                let known = leaves.iter().any(|d| {
                    d.decl.name == p
                        || p.strip_prefix(d.decl.name.as_str())
                            .is_some_and(|r| r.starts_with('.'))
                });
                if !known {
                    let who = match leaves[0].address.as_deref() {
                        Some(a) => format!(
                            "input {}",
                            &a[..a.len() - (leaves[0].decl.name.len() - k.len())]
                        ),
                        None if scope.is_empty() => format!("input {k}"),
                        None => format!("input {k} of {scope}"),
                    };
                    out.push(format!("{who} has no field {}", &p[k.len() + 1..]));
                }
            }
        }
    }
    out
}

/// The leaves an address names: the leaf itself, or every leaf of the
/// object `k` (`nodes` names `nodes.flavor` and `nodes.count`).
fn under<'a>(given: &'a [&'a Declared], k: &str) -> Vec<&'a Declared> {
    given
        .iter()
        .copied()
        .filter(|d| {
            d.address
                .as_deref()
                .is_some_and(|a| a == k || a.strip_prefix(k).is_some_and(|r| r.starts_with('.')))
        })
        .collect()
}

/// The object input `k` is a field of, and the fields it has: `nodes.cnt`
/// is no field of `nodes` (its fields: count, flavor).
fn no_field(given: &[&Declared], k: &str) -> Option<String> {
    let mut at = k;
    while let Some((object, _)) = at.rsplit_once('.') {
        let leaves = under(given, object);
        if !leaves.is_empty() {
            let mut fields: Vec<&str> = leaves
                .iter()
                .filter_map(|d| {
                    d.address
                        .as_deref()?
                        .strip_prefix(object)?
                        .strip_prefix('.')
                })
                .map(|r| r.split('.').next().unwrap_or(r))
                .collect();
            fields.dedup();
            let field = &k[object.len() + 1..];
            return Some(format!(
                "input {object} has no field {field} (its fields: {})",
                fields.join(", ")
            ));
        }
        at = object;
    }
    None
}

/// `v` at the path `rest` (`a.b`) of an object, if it has it.
fn at_path<'v>(v: &'v Value, rest: &str) -> Option<&'v Value> {
    rest.split('.').try_fold(v, |v, seg| match v {
        Value::Obj(m) => m.get(seg),
        _ => None,
    })
}

/// Every leaf path of an object value, `a.b` for `{a: {b: 1}}`.
fn value_leaves(v: &Value, path: &str, out: &mut Vec<String>) {
    match v {
        Value::Obj(m) if !m.is_empty() => {
            for (k, v) in m {
                value_leaves(v, &format!("{path}.{k}"), out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

/// `v` given to the object `k` whose leaves are `leaves`: each field read
/// as its leaf's type; a field no leaf has is an error naming the fields.
fn object_value(k: &str, v: Value, leaves: &[&Declared]) -> Result<Value> {
    let Value::Obj(_) = &v else {
        let fields: Vec<&str> = leaves
            .iter()
            .filter_map(|d| d.address.as_deref()?.strip_prefix(k)?.strip_prefix('.'))
            .collect();
        anyhow::bail!(
            "--set {k}={}: input {k} is an object of {}: give a field, `--set {k}.FIELD=v`, \
             or a document, `--set {k}=@FILE`",
            shown(&v),
            fields.join(", ")
        );
    };
    let mut paths = Vec::new();
    value_leaves(&v, k, &mut paths);
    for p in &paths {
        let known = leaves.iter().any(|d| {
            d.address
                .as_deref()
                .is_some_and(|a| a == p || p.strip_prefix(a).is_some_and(|r| r.starts_with('.')))
        });
        if !known {
            let msg = no_field(leaves, p).unwrap_or_else(|| format!("input {k} has no field {p}"));
            anyhow::bail!("--set {k}: {msg}");
        }
    }
    let mut out = v;
    for d in leaves {
        let a = d.address.as_deref().unwrap_or_default();
        let rest = &a[k.len() + 1..];
        let Some(x) = at_path(&out, rest).cloned() else {
            continue;
        };
        let x = coerce(&d.decl.ty, x);
        if !has_type(&d.decl.ty, &x) {
            anyhow::bail!(
                "--set {k}: {a} = {} is not {}",
                shown(&x),
                type_text(&d.decl.ty)
            );
        }
        set_path(&mut out, rest, x);
    }
    Ok(out)
}

fn set_path(v: &mut Value, rest: &str, x: Value) {
    let mut v = v;
    for seg in rest.split('.') {
        let Value::Obj(m) = v else { return };
        let Some(next) = m.get_mut(seg) else { return };
        v = next;
    }
    *v = x;
}

/// The stack's inputs given on the command line, `--set k=v`, as `input(k,
/// v)` facts read as their declared types. `k` is an input's address: the
/// stack's own `k`, a leaf of an object input `nodes.count`, the object
/// itself `nodes` (a document of its fields), or a used module's `m.k`
/// (R-54, R-55). A key the program does not declare, when it declares
/// inputs, is an error; so is a value of the wrong type.
pub fn set_facts(declared: &[Declared], set: &[(String, Value)]) -> Result<Vec<Atom>> {
    let own: Vec<&Declared> = declared.iter().filter(|d| d.address.is_some()).collect();
    let mut out = Vec::new();
    for (k, v) in set {
        let leaves = under(&own, k);
        let v = if own.is_empty() {
            v.clone()
        } else if let [d] = leaves.as_slice()
            && d.address.as_deref() == Some(k.as_str())
        {
            let v = coerce(&d.decl.ty, v.clone());
            if !has_type(&d.decl.ty, &v) {
                anyhow::bail!(
                    "--set {k}={}: input {k} is {}",
                    shown(&v),
                    type_text(&d.decl.ty)
                );
            }
            v
        } else if !leaves.is_empty() {
            object_value(k, v.clone(), &leaves)?
        } else if let Some(msg) = no_field(&own, k) {
            anyhow::bail!("--set {k}: {msg}");
        } else {
            let names: Vec<&str> = own.iter().filter_map(|d| d.address.as_deref()).collect();
            anyhow::bail!(
                "--set {k}: the program declares no input {k} (its inputs: {})",
                names.join(", ")
            );
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
/// A fact of an object input contributes to each leaf its object gives.
pub fn file_stmts(program: &Program, declared: &[Declared]) -> Result<Vec<Stmt>> {
    let own: Vec<&Declared> = declared.iter().filter(|d| d.scope.is_empty()).collect();
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
                    "an input file holds facts, `name(value)`",
                ));
                continue;
            }
        };
        let leaves: Vec<&Declared> = own
            .iter()
            .copied()
            .filter(|d| {
                d.decl.name == a.pred
                    || d.decl
                        .name
                        .strip_prefix(&a.pred)
                        .is_some_and(|r| r.starts_with('.'))
            })
            .collect();
        if leaves.is_empty() || a.args.len() != 1 {
            diags.push(Diagnostic::error(
                span,
                format!("{}/{} is not an input of the program", a.pred, a.args.len()),
            ));
            continue;
        }
        let mut given = Vec::new();
        for d in &leaves {
            let value = match d.decl.name.strip_prefix(&a.pred) {
                Some("") => Some(a.args[0].clone()),
                Some(rest) => field_term(&a.args[0], &rest[1..]),
                None => None,
            };
            if let Some(v) = value {
                given.push(d.decl.name.clone());
                out.push(Stmt::Fact(Atom {
                    pred: "arg".into(),
                    args: vec![
                        Term::Val(Value::Str(crate::modules::INPUT.into())),
                        Term::Val(Value::Str(String::new())),
                        Term::Val(Value::Str(d.decl.name.clone())),
                        v,
                        Term::Val(Value::Str(crate::transform::NORMAL.into())),
                    ],
                    record: None,
                    span,
                }));
            }
        }
        // A field no leaf has.
        if let Term::Obj(_) = &a.args[0]
            && leaves.iter().all(|d| d.decl.name != a.pred)
        {
            let mut paths = Vec::new();
            term_leaves(&a.args[0], &a.pred, &mut paths);
            for p in paths {
                if !given.contains(&p) {
                    let names: Vec<&str> = leaves.iter().map(|d| d.decl.name.as_str()).collect();
                    diags.push(Diagnostic::error(
                        span,
                        format!(
                            "input {} has no field {} (its fields: {})",
                            a.pred,
                            &p[a.pred.len() + 1..],
                            names.join(", ")
                        ),
                    ));
                }
            }
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// The field at `rest` (`a.b`) of an object term.
fn field_term(t: &Term, rest: &str) -> Option<Term> {
    rest.split('.').try_fold(t.clone(), |t, seg| match t {
        Term::Obj(mut m) => m.remove(seg),
        _ => None,
    })
}

fn term_leaves(t: &Term, path: &str, out: &mut Vec<String>) {
    match t {
        Term::Obj(m) if !m.is_empty() => {
            for (k, v) in m {
                term_leaves(v, &format!("{path}.{k}"), out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

/// Every required input the stack gives (no default, and no `use` block
/// gives it) that nothing gives a value: an error at its declaration. An
/// object given whole gives each of its leaves.
pub fn check_required(declared: &[Declared], given: &BTreeSet<String>) -> Result<()> {
    let is_given =
        |a: &str| given.contains(a) || a.match_indices('.').any(|(i, _)| given.contains(&a[..i]));
    let diags: Vec<Diagnostic> = declared
        .iter()
        .filter(|d| d.decl.default.is_none() && !d.bound)
        .filter_map(|d| Some((d, d.address.as_deref()?)))
        .filter(|(_, a)| !is_given(a))
        .map(|(d, k)| {
            let what = if d.decl.key { "key" } else { "input" };
            let msg = format!("{what} {k} is required and has no value");
            if let Some(at) = d.used_at {
                // A used module's: where the stack uses it (R-55).
                let field = &d.decl.name;
                return Diagnostic::error(at, msg)
                    .with_label(
                        d.decl.span,
                        format!("{field}: {} declared here", type_text(&d.decl.ty)),
                    )
                    .with_help(format!(
                        "give it in the block, `{{ {field} = ... }}`, with `--set {k}=...`, \
                         or give it a default"
                    ));
            }
            let help = match d.decl.key {
                true => format!("give it with the target, `STACK {k}=...`"),
                false => format!("give it with `--set {k}=...` or in an `--input-file`"),
            };
            Diagnostic::error(d.decl.span, msg).with_help(help)
        })
        .collect();
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}
