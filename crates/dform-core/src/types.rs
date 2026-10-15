//! Typed positions against the provider's schema (R-43, R-31): every
//! contribution to an attribute the schema types is checked at compile
//! time, against the lowered program's `arg(T, A, Path, Value, Rank)`
//! heads.
//!
//! - `ref(T)` (and `list(ref(T))`, `set(ref(T))`) takes a reference to a
//!   `T`: a resource by its name, `T[e]`, a typed variable, `ref(r)`. A
//!   reference to another type, or a literal, is an error naming both.
//! - A scalar attribute (`string`, `int`, `bool`, `inet`, `enum(..)`) takes
//!   a value of its type. A literal is checked as written (a string literal
//!   in an `inet` position must parse as one; Postgres's unknown-literal
//!   rule); a reference is an error that says to read an attribute, unless
//!   it is written out as `ref(r)` (the id, where the API takes one as text).
//!
//! - A quantity attribute (`bytes`, `cpu`, `duration`, R-66) or a `time`
//!   (R-62) takes a value of its type, a literal read as one: `512Mi`,
//!   `500m` (millicores in a cpu position, minutes in a duration one), a
//!   bare integer (bytes, cores), a string that parses (`"P1M"`, a time's
//!   RFC 3339 text). [`read`] reads them against the schema before the
//!   program is evaluated, into the nested paths of an object or list
//!   value (`containers.resources.limits.memory`); a `500m` no type reads
//!   is an error naming both readings.
//!
//! A term the compiler cannot see the value of (a variable, a call) is
//! checked when it has one, at evaluation.

use crate::address::Address;
use crate::ast::str_term;
use crate::ast::{Lit, Program, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::program::node::{
    ExprId, ExprKind, Header, ItemId, ItemKind, ListPart, ObjPart, ResourceBody, Target, Write,
};
use crate::program::{self as prog, ClauseId, Readings};
use crate::quantity::{self, Dim};
use crate::schema::Schema;
use crate::spell;
use crate::value::Value;
use anyhow::Result;
use std::collections::BTreeMap;

/// The internal function a quantity literal with no reading of its own
/// lowers to (`500m`, `2.5m`): the position's type reads it.
pub const AMBIGUOUS: &str = "__quantity";

/// A schema attribute's type, as `type_attr` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    /// `ref(T)`: a reference to a `T`; `ref(T1 | T2)`, to one of several
    /// (R-185: a relation's column a rule per type fills), its types
    /// sorted and joined by ` | `.
    Ref(String),
    /// `list(T)`, `set(T)`.
    List(Box<Ty>),
    /// `map(T)`: an object with open keys, each value a `T`.
    Map(Box<Ty>),
    /// `secret(T)`.
    Secret(Box<Ty>),
    /// `enum(a, b, ..)`.
    Enum(Vec<String>),
    /// `string`, `int`, `float`, `number` (an int or a float, R-75),
    /// `bool`, `inet`, `ip`, `uri`, `oci`, `regex` (a pattern, not a schema type:
    /// a function parameter only), the quantities `bytes`, `cpu`,
    /// `duration`, `time`, and `range(T)` of an ordered `T` (R-180).
    Scalar(String),
    /// Anything the check does not judge (an untyped `map`, `object`,
    /// `any`, an untyped `list`).
    Any,
}

impl Ty {
    /// The types a reference type's text names: `T`, or each of `T1 | T2`.
    pub fn ref_types(text: &str) -> impl Iterator<Item = &str> {
        text.split('|').map(str::trim)
    }

    /// `ref(a | b)`: a reference to one of the types of `a` or of `b`.
    pub fn ref_union(a: &str, b: &str) -> Ty {
        let types: std::collections::BTreeSet<&str> =
            Ty::ref_types(a).chain(Ty::ref_types(b)).collect();
        Ty::Ref(types.into_iter().collect::<Vec<_>>().join(" | "))
    }

    /// Read a type's text: `ref(net.vpc)`, `list(ref(net.subnet))`,
    /// `enum("a", "b")`, `inet`, `bytes(gib)` (a quantity and how the
    /// provider takes it, `schema::Render`).
    pub fn parse(s: &str) -> Ty {
        let s = s.trim();
        let Some((head, rest)) = s.split_once('(') else {
            return match s {
                "string" | "int" | "float" | "number" | "bool" | "inet" | "ip" | "bytes"
                | "cpu" | "duration" | "time" | "uri" | "oci" | "semver" | "regex" => {
                    Ty::Scalar(s.to_string())
                }
                _ => Ty::Any,
            };
        };
        let Some(inner) = rest.strip_suffix(')') else {
            return Ty::Any;
        };
        match head.trim() {
            "ref" => Ty::Ref(inner.trim().trim_matches('"').to_string()),
            "list" | "set" => Ty::List(Box::new(Ty::parse(inner))),
            "map" => Ty::Map(Box::new(Ty::parse(inner))),
            // `range(T)` (R-180): a value type, held by its text.
            "range" => range(inner.trim()),
            "secret" => Ty::Secret(Box::new(Ty::parse(inner))),
            "enum" => Ty::Enum(
                inner
                    .split(',')
                    .map(|m| m.trim().trim_matches('"').to_string())
                    .collect(),
            ),
            h if measured(h) => Ty::Scalar(h.to_string()),
            _ => Ty::Any,
        }
    }

    /// A quantity's or a time's type: what [`read`] reads.
    fn measured(&self) -> bool {
        match self {
            Ty::Scalar(s) => measured(s),
            Ty::Secret(t) | Ty::List(t) | Ty::Map(t) => t.measured(),
            _ => false,
        }
    }
}

/// `range(T)` for an ordered `T`; nothing the check judges otherwise.
fn range(elem: &str) -> Ty {
    match crate::range::ORDERED.contains(&elem) {
        true => Ty::Scalar(format!("range({elem})")),
        false => Ty::Any,
    }
}

/// The types read from a literal's text, a quantity's or a time's
/// (`512Mi`, `500m`, `1h30m`, an instant): `bytes`, `cpu`, `duration`,
/// `time`.
pub fn measured(ty: &str) -> bool {
    matches!(ty, "bytes" | "cpu" | "duration" | "time")
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Ref(t) => write!(f, "ref({t})"),
            Ty::List(t) => write!(f, "list({t})"),
            Ty::Map(t) => write!(f, "map({t})"),
            Ty::Secret(t) => write!(f, "secret({t})"),
            Ty::Enum(ms) => write!(
                f,
                "enum({})",
                ms.iter()
                    .map(|m| format!("{m:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Ty::Scalar(s) => write!(f, "{s}"),
            Ty::Any => write!(f, "any"),
        }
    }
}

/// A reference as it lowers: `ref(T, A, "")`, its type and address.
fn reference(t: &Term) -> Option<(&str, &Term)> {
    match t {
        Term::Func { name, args } if name == crate::address::REF && args.len() == 3 => {
            match (&args[0], &args[2]) {
                (Term::Val(Value::Str(typ)), Term::Val(Value::Str(p))) if p.is_empty() => {
                    Some((typ, &args[1]))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `ref(r)` written out: `ref(ref(T, A, ""))`.
fn explicit(t: &Term) -> Option<&Term> {
    match t {
        Term::Func { name, args } if name == crate::address::REF && args.len() == 1 => {
            Some(&args[0])
        }
        _ => None,
    }
}

/// How a reference reads in a message: `net.vpc["main"]`, or `a net.vpc`
/// when its address is a variable.
fn shown_ref(typ: &str, addr: &Term) -> String {
    match addr {
        Term::Val(Value::Str(a)) => Address {
            typ: typ.to_string(),
            name: a.to_string(),
        }
        .to_string(),
        _ => format!("a {typ}"),
    }
}

fn shown_literal(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("the string {s:?}"),
        Value::Int(i) => format!("the int {i}"),
        Value::Float(f) => format!("the float {f}"),
        Value::Bool(b) => format!("the bool {b}"),
        Value::Quantity(q) => format!("the {} {q}", q.dim().name()),
        Value::Time(t) => format!("the time {t}"),
        Value::Uri(u) => format!("the uri {u}"),
        Value::Oci(r) => format!("the image reference {r}"),
        Value::Semver(v) => format!("the version {v}"),
        v => spell::value(v),
    }
}

/// A quantity literal with no unit of its own (`quantity::Literal::Ambiguous`)
/// as it lowers: a decimal, `0.5`, is a float (R-75), which a quantity's
/// position reads as one (`0.5` cores is `500m`); `500m` is
/// `__quantity("500m", file, start, end)`, its span kept for the error
/// when no position reads it.
pub fn ambiguous_literal(text: &str, span: crate::ast::Span) -> Term {
    if !text.ends_with('m')
        && let Ok(f) = crate::value::Float::parse(text)
    {
        return Term::Val(Value::Float(f));
    }
    spanned(AMBIGUOUS, text, span)
}

/// `name(text, file, start, end)`: an internal call that keeps the span of
/// what it was written as.
fn spanned(name: &str, text: &str, span: crate::ast::Span) -> Term {
    let n = |x: u32| Term::Val(Value::Int(i64::from(x)));
    Term::Func {
        name: name.to_string(),
        args: vec![
            Term::Val(Value::Str(text.to_string())),
            n(span.file),
            n(span.start),
            n(span.end),
        ],
    }
}

/// A resource named by a bare name two or more types share, in a
/// resource's attribute (R-74): `__resource("main", file, start, end,
/// ref(T1, A1, ""), ..)`, one reference per candidate. The attribute's
/// `ref(T)` picks the candidate of type `T` ([`read`]); anywhere else it
/// is the error that lists them.
pub const AMBIGUOUS_REF: &str = "__resource";

/// The name and the candidate references of an ambiguous resource name.
fn ambiguous_ref_of(t: &Term) -> Option<(&str, &[Term])> {
    match t {
        Term::Func { name, args } if name == AMBIGUOUS_REF && args.len() > 4 => match &args[0] {
            Term::Val(Value::Str(s)) => Some((s, &args[4..])),
            _ => None,
        },
        _ => None,
    }
}

/// The error for a bare name two or more resources of `types` share,
/// where no type picks one: `main` names 2 resources: write one of ..
pub fn ambiguous_resource(name: &str, types: &[String]) -> String {
    let list = types
        .iter()
        .map(|t| {
            Address {
                typ: t.clone(),
                name: name.to_string(),
            }
            .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "`{name}` names {} resources: write one of {list}",
        types.len()
    )
}

/// Pick, in `t` (a value of an attribute typed `ty`), the candidate of an
/// ambiguous resource name whose type the attribute's `ref(T)` names.
fn pick_refs(ty: &Ty, t: &mut Term) {
    match (ty, &mut *t) {
        (Ty::Secret(inner), _) => pick_refs(inner, t),
        (Ty::List(inner), Term::List(xs)) => xs.iter_mut().for_each(|x| pick_refs(inner, x)),
        (Ty::Ref(want), _) => {
            let picked = ambiguous_ref_of(t).and_then(|(_, cs)| {
                let mut of = cs
                    .iter()
                    .filter(|c| reference(c).is_some_and(|(typ, _)| typ == want));
                of.next().filter(|_| of.next().is_none()).cloned()
            });
            if let Some(c) = picked {
                *t = c;
            }
        }
        _ => {}
    }
}

/// The text of an ambiguous quantity literal, `__quantity("500m", ..)`.
pub fn ambiguous(t: &Term) -> Option<&str> {
    match t {
        Term::Func { name, args } if name == AMBIGUOUS => match args.first() {
            Some(Term::Val(Value::Str(s))) => Some(s),
            _ => None,
        },
        _ => None,
    }
}

/// Where an ambiguous quantity literal was written.
fn ambiguous_span(t: &Term) -> Option<crate::ast::Span> {
    let Term::Func { args, .. } = t else {
        return None;
    };
    let n = |i: usize| match args.get(i) {
        Some(Term::Val(Value::Int(x))) => u32::try_from(*x).ok(),
        _ => None,
    };
    Some(crate::ast::Span {
        file: n(1)?,
        start: n(2)?,
        end: n(3)?,
        origin: 0,
    })
}

/// A literal read as the quantity or time `s` names: the value, or why it
/// is not one. `None`: not a literal this reads.
fn measure(s: &str, t: &Term) -> Option<Result<Value, String>> {
    let text = |v: &str| -> Result<Value, String> {
        match Dim::parse(s) {
            Some(d) => quantity::read(d, v).map(Value::Quantity),
            None => crate::time::Time::parse(v).map(Value::Time),
        }
    };
    Some(match (s, t) {
        (_, Term::Val(Value::Quantity(q))) if q.dim().name() == s => Ok(Value::Quantity(*q)),
        ("time", Term::Val(v @ Value::Time(_))) => Ok(v.clone()),
        ("bytes", Term::Val(Value::Int(n))) => Ok(Value::Quantity(quantity::Quantity::Bytes(*n))),
        ("cpu", Term::Val(Value::Int(n))) => n
            .checked_mul(1000)
            .map(|m| Value::Quantity(quantity::Quantity::Cpu(m)))
            .ok_or_else(|| format!("`{n}` cores is out of range")),
        ("duration", Term::Val(Value::Int(n))) => Err(format!(
            "is a duration: the int {n} has no unit (`{n}s`, `{n}m`, `{n}h`, `{n}d`)"
        )),
        // A decimal is cores (`0.5` is `500m`, R-66); bytes are whole.
        ("cpu" | "bytes", Term::Val(Value::Float(f))) => {
            text(&f.to_string()).map_err(|e| format!("is {s}: {e}"))
        }
        (_, Term::Val(Value::Str(x))) => text(x).map_err(|e| format!("is {s}: {e}")),
        (_, Term::Val(Value::Null { .. })) => return None,
        (_, Term::Val(v)) => Err(format!("is {s}, not {}", shown_literal(v))),
        (_, t) => text(ambiguous(t)?).map_err(|e| format!("is {s}: {e}")),
    })
}

/// Why `t` is not a `ty`, when the compiler can tell; `None` when it fits
/// or cannot be seen until evaluation.
pub fn mismatch(ty: &Ty, t: &Term) -> Option<String> {
    if let Some((typ, addr)) = explicit(t).and_then(reference).or_else(|| reference(t)) {
        let written = explicit(t).is_some();
        return match ty {
            Ty::Ref(want) if !Ty::ref_types(want).any(|w| w == typ) => {
                Some(format!("takes a ref({want}), got {}", shown_ref(typ, addr)))
            }
            Ty::Ref(_) | Ty::Any => None,
            Ty::Secret(_) => match ty {
                Ty::Secret(inner) => mismatch(inner, t),
                _ => None,
            },
            Ty::List(_) => Some(format!(
                "is {ty}, not one reference: {} in a list is `[r]`",
                shown_ref(typ, addr)
            )),
            Ty::Scalar(s) if s == "string" && written => None,
            _ => Some(format!(
                "is {ty}, not a reference: {} is no {ty}; read one of its attributes \
                 (`r.name`){}",
                shown_ref(typ, addr),
                if matches!(ty, Ty::Scalar(s) if s == "string") {
                    ", or write `ref(r)` where the API takes its id as text"
                } else {
                    ""
                }
            )),
        };
    }
    match (ty, t) {
        (Ty::Any, _) => None,
        (Ty::Secret(inner), t) => mismatch(inner, t),
        (Ty::List(inner), Term::List(xs)) => xs.iter().find_map(|x| mismatch(inner, x)),
        (Ty::List(inner), Term::Val(Value::List(xs))) => xs
            .iter()
            .find_map(|x| mismatch(inner, &Term::Val(x.clone()))),
        (Ty::List(_), Term::Val(v)) => Some(format!("is {ty}, not {}", shown_literal(v))),
        (Ty::Map(inner), Term::Obj(m)) => m.iter().find_map(|(k, x)| {
            mismatch(inner, x).map(|why| format!("is {ty}: its key {k:?} {why}"))
        }),
        (Ty::Map(inner), Term::Val(Value::Obj(m))) => m.iter().find_map(|(k, x)| {
            mismatch(inner, &Term::Val(x.clone()))
                .map(|why| format!("is {ty}: its key {k:?} {why}"))
        }),
        (Ty::Map(_), Term::Val(v)) => Some(format!("is {ty}, not {}", shown_literal(v))),
        (Ty::Ref(want), Term::Val(v)) => Some(format!(
            "takes a ref({want}): a resource, by its name, `{want}[\"a\"]` or a variable, not {}",
            shown_literal(v)
        )),
        (Ty::Enum(ms), Term::Val(Value::Str(s))) if !ms.contains(s) => {
            Some(format!("is {ty}: {s:?} is not one of its members"))
        }
        (Ty::Enum(_), Term::Val(Value::Str(_))) => None,
        (Ty::Scalar(s), t) if ty.measured() => measure(s, t)?.err(),
        (Ty::Scalar(s), t) if ambiguous(t).is_some() => Some(format!(
            "is {s}, not the quantity `{}`",
            ambiguous(t).unwrap_or_default()
        )),
        (Ty::Scalar(s), Term::Val(v)) => {
            let fits = match (s.as_str(), v) {
                ("string", Value::Str(_)) => true,
                ("int", Value::Int(_)) => true,
                // An int literal in a float position is that float.
                ("float" | "number", Value::Int(_) | Value::Float(_)) => true,
                ("bool", Value::Bool(_)) => true,
                ("inet", Value::IpNet { .. }) => true,
                ("inet", Value::Str(x)) => crate::value::parse_ipnet(x).is_some(),
                ("ip", Value::Ip(_)) => true,
                ("ip", Value::Str(x)) => crate::value::ipv4_to_u32(x).is_some(),
                // A uri's text is read as one (`read_as`); a regex pattern
                // stays a string, its text checked.
                ("uri", Value::Uri(_)) => true,
                ("uri", Value::Str(x)) => crate::uri::Uri::parse(x).is_ok(),
                ("oci", Value::Oci(_)) => true,
                ("oci", Value::Str(x)) => crate::value::OciRef::parse(x).is_ok(),
                ("semver", v) => crate::value::read_typed(s, v).is_ok(),
                (s, v) if crate::range::element(s).is_some() => {
                    crate::value::read_typed(s, v).is_ok()
                }
                ("regex", Value::Str(x)) => regex::Regex::new(x).is_ok(),
                // A null is not known yet; a computed value fits its type.
                (_, Value::Null { .. }) => true,
                _ => false,
            };
            (!fits).then(|| match (s.as_str(), v) {
                ("inet", Value::Str(x)) => {
                    format!("is an inet: {x:?} is not a network (`a.b.c.d/n`)")
                }
                ("ip", Value::Str(x)) => format!("is an ip: {x:?} is not an address (`a.b.c.d`)"),
                ("uri", Value::Str(x)) => {
                    format!("is a uri: {}", crate::uri::Uri::parse(x).unwrap_err())
                }
                ("oci", Value::Str(x)) => {
                    format!("is an oci: {}", crate::value::parse_oci(x).unwrap_err())
                }
                ("semver", Value::Str(_)) => format!(
                    "is a semver: {}",
                    crate::value::read_typed(s, v).unwrap_err()
                ),
                (s, Value::Str(_) | Value::Range(_)) if crate::range::element(s).is_some() => {
                    format!("is a {s}: {}", crate::value::read_typed(s, v).unwrap_err())
                }
                ("regex", Value::Str(x)) => format!(
                    "is a regex: {x:?} is not a valid pattern ({})",
                    regex::Regex::new(x).unwrap_err()
                ),
                _ => format!("is {s}, not {}", shown_literal(v)),
            })
        }
        (Ty::Enum(_), Term::Val(v)) => Some(format!("is {ty}, not {}", shown_literal(v))),
        _ => None,
    }
}

/// A declared type (an input's, a module input's) as the check reads it:
/// a resource type is a reference to one; an alias is already expanded.
pub fn of_expr(t: &TypeExpr) -> Ty {
    match t {
        TypeExpr::Name(n) if n.contains('.') => Ty::Ref(n.clone()),
        TypeExpr::Name(n) => Ty::parse(n),
        TypeExpr::Apply(n, args) => match (n.as_str(), args.as_slice()) {
            ("ref", [TypeExpr::Name(t) | TypeExpr::Str(t)]) => Ty::Ref(t.clone()),
            ("list" | "set", [x]) => Ty::List(Box::new(of_expr(x))),
            ("map", [x]) => Ty::Map(Box::new(of_expr(x))),
            ("secret", [x]) => Ty::Secret(Box::new(of_expr(x))),
            ("range", [TypeExpr::Name(t)]) => range(t),
            ("enum", xs) => Ty::Enum(
                xs.iter()
                    .filter_map(|x| match x {
                        TypeExpr::Name(a) | TypeExpr::Str(a) => Some(a.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => Ty::Any,
        },
        TypeExpr::Object(_) | TypeExpr::Str(_) => Ty::Any,
    }
}

/// The paths inside a value of the declared type `t` that it declares
/// `secret(T)`, each with T: `""` when `t` is one, `password` for `{ host:
/// string, password: secret(string) }`, through nested object types (an
/// alias is already expanded). The secrets pass reads a secret at such a
/// path as declared (R-118).
pub fn secret_fields(t: &TypeExpr) -> Vec<(String, Option<&TypeExpr>)> {
    match t {
        TypeExpr::Apply(n, args) if n == "secret" => vec![(String::new(), args.first())],
        TypeExpr::Object(fs) => fs
            .iter()
            .flat_map(|(k, t)| {
                secret_fields(t)
                    .into_iter()
                    .map(move |(p, x)| (dotted(k, &p), x))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `a.b`, or `a` when `b` is empty and `b` when `a` is (a scope `""`
/// is the program's).
pub fn dotted(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}.{b}"),
    }
}

/// The declared type at `path` inside `t` (`""` is `t`): an object type's
/// field, `None` past a type that has no fields.
pub fn field<'a>(t: &'a TypeExpr, path: &str) -> Option<&'a TypeExpr> {
    if path.is_empty() {
        return Some(t);
    }
    let (k, rest) = path.split_once('.').unwrap_or((path, ""));
    match t {
        TypeExpr::Object(fs) => field(&fs.iter().find(|(f, _)| f == k)?.1, rest),
        _ => None,
    }
}

/// An enum type's values, in declaration order: what `x in T` enumerates
/// (R-70) and `dform test` takes for an input of the type.
pub fn members(t: &TypeExpr) -> Option<Vec<String>> {
    match t {
        TypeExpr::Apply(n, args) if n == "enum" => args
            .iter()
            .map(|a| match a {
                TypeExpr::Str(v) | TypeExpr::Name(v) => Some(v.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A literal where `ty` is expected (R-31): read as that type (a string
/// is an `inet` or an `ip` where one is expected), or why it cannot be.
/// What is not a literal is left as it is.
pub fn literal(ty: &Ty, t: Term) -> Result<Term, String> {
    if let Some(why) = mismatch(ty, &t) {
        return Err(why);
    }
    Ok(read_as(ty, t))
}

/// A literal where the declared type `t` is expected: an object type's
/// fields each read as theirs, a list's elements as its element type,
/// anything else as [`literal`] (R-192). `Err((path, why))` at the first
/// that cannot be its type, `path` inside the value (`a`, `""` for all
/// of it). What is not a literal is left as it is.
pub fn declared(t: &TypeExpr, v: Term) -> Result<Term, (String, String)> {
    let field = |k: &str| match t {
        TypeExpr::Object(fs) => fs.iter().find(|(f, _)| f == k).map(|(_, t)| t),
        _ => None,
    };
    let elem = match t {
        TypeExpr::Apply(n, args) if n == "list" || n == "set" => args.first(),
        _ => None,
    };
    match v {
        Term::Obj(m) if matches!(t, TypeExpr::Object(_)) => m
            .into_iter()
            .map(|(k, x)| match field(&k) {
                Some(ft) => declared(ft, x)
                    .map(|x| (k.clone(), x))
                    .map_err(|(p, why)| (dotted(&k, &p), why)),
                None => Ok((k, x)),
            })
            .collect::<Result<_, _>>()
            .map(Term::Obj),
        Term::List(xs) if elem.is_some() => xs
            .into_iter()
            .map(|x| declared(elem.expect("matched"), x))
            .collect::<Result<_, _>>()
            .map(Term::List),
        Term::Val(v @ (Value::Obj(_) | Value::List(_)))
            if matches!(t, TypeExpr::Object(_)) || elem.is_some() =>
        {
            let terms = match v {
                Value::Obj(m) => Term::Obj(m.into_iter().map(|(k, x)| (k, Term::Val(x))).collect()),
                Value::List(xs) => Term::List(xs.into_iter().map(Term::Val).collect()),
                _ => unreachable!("matched"),
            };
            Ok(match declared(t, terms)? {
                Term::Obj(m) => match m
                    .into_iter()
                    .map(|(k, x)| match x {
                        Term::Val(x) => Ok((k, x)),
                        x => Err((k, x)),
                    })
                    .collect::<Result<_, _>>()
                {
                    Ok(m) => Term::Val(Value::Obj(m)),
                    Err(_) => unreachable!("a constant read is a constant"),
                },
                Term::List(xs) => Term::Val(Value::List(xs.into_iter().map(constant).collect())),
                x => x,
            })
        }
        v => literal(&of_expr(t), v).map_err(|why| (String::new(), why)),
    }
}

/// A computed term (a variable, a call) where a value type is wanted,
/// read as one when it has a value (R-134: there are no constructors, so
/// `let n: inet = cfg.net` is how a computed string becomes a network):
/// `__as(t, "inet")`, which a string that is not one leaves with no
/// value, an error at the position. A reference, an ambiguous quantity
/// (read by [`read`]) and what already reads it are left as they are.
pub fn at_run_time(ty: &Ty, t: Term) -> Term {
    let Ty::Scalar(s) = ty else {
        return match ty {
            Ty::Secret(inner) => at_run_time(inner, t),
            _ => t,
        };
    };
    // A number's text is read at an `int` or a `float` position too
    // (R-155: no `int(s)`), but for what is a number already.
    let number = s == "int" || s == "float";
    if !crate::value::is_value_type(s) && !number {
        return t;
    }
    // A call whose result is typed is checked as it is; what may be a
    // number's text is a variable or a read of a value (`cfg.port`).
    let typed = |t: &Term| match t {
        Term::Func { name, .. } => {
            crate::functions::get(name).is_none_or(|f| !matches!(f.ret.as_str(), "any" | "any?"))
        }
        _ => false,
    };
    if number && typed(&t) {
        return t;
    }
    match &t {
        Term::Func { name, .. }
            if matches!(
                name.as_str(),
                AS | AMBIGUOUS
                    | crate::address::REF
                    | crate::address::CLOUD_REF
                    | crate::address::SCOPED
            ) =>
        {
            t
        }
        Term::Var(_) | Term::Func { .. } => Term::Func {
            name: AS.to_string(),
            args: vec![t, Term::Val(Value::Str(s.clone()))],
        },
        _ => t,
    }
}

/// The internal function a typed position over a computed value lowers
/// to ([`at_run_time`]).
pub const AS: &str = "__as";

fn read_as(ty: &Ty, t: Term) -> Term {
    match (ty, t) {
        (Ty::Scalar(s), t) if ty.measured() => match measure(s, &t) {
            Some(Ok(v)) => Term::Val(v),
            _ => at_run_time(ty, t),
        },
        (Ty::Scalar(s), Term::Val(Value::Int(i))) if s == "float" => {
            Term::Val(crate::value::Float::new(i as f64).map_or(Value::Int(i), Value::Float))
        }
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "inet" => {
            match crate::value::parse_ipnet(&x) {
                Some((addr, prefix)) => Term::Val(Value::IpNet { addr, prefix }),
                None => Term::Val(Value::Str(x)),
            }
        }
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "ip" => {
            match crate::value::ipv4_to_u32(&x) {
                Some(n) => Term::Val(Value::Ip(n)),
                None => Term::Val(Value::Str(x)),
            }
        }
        // A string in a uri position is a uri, parsed at compile time.
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "uri" => {
            Term::Val(crate::value::parse_uri(&x).unwrap_or(Value::Str(x)))
        }
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "oci" => {
            Term::Val(crate::value::parse_oci(&x).unwrap_or(Value::Str(x)))
        }
        (Ty::Scalar(s), Term::Val(v @ Value::Str(_))) if s == "semver" => {
            Term::Val(crate::value::read_typed(s, &v).unwrap_or(v))
        }
        // A range's text, or a literal's ends, read as the range (R-180).
        (Ty::Scalar(s), Term::Val(v @ (Value::Str(_) | Value::Range(_))))
            if crate::range::element(s).is_some() =>
        {
            Term::Val(crate::value::read_typed(s, &v).unwrap_or(v))
        }
        (Ty::Scalar(s), Term::Func { name, args })
            if name == crate::range::LOWERED && crate::range::element(s).is_some() =>
        {
            range_read(s, args)
        }
        (Ty::Secret(inner), t) => read_as(inner, t),
        (Ty::Scalar(_), t @ (Term::Var(_) | Term::Func { .. })) => at_run_time(ty, t),
        (Ty::List(inner), Term::List(xs)) => {
            Term::List(xs.into_iter().map(|x| read_as(inner, x)).collect())
        }
        (Ty::Map(inner), Term::Obj(m)) => {
            Term::Obj(m.into_iter().map(|(k, x)| (k, read_as(inner, x))).collect())
        }
        (_, t) => t,
    }
}

/// `__range(start, end, inclusive)` where a `range(T)` is wanted: its
/// ends read as `T` (`100m..=1` a cpu's), the range when both are known,
/// else read at run time.
fn range_read(ty: &str, args: Vec<Term>) -> Term {
    let elem = Ty::Scalar(crate::range::element(ty).unwrap_or_default().to_string());
    let args: Vec<Term> = args
        .into_iter()
        .enumerate()
        .map(|(i, a)| if i < 2 { read_as(&elem, a) } else { a })
        .collect();
    folded(args).unwrap_or_else(|args| at_run_time(&Ty::Scalar(ty.to_string()), args))
}

/// A range literal's ends as written, an ambiguous quantity (`500m`) read
/// as the one dimension that reads both (`100m..=1` a cpu's, `1h..=90m`
/// a duration's); left for the position to read when none or two do.
pub fn range_ends(a: Term, b: Term) -> (Term, Term) {
    if ambiguous(&a).is_none() && ambiguous(&b).is_none() {
        return (a, b);
    }
    let text = |t: &Term| match t {
        Term::Val(Value::Quantity(q)) => Some((Some(q.dim()), q.to_string())),
        Term::Val(Value::Int(n)) => Some((None, n.to_string())),
        Term::Val(Value::Float(f)) => Some((None, f.to_string())),
        t => ambiguous(t).map(|s| (None, s.to_string())),
    };
    let (Some((da, ta)), Some((db, tb))) = (text(&a), text(&b)) else {
        return (a, b);
    };
    let reads = |d: Dim| {
        Some(d) == da.or(db).or(Some(d))
            && quantity::read(d, &ta).is_ok()
            && quantity::read(d, &tb).is_ok()
    };
    let dims: Vec<Dim> = [Dim::Bytes, Dim::Cpu, Dim::Duration]
        .into_iter()
        .filter(|d| reads(*d))
        .collect();
    match dims.as_slice() {
        [d] => {
            let q = |t: &str| quantity::read(*d, t).map(|q| Term::Val(Value::Quantity(q)));
            match (q(&ta), q(&tb)) {
                (Ok(x), Ok(y)) => (x, y),
                _ => (a, b),
            }
        }
        _ => (a, b),
    }
}

/// A range literal's term: the range when its ends are known (`Err` with
/// the `__range` call otherwise, or when they make none).
pub fn folded(args: Vec<Term>) -> Result<Term, Term> {
    if let [
        Term::Val(a),
        Term::Val(b),
        Term::Val(Value::Bool(inclusive)),
    ] = args.as_slice()
        && let Ok(r) = crate::range::Range::new(a.clone(), b.clone(), *inclusive)
    {
        return Ok(Term::Val(r.into()));
    }
    Err(Term::Func {
        name: crate::range::LOWERED.to_string(),
        args,
    })
}

/// Check every contribution to a schema-typed attribute; one error per
/// contribution that cannot have its attribute's type.
pub fn check(program: &Program, schema: &Schema) -> Result<()> {
    let mut diags = Vec::new();
    for s in &program.statements {
        let head = match s {
            Stmt::Fact(a) => a,
            Stmt::Rule(r) => &r.head,
            _ => continue,
        };
        if head.pred != "arg" || head.args.len() != 5 {
            continue;
        }
        let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) =
            (&head.args[0], &head.args[2])
        else {
            continue;
        };
        // The provider's name of a reference the program names for the
        // resource (`schema::reference_name`).
        if let Some(named) = schema.program_name(typ, path) {
            diags.push(
                Diagnostic::error(
                    head.span,
                    format!("`{path}` is `{named}` here: a reference is the resource"),
                )
                .with_help(format!("write `{named}`")),
            );
            continue;
        }
        let Some(spec) = schema.attr(typ, path) else {
            continue;
        };
        if let Some(why) = mismatch(&Ty::parse(&spec.ty), &head.args[3]) {
            let at = match &head.args[1] {
                Term::Val(Value::Str(a)) => Address {
                    typ: typ.clone(),
                    name: a.clone(),
                }
                .attr(path),
                _ => format!("{typ}.{path}"),
            };
            diags.push(Diagnostic::error(head.span, format!("{at} {why}")));
        }
    }
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// The errors of what no position read: a quantity literal no type reads
/// (`500m`), a name two resources share no `ref(T)` picks (R-74).
fn left_unread(program: &Program, diags: &mut Vec<Diagnostic>) {
    for s in &program.statements {
        stmt_terms(s, &mut |t, span| {
            visit(t, &mut |t| {
                let at = || ambiguous_span(t).filter(|s| !s.is_none()).unwrap_or(span);
                if let Some(x) = ambiguous(t) {
                    diags.push(Diagnostic::error(at(), quantity::ambiguous(x)));
                }
                if let Some((name, cs)) = ambiguous_ref_of(t) {
                    let types: Vec<String> = cs
                        .iter()
                        .filter_map(|c| reference(c).map(|(typ, _)| typ.to_string()))
                        .collect();
                    diags.push(Diagnostic::error(at(), ambiguous_resource(name, &types)));
                }
            })
        });
    }
}

/// The types a `set` through a variable (`r`, its head `arg(X, A, path,
/// V)`) may write: those its body binds the variable to (R-185, `x in
/// workload`), else every type of the schema (of the namespace, where the
/// rule names one, `x in k8s`) that declares the attribute or a path
/// under it.
pub(crate) fn set_types(schema: &Schema, r: &crate::ast::RuleStmt, path: &str) -> Vec<String> {
    let types = row_types(&r.head.args[0], &r.body);
    match types.is_empty() {
        true => any_types(schema, r, path).into_iter().collect(),
        false => types,
    }
}

/// The types of the schema (of the namespace a rule's `x in NS` names)
/// that declare `path` or a path under it.
fn any_types(
    schema: &Schema,
    r: &crate::ast::RuleStmt,
    path: &str,
) -> std::collections::BTreeSet<String> {
    let namespace = r.body.iter().find_map(|l| match l {
        Lit::Pos(a) if a.pred == "__namespace" && a.args.get(1) == Some(&r.head.args[0]) => {
            match a.args.first() {
                Some(Term::Val(Value::Str(ns))) => Some(format!("{ns}.")),
                _ => None,
            }
        }
        _ => None,
    });
    let under = format!("{path}.");
    schema
        .attrs
        .keys()
        .filter(|(_, p)| p == path || p.starts_with(&under))
        .map(|(t, _)| t.clone())
        .filter(|t| {
            namespace
                .as_ref()
                .is_none_or(|ns| t.starts_with(ns.as_str()))
        })
        .collect()
}

/// Whether `t` holds a quantity only a position's type reads.
fn holds_ambiguous(t: &Term) -> bool {
    let mut found = false;
    visit(t, &mut |x| found |= ambiguous(x).is_some());
    found
}

/// The types a rule's type variable `typ` may be, by the `member([T1,
/// T2], typ)` its body binds it with (`columns::row_types`).
fn row_types(typ: &Term, body: &[Lit]) -> Vec<String> {
    body.iter()
        .find_map(|l| match l {
            Lit::Pos(a) if a.pred == "member" && a.args.get(1) == Some(typ) => match &a.args[0] {
                Term::List(xs) => xs
                    .iter()
                    .map(|x| match x {
                        Term::Val(Value::Str(t)) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default()
}

/// Read the value at `path` of a `typ`, and what it holds at nested
/// paths; `Err((path, why))` at the first that cannot be its type.
fn read_at(schema: &Schema, typ: &str, path: &str, t: &mut Term) -> Result<(), (String, String)> {
    if let Some(spec) = schema.attr(typ, path) {
        let ty = Ty::parse(&spec.ty);
        pick_refs(&ty, t);
        if ty.measured() {
            if let Some(why) = mismatch(&ty, t) {
                return Err((path.to_string(), why));
            }
            let v = std::mem::replace(t, Term::Wildcard);
            *t = read_as(&ty, v);
            return Ok(());
        }
        // A value where a value type is wanted is read as one (R-134): a
        // literal now (`check` says why one is not), a computed value at
        // run time.
        if let Ty::Scalar(s) = &ty
            && crate::value::is_value_type(s)
        {
            let v = std::mem::replace(t, Term::Wildcard);
            *t = match v {
                Term::Func { ref name, .. } if name == crate::range::LOWERED => read_as(&ty, v),
                Term::Var(_) | Term::Func { .. } => at_run_time(&ty, v),
                v => read_as(&ty, v),
            };
            return Ok(());
        }
    }
    match t {
        Term::Obj(m) => {
            for (k, v) in m {
                read_at(schema, typ, &format!("{path}.{k}"), v)?;
            }
        }
        // `{ ..a, k: v }`, `[..a, x]`: what is written beside a spread
        // is the value's (R-192).
        Term::Func { name, args }
            if name == crate::functions::MERGE || name == crate::functions::CONCAT =>
        {
            for a in args {
                read_at(schema, typ, path, a)?;
            }
        }
        Term::List(xs) => {
            for x in xs {
                read_at(schema, typ, path, x)?;
            }
        }
        Term::Val(v @ (Value::Obj(_) | Value::List(_))) => {
            let mut held = Term::Val(std::mem::replace(v, Value::Bool(false)));
            // A constant object or list: its elements as terms, read, and
            // folded back.
            let r = match &mut held {
                Term::Val(Value::Obj(m)) => {
                    let mut terms: std::collections::BTreeMap<String, Term> = std::mem::take(m)
                        .into_iter()
                        .map(|(k, v)| (k, Term::Val(v)))
                        .collect();
                    let r = terms
                        .iter_mut()
                        .try_for_each(|(k, v)| read_at(schema, typ, &format!("{path}.{k}"), v));
                    *m = terms.into_iter().map(|(k, t)| (k, constant(t))).collect();
                    r
                }
                Term::Val(Value::List(xs)) => {
                    let mut terms: Vec<Term> =
                        std::mem::take(xs).into_iter().map(Term::Val).collect();
                    let r = terms
                        .iter_mut()
                        .try_for_each(|v| read_at(schema, typ, path, v));
                    *xs = terms.into_iter().map(constant).collect();
                    r
                }
                _ => Ok(()),
            };
            if let Term::Val(x) = held {
                *v = x;
            }
            r?;
        }
        _ => {}
    }
    Ok(())
}

/// A term read from a constant: still one.
fn constant(t: Term) -> Value {
    match t {
        Term::Val(v) => v,
        // `read_term` keeps a constant a constant.
        _ => unreachable!("a constant read is a constant"),
    }
}

/// What an operand is, as far as the compiler sees it.
#[derive(Clone, Copy, PartialEq)]
enum Operand {
    Q(Dim),
    Time,
    Int,
    Float,
    /// `500m`: the other side says which.
    Ambiguous,
}

fn operand(t: &Term) -> Option<Operand> {
    Some(match t {
        Term::Val(Value::Quantity(q)) => Operand::Q(q.dim()),
        Term::Val(Value::Time(_)) => Operand::Time,
        Term::Val(Value::Int(_)) => Operand::Int,
        Term::Val(Value::Float(_)) => Operand::Float,
        t if ambiguous(t).is_some() => Operand::Ambiguous,
        _ => return None,
    })
}

fn operand_name(o: Operand) -> &'static str {
    match o {
        Operand::Q(d) => d.name(),
        Operand::Time => "a time",
        Operand::Int | Operand::Float => "a number",
        Operand::Ambiguous => "a quantity",
    }
}

/// The operands of `op` as written (`+ - * / %`, or a comparison) as the
/// compiler reads them (R-66): an ambiguous literal
/// takes the other side's quantity (`1h + 30m`, `c > 500m` where `c` is
/// a cpu literal), and an operation that mixes dimensions or changes one
/// is an error: a quantity scales by a number, adds, subtracts and
/// compares within its dimension, and over its own dimension is a number;
/// a time adds or subtracts a duration and compares with a time. Operands
/// the compiler cannot see are checked at evaluation, where a mix has no
/// value.
pub fn operands(op: &str, a: Term, b: Term) -> Result<(Term, Term), String> {
    let (Some(x), Some(y)) = (operand(&a), operand(&b)) else {
        return Ok((a, b));
    };
    let read = |t: Term, d: Dim| -> Result<(Term, Operand), String> {
        let text = match &t {
            Term::Val(Value::Float(f)) => f.to_string(),
            t => ambiguous(t).unwrap_or_default().to_string(),
        };
        quantity::read(d, &text)
            .map(|q| (Term::Val(Value::Quantity(q)), Operand::Q(d)))
            .map_err(|e| format!("{e}, where the other side is {}", d.name()))
    };
    // A decimal is a quantity where it adds to or compares with one
    // (`c > 0.5` where `c` is cpu); it does not scale one.
    let measures = !matches!(op, "*" | "/" | "%");
    let ((a, x), (b, y)) = match (x, y) {
        (Operand::Ambiguous, Operand::Q(d)) => (read(a, d)?, (b, y)),
        (Operand::Q(d), Operand::Ambiguous) => ((a, x), read(b, d)?),
        (Operand::Float, Operand::Q(d)) if measures => (read(a, d)?, (b, y)),
        (Operand::Q(d), Operand::Float) if measures => ((a, x), read(b, d)?),
        _ => ((a, x), (b, y)),
    };
    let shown = |t: &Term| match ambiguous(t) {
        Some(s) => s.to_string(),
        None => spell::term(t),
    };
    let written = format!("`{} {op} {}`", shown(&a), shown(&b));
    let op = match op {
        "+" => "add",
        "-" => "sub",
        "*" => "mul",
        "/" => "div",
        "%" => "mod",
        _ => "cmp",
    };
    let mix = || {
        Err(format!(
            "{written} mixes {} and {}: a quantity adds, subtracts and compares only with its \
             own dimension",
            operand_name(x),
            operand_name(y)
        ))
    };
    use Operand::*;
    let dur = Q(Dim::Duration);
    match (op, x, y) {
        (_, Ambiguous, _) | (_, _, Ambiguous) => return Ok((a, b)),
        (_, Int | Float, Int | Float) => {}
        ("mul", Q(_), Float) | ("mul", Float, Q(_)) | ("div", Q(_), Float) => {
            return Err(format!(
                "{written}: a quantity scales by an int; a float scales none"
            ));
        }
        ("add" | "sub" | "cmp", Q(p), Q(q)) if p == q => {}
        ("add" | "sub", Time, d) if d == dur => {}
        ("add", d, Time) if d == dur => {}
        ("cmp", Time, Time) => {}
        ("add" | "sub" | "cmp", _, _) => return mix(),
        ("mul", Q(_), Int) | ("mul", Int, Q(_)) => {}
        ("mul", _, _) => {
            return Err(format!(
                "{written}: a quantity scales by a number; no operation changes its dimension"
            ));
        }
        ("div", Q(_), Int) => {}
        ("div", Q(p), Q(q)) if p == q => {}
        ("div", Q(_), Q(_)) => return mix(),
        ("div", _, _) => {
            return Err(format!(
                "{written}: a quantity is divided by a number, or by its own dimension for a ratio"
            ));
        }
        _ => return Err(format!("{written}: `%` is for numbers")),
    }
    Ok((a, b))
}

/// Every term a statement holds, with the span an error about it points at.
fn stmt_terms(s: &Stmt, f: &mut dyn FnMut(&Term, crate::ast::Span)) {
    let lits = |body: &[Lit], span, f: &mut dyn FnMut(&Term, crate::ast::Span)| {
        for l in body {
            lit_terms(l, &mut |t| f(t, span));
        }
    };
    match s {
        Stmt::Fact(a) => a.args.iter().for_each(|t| f(t, a.span)),
        Stmt::Rule(r) => {
            r.head.args.iter().for_each(|t| f(t, r.head.span));
            lits(&r.body, r.head.span, f);
        }
        Stmt::Resource(r) => {
            for x in &r.fields {
                f(&x.value, x.span);
            }
            lits(r.body.as_deref().unwrap_or_default(), r.span, f);
        }
        Stmt::Module(m) => m.body.iter().for_each(|s| stmt_terms(s, f)),
        Stmt::Instance(i) | Stmt::Use(i) => i.inputs.iter().for_each(|(_, t, span)| f(t, *span)),
        Stmt::Output(o) => o.value.iter().for_each(|t| f(t, o.span)),
        Stmt::Input(i) => i.default.iter().for_each(|t| f(t, i.span)),
        _ => {}
    }
}

fn lit_terms(l: &Lit, f: &mut dyn FnMut(&Term)) {
    match l {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(f),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            f(a);
            f(b);
        }
    }
}

/// `t` and every term inside it.
fn visit(t: &Term, f: &mut dyn FnMut(&Term)) {
    f(t);
    match t {
        Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|x| visit(x, f)),
        Term::Obj(m) => m.values().for_each(|x| visit(x, f)),
        Term::ListComp { item, body } => {
            visit(item, f);
            for l in body {
                lit_terms(l, &mut |t| visit(t, f));
            }
        }
        _ => {}
    }
}

// --- over the program's nodes -------------------------------------------

/// What a line of a `set` writes: a node, or an element's content
/// (`items::element_write`: its key, the path under the element, the
/// value).
#[derive(Clone, Copy)]
enum Written<'p> {
    Node(ExprId),
    Elem {
        key: ExprId,
        rest: &'p [String],
        value: ExprId,
    },
}

/// The readings of a program's attributes (the half of [`read`] that reads
/// a resource's entries and a `set`'s values at their schema paths), by
/// node.
struct Reader<'p> {
    p: &'p prog::Program,
    schema: &'p Schema,
    read: Readings,
    diags: Vec<Diagnostic>,
}

impl<'p> Reader<'p> {
    fn items(&mut self, ids: &[ItemId]) {
        let p = self.p;
        for &id in ids {
            let it = &p.items[id];
            match &it.kind {
                ItemKind::Module { items, .. } => self.items(items),
                ItemKind::Resource {
                    typ, name, body, ..
                } => self.resource(&typ.name, name, body),
                ItemKind::Set { writes, clause } => {
                    for w in writes {
                        self.write(w, *clause);
                    }
                }
                _ => {}
            }
        }
    }

    /// Each entry at its attribute's path.
    fn resource(&mut self, typ: &str, name: &Header, body: &ResourceBody) {
        let p = self.p;
        let name = crate::edges::header_term(name);
        // A constant object's keys, each read and folded back.
        if let ResourceBody::Value(v) = body
            && p.exprs[*v].hoisted.is_none()
            && let ExprKind::Lit(Value::Obj(m)) = &p.exprs[*v].kind
        {
            let span = p.exprs[*v].span;
            let mut read = m.clone();
            for (k, x) in &mut read {
                let key = crate::address::path_join("", k);
                let mut t = Term::Val(x.clone());
                if let Err((path, why)) = read_at(self.schema, typ, &key, &mut t) {
                    let at = crate::edges::shown_attr(typ, &name, &path);
                    self.diags
                        .push(Diagnostic::error(span, format!("{at} {why}")));
                }
                *x = constant(t);
            }
            if read != *m {
                self.read.insert(*v, Term::Val(Value::Obj(read)));
            }
            return;
        }
        for (key, part, span) in crate::edges::entries(p, body) {
            let crate::edges::Part::Node(e) = part else {
                unreachable!("a value's entries are its nodes")
            };
            let mut out = std::mem::take(&mut self.read);
            let r = self.at(typ, &key, e, &mut out);
            self.read = out;
            if let Err((path, why)) = r {
                let at = crate::edges::shown_attr(typ, &name, &path);
                self.diags
                    .push(Diagnostic::error(span, format!("{at} {why}")));
            }
        }
    }

    /// `arg(T, A, P, V, ..)`: `V` at `T`'s `P`; through a variable of
    /// several types (R-185) as each, which must agree.
    fn write(&mut self, w: &'p Write, clause: Option<ClauseId>) {
        if !matches!(w.op, crate::ast::FieldOp::Assign) {
            return;
        }
        let (typ, addr, path, written, cell) = match &w.target {
            Target::Attr { typ, addr, path } => {
                let typ = prog::lower_expr(self.p, *typ).0;
                (
                    typ,
                    Some(*addr),
                    path.as_str(),
                    Written::Node(w.value),
                    vec![],
                )
            }
            Target::Element {
                typ,
                addr,
                list,
                key,
                rest,
            } => {
                let typ = prog::lower_expr(self.p, *typ).0;
                let elem = Written::Elem {
                    key: *key,
                    rest,
                    value: w.value,
                };
                (typ, Some(*addr), list.as_str(), elem, vec![*key])
            }
            Target::Input { path } => (
                str_term(crate::modules::INPUT),
                None,
                path.as_str(),
                Written::Node(w.value),
                vec![],
            ),
        };
        match &typ {
            Term::Var(_) => {
                let reads = cell
                    .iter()
                    .chain(addr.iter())
                    .chain([&w.value])
                    .any(|e| !prog::lower_expr(self.p, *e).1.is_empty());
                if clause.is_some() || reads {
                    self.through_var(w, typ, path, written, clause);
                }
            }
            Term::Val(Value::Str(t)) => {
                let mut out = std::mem::take(&mut self.read);
                let r = self.written(t, path, written, &mut out);
                self.read = out;
                if let Err((path, why)) = r {
                    let name = match addr {
                        Some(a) => prog::lower_expr(self.p, a).0,
                        None => str_term(""),
                    };
                    let at = crate::edges::shown_attr(t, &name, &path);
                    self.diags
                        .push(Diagnostic::error(w.span, format!("{at} {why}")));
                }
            }
            _ => {}
        }
    }

    /// A `set` through a variable of several types (R-185: `set
    /// w.spec.x = v where workload(w)`): read as each, which must agree;
    /// through any type (`x in resource`), a quantity only a position's
    /// type reads at the attribute's edge ([`at_edge`]).
    fn through_var(
        &mut self,
        w: &Write,
        typ: Term,
        path: &str,
        written: Written,
        clause: Option<ClauseId>,
    ) {
        let r = crate::edges::set_rule(self.p, typ, clause);
        let types = row_types(&r.head.args[0], &r.body);
        if types.is_empty() {
            if let Some(read) = self.at_edge(&r, path, written) {
                self.read.extend(read);
            }
            return;
        }
        let mut read: Option<(Term, Readings)> = None;
        for typ in &types {
            let mut tmp = Readings::new();
            if let Err((path, why)) = self.written(typ, path, written, &mut tmp) {
                self.diags
                    .push(Diagnostic::error(w.span, format!("{typ}.{path} {why}")));
                return;
            }
            let v = self.term(written, &tmp);
            match &read {
                Some((first, _)) if *first != v => {
                    self.diags.push(Diagnostic::error(
                        w.span,
                        format!(
                            "{path} is read one way for {} and another for {typ}: write \
                             it once per type",
                            types[0]
                        ),
                    ));
                    return;
                }
                Some(_) => {}
                None => read = Some((v, tmp)),
            }
        }
        if let Some((_, tmp)) = read {
            self.read.extend(tmp);
        }
    }

    /// [`at_edge`] of a written value: the readings of the types that
    /// read it alike.
    fn at_edge(&self, r: &crate::ast::RuleStmt, path: &str, written: Written) -> Option<Readings> {
        if !holds_ambiguous(&self.term(written, &Readings::new())) {
            return None;
        }
        let mut read: Option<(Term, Readings)> = None;
        for typ in &any_types(self.schema, r, path) {
            let mut tmp = Readings::new();
            if self.written(typ, path, written, &mut tmp).is_err() {
                continue;
            }
            let v = self.term(written, &tmp);
            if holds_ambiguous(&v) {
                continue;
            }
            match &read {
                Some((first, _)) if *first != v => return None,
                Some(_) => {}
                None => read = Some((v, tmp)),
            }
        }
        read.map(|(_, tmp)| tmp)
    }

    /// The term a line writes, its nodes read as `readings` reads them.
    fn term(&self, written: Written, readings: &Readings) -> Term {
        match written {
            Written::Node(e) => prog::read_expr(self.p, readings, e),
            Written::Elem { key, rest, value } => prog::lower::element_write(
                prog::read_expr(self.p, readings, key),
                rest,
                prog::read_expr(self.p, readings, value),
            ),
        }
    }

    /// [`read_at`] of what a line writes at `typ`'s `path`, its readings
    /// onto `out`.
    fn written(
        &self,
        typ: &str,
        path: &str,
        written: Written,
        out: &mut Readings,
    ) -> Result<(), (String, String)> {
        match written {
            Written::Node(e) => self.at(typ, path, e, out),
            // `{"[key]": k, p: {q: v}}` at the list's path: the content's
            // paths under it (`l.p.q`); its key and a value held whole
            // under `[value]` no path of the schema's.
            Written::Elem { rest, value, .. } => {
                if !rest.is_empty() {
                    let mut at = path.to_string();
                    for k in rest {
                        at = format!("{at}.{k}");
                    }
                    return self.at(typ, &at, value, out);
                }
                let p = self.p;
                if p.exprs[value].hoisted.is_some() {
                    return Ok(());
                }
                match &p.exprs[value].kind {
                    ExprKind::Object { parts } if !object_spreads(parts) => {
                        for (k, v) in fields(parts) {
                            self.at(typ, &format!("{path}.{k}"), v, out)?;
                        }
                        Ok(())
                    }
                    ExprKind::Lit(Value::Obj(m)) => {
                        let mut read = m.clone();
                        let mut r = Ok(());
                        for (k, x) in &mut read {
                            let mut t = Term::Val(x.clone());
                            if r.is_ok() {
                                r = read_at(self.schema, typ, &format!("{path}.{k}"), &mut t);
                            }
                            *x = constant(t);
                        }
                        if read != *m {
                            out.insert(value, Term::Val(Value::Obj(read)));
                        }
                        r
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    /// [`read_at`] over the nodes: the node `e` at `typ`'s `path`, its
    /// readings onto `out`.
    fn at(
        &self,
        typ: &str,
        path: &str,
        e: ExprId,
        out: &mut Readings,
    ) -> Result<(), (String, String)> {
        let p = self.p;
        let x = &p.exprs[e];
        let leaf = matches!(
            x.kind,
            ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Range { .. }
        );
        // A literal: its term, read.
        if x.hoisted.is_none() && leaf {
            let mut t = prog::lower_expr(p, e).0;
            let before = t.clone();
            let r = read_at(self.schema, typ, path, &mut t);
            if t != before {
                out.insert(e, t);
            }
            return r;
        }
        if let Some(spec) = self.schema.attr(typ, path)
            && let Some(r) = self.at_attr(&Ty::parse(&spec.ty), path, e, out)
        {
            return r;
        }
        if x.hoisted.is_some() {
            return Ok(());
        }
        match &x.kind {
            ExprKind::Object { parts } if !object_computes(parts) => {
                // `{ ..a, k: v }`: what is written beside a spread is the
                // value's (R-192).
                for part in object_parts(parts) {
                    match part {
                        Ok(fields) => {
                            for (k, v) in fields {
                                self.at(typ, &format!("{path}.{k}"), v, out)?;
                            }
                        }
                        Err(s) => self.at(typ, path, s, out)?,
                    }
                }
                Ok(())
            }
            ExprKind::List { parts } => {
                for part in parts {
                    match part {
                        ListPart::Elem(v) | ListPart::Spread(v) => self.at(typ, path, *v, out)?,
                    }
                }
                Ok(())
            }
            ExprKind::Alias(a) => self.at(typ, path, *a, out),
            _ => Ok(()),
        }
    }

    /// The node `e` at an attribute of type `ty`: a name two resources
    /// share picked by a `ref(T)` (R-74); a quantity's or a time's
    /// position read as one, a value type's as one (R-134), now or at run
    /// time. `None` where the type reads it as its parts.
    fn at_attr(
        &self,
        ty: &Ty,
        path: &str,
        e: ExprId,
        out: &mut Readings,
    ) -> Option<Result<(), (String, String)>> {
        let p = self.p;
        self.pick(ty, e, out);
        let unread = prog::lower_expr(p, e).0;
        let t = if ty.measured() {
            let t = prog::read_expr(p, out, e);
            if let Some(why) = mismatch(ty, &t) {
                return Some(Err((path.to_string(), why)));
            }
            read_as(ty, t)
        } else if let Ty::Scalar(s) = ty
            && crate::value::is_value_type(s)
        {
            let t = prog::read_expr(p, out, e);
            match &t {
                Term::Func { name, .. } if name == crate::range::LOWERED => read_as(ty, t),
                Term::Var(_) | Term::Func { .. } => at_run_time(ty, t),
                _ => read_as(ty, t),
            }
        } else {
            return None;
        };
        if t != unread {
            out.insert(e, t);
        }
        Some(Ok(()))
    }

    /// [`pick_refs`] over the nodes.
    fn pick(&self, ty: &Ty, e: ExprId, out: &mut Readings) {
        let p = self.p;
        match ty {
            Ty::Secret(inner) => self.pick(inner, e, out),
            Ty::List(inner) => {
                if p.exprs[e].hoisted.is_none()
                    && let ExprKind::List { parts } = &p.exprs[e].kind
                    && !parts.iter().any(|x| matches!(x, ListPart::Spread(_)))
                {
                    for part in parts {
                        if let ListPart::Elem(x) = part {
                            self.pick(inner, *x, out);
                        }
                    }
                }
            }
            Ty::Ref(want) => {
                let t = prog::read_expr(p, out, e);
                let picked = ambiguous_ref_of(&t).and_then(|(_, cs)| {
                    let mut of = cs
                        .iter()
                        .filter(|c| reference(c).is_some_and(|(typ, _)| typ == want));
                    of.next().filter(|_| of.next().is_none()).cloned()
                });
                if let Some(c) = picked {
                    out.insert(e, c);
                }
            }
            _ => {}
        }
    }
}

/// Whether an object's parts compute a key (`__object`).
fn object_computes(parts: &[ObjPart]) -> bool {
    parts.iter().any(|p| matches!(p, ObjPart::Computed { .. }))
}

/// Whether an object's parts spread one (`__merge`).
fn object_spreads(parts: &[ObjPart]) -> bool {
    parts.iter().any(|p| matches!(p, ObjPart::Spread(_)))
}

/// An object's written fields as `lower` writes them: by key, the last
/// written of a key its value.
fn fields(parts: &[ObjPart]) -> BTreeMap<&str, ExprId> {
    parts
        .iter()
        .filter_map(|p| match p {
            ObjPart::Field { key, value, .. } => Some((key.as_str(), *value)),
            _ => None,
        })
        .collect()
}

/// An object's parts as `lower` writes them: each run of written fields
/// one object (`Ok`), each spread its source (`Err`).
fn object_parts(parts: &[ObjPart]) -> Vec<Result<BTreeMap<&str, ExprId>, ExprId>> {
    let mut out = Vec::new();
    let mut run: Option<BTreeMap<&str, ExprId>> = None;
    for p in parts {
        match p {
            ObjPart::Field { key, value, .. } => {
                run.get_or_insert_default().insert(key.as_str(), *value);
            }
            ObjPart::Spread(s) => {
                out.extend(run.take().map(Ok));
                out.push(Err(*s));
            }
            ObjPart::Computed { .. } => {}
        }
    }
    let spread = object_spreads(parts);
    match (spread, run) {
        (false, run) => out.push(Ok(run.unwrap_or_default())),
        (true, run) => out.extend(run.map(Ok)),
    }
    out
}

/// What every literal of the program's nodes is read as, now the schema
/// is known (R-66, R-62, R-134, R-192): each node's reading, and the
/// errors but those of what no position reads ([`read`]).
pub fn readings(program: &prog::Program, schema: &Schema) -> (Readings, Vec<Diagnostic>) {
    if !program.diags.is_empty() {
        return (Readings::new(), program.diags.clone());
    }
    let mut r = Reader {
        p: program,
        schema,
        read: Readings::new(),
        diags: Vec::new(),
    };
    r.items(&program.roots);
    let (edges, diags) = crate::edges::read_nodes(program, schema);
    r.read.extend(edges);
    r.diags.extend(diags);
    (r.read, r.diags)
}

/// Read every literal as its position's type, once the providers' schemas
/// are known: a quantity's or a time's (R-66, R-62), a value type's (R-134;
/// an attribute the schema types, at its own path or nested in an object
/// or list value, `containers.resources.limits.memory`), and every other
/// at the edges its value reaches (R-192). The program evaluated is
/// `program`, whose first statements are what its nodes lower to and the
/// rest what the run adds (an input file's facts, the policy rules,
/// declared externs), with its nodes lowered as read ([`readings`]) in
/// their place: no statement is rewritten. The program read carries its
/// nodes with their readings, what its copies are instanced from. A
/// program with no nodes has none to read. Then a quantity literal no
/// position read (`500m` where nothing gives a type) is an error naming
/// both readings.
pub fn read(program: &Program, schema: &Schema) -> Result<Program> {
    let mut nodes = program.nodes.as_deref().cloned().unwrap_or_default();
    let (readings, mut diags) = readings(&nodes, schema);
    nodes.readings = readings;
    let read = crate::program::lower(&nodes)
        .into_result()
        .map_err(Diagnostics)?;
    let mut out = program.clone();
    let n = read.statements.len();
    out.statements.splice(..n, read.statements);
    out.nodes = program.nodes.as_ref().map(|_| std::rc::Rc::new(nodes));
    left_unread(&out, &mut diags);
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_parse_as_the_schema_writes_them() {
        assert_eq!(Ty::parse("ref(net.vpc)"), Ty::Ref("net.vpc".into()));
        assert_eq!(
            Ty::parse("list(ref(net.subnet))"),
            Ty::List(Box::new(Ty::Ref("net.subnet".into())))
        );
        assert_eq!(Ty::parse("map"), Ty::Any);
        assert_eq!(
            Ty::parse("map(string)"),
            Ty::Map(Box::new(Ty::Scalar("string".into())))
        );
        assert_eq!(Ty::parse("list"), Ty::Any);
        assert_eq!(
            Ty::parse("enum(\"a\", \"b\")"),
            Ty::Enum(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn the_measured_types_are_the_quantities_and_time() {
        for t in ["bytes", "cpu", "duration", "time"] {
            assert!(measured(t) && Ty::parse(t).measured(), "{t}");
        }
        assert!(!measured("int") && !Ty::parse("inet").measured());
    }

    /// A program that writes the provider's name of a reference is told
    /// the name it has, at the site.
    #[test]
    fn the_providers_name_of_a_reference_names_the_programs() {
        let schema = Schema::parse("type_attr(\"t.s\", \"vpc_id\", \"ref(t.v)\", [])\n", "test")
            .and_then(Schema::imported)
            .unwrap();
        let s = |x: &str| Term::Val(Value::Str(x.into()));
        let main = Term::Func {
            name: crate::address::REF.into(),
            args: vec![s("t.v"), s("main"), s("")],
        };
        let arg = |path: &str| {
            Stmt::Fact(crate::ast::Atom {
                pred: "arg".into(),
                args: vec![s("t.s"), s("a"), s(path), main.clone(), s("")],
                record: None,
                span: Default::default(),
            })
        };
        let program = |path: &str| Program {
            statements: vec![arg(path)],
            ..Default::default()
        };
        assert!(check(&program("vpc"), &schema).is_ok());
        let e = check(&program("vpc_id"), &schema).unwrap_err();
        let d = &e.downcast_ref::<Diagnostics>().unwrap().0[0];
        assert_eq!(
            d.message,
            "`vpc_id` is `vpc` here: a reference is the resource"
        );
        assert_eq!(d.help.as_deref(), Some("write `vpc`"));
    }

    /// The nodes `src` builds to, as a stack's file.
    fn nodes(src: &str) -> prog::Program {
        use crate::syntax::resolve::{Mode, Unit, lower_stack};
        let file = crate::diag::add_source("readings.df", src);
        let parse = crate::syntax::parser::parse(src);
        assert!(parse.errors.is_empty(), "{:?}", parse.errors);
        let units = [Unit {
            file,
            root: parse.syntax(),
            path: None,
            project: false,
        }];
        let program = lower_stack(&units, &[0], true, Mode::Program, None, &[]).unwrap();
        prog::Program::clone(program.nodes.as_ref().unwrap())
    }

    /// A component's literals are read by node, its body as written
    /// (R-211): the attribute's own (`cidr = "10.0.0.0/8"`), one a let
    /// gives the attribute, and the let's value the attribute reads at run
    /// time (`__as(C, "inet")`), each a reading `lower` writes in its
    /// place; the program lowered without them is unread.
    #[test]
    fn a_components_literals_are_read_by_node_its_body_as_written() {
        let schema = Schema::parse("type_attr(\"t.r\", \"cidr\", \"inet\", [])\n", "test")
            .and_then(Schema::imported)
            .unwrap();
        let mut nodes = nodes(
            "component lan {\n  let c = \"10.1.0.0/16\"\n  \
             resource t.r a { cidr = \"10.0.0.0/8\" }\n  \
             resource t.r b { cidr = c }\n}\nresource lan n {}\n",
        );
        let (readings, diags) = readings(&nodes, &schema);
        assert!(diags.is_empty(), "{diags:?}");
        let read: Vec<String> = readings.values().map(|t| format!("{t:?}")).collect();
        let networks = read.iter().filter(|t| t.contains("IpNet")).count();
        let at_run_time = read.iter().filter(|t| t.contains("__as")).count();
        assert_eq!((networks, at_run_time, read.len()), (2, 1, 3), "{read:?}");
        let body = |p: &Program| {
            p.statements
                .iter()
                .find_map(|s| match s {
                    Stmt::Module(m) => Some(format!("{:?}", m.body)),
                    _ => None,
                })
                .expect("the component")
        };
        let unread = body(&crate::program::lower(&nodes).into_result().unwrap());
        assert!(unread.contains("Str(\"10.0.0.0/8\")") && !unread.contains("IpNet"));
        nodes.readings = readings;
        let read = body(&crate::program::lower(&nodes).into_result().unwrap());
        assert!(!read.contains("Str(\"10.0.0.0/8\")") && !read.contains("Str(\"10.1.0.0/16\")"));
    }

    #[test]
    fn dotted_joins_a_scope_and_a_name_either_empty() {
        assert_eq!(dotted("net", "vpc"), "net.vpc");
        assert_eq!(dotted("", "vpc"), "vpc");
        assert_eq!(dotted("net", ""), "net");
    }
}
