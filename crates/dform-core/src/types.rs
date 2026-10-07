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

use crate::ast::{Lit, Program, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::ir::Address;
use crate::quantity::{self, Dim};
use crate::schema::Schema;
use crate::value::Value;
use anyhow::Result;

/// The internal function a quantity literal with no reading of its own
/// lowers to (`500m`, `2.5m`): the position's type reads it.
pub const AMBIGUOUS: &str = "__quantity";

/// A schema attribute's type, as `type_attr` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    /// `ref(T)`: a reference to a `T`.
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
    /// `bool`, `inet`, `ip`, `url`, `oci`, `regex` (a pattern, not a schema type:
    /// a function parameter only), the quantities `bytes`, `cpu`,
    /// `duration`, and `time`.
    Scalar(String),
    /// Anything the check does not judge (an untyped `map`, `object`,
    /// `any`, an untyped `list`).
    Any,
}

impl Ty {
    /// Read a type's text: `ref(net.vpc)`, `list(ref(net.subnet))`,
    /// `enum("a", "b")`, `inet`, `bytes(gib)` (a quantity and how the
    /// provider takes it, `schema::Render`).
    pub fn parse(s: &str) -> Ty {
        let s = s.trim();
        let Some((head, rest)) = s.split_once('(') else {
            return match s {
                "string" | "int" | "float" | "number" | "bool" | "inet" | "ip" | "iprange"
                | "bytes" | "cpu" | "duration" | "time" | "url" | "oci" | "semver" | "regex" => {
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
        Term::Func { name, args } if name == "ref" && args.len() == 3 => match (&args[0], &args[2])
        {
            (Term::Val(Value::Str(typ)), Term::Val(Value::Str(p))) if p.is_empty() => {
                Some((typ, &args[1]))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `ref(r)` written out: `ref(ref(T, A, ""))`.
fn explicit(t: &Term) -> Option<&Term> {
    match t {
        Term::Func { name, args } if name == "ref" && args.len() == 1 => Some(&args[0]),
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
        Value::Url(u) => format!("the url {u}"),
        Value::Oci(r) => format!("the image reference {r}"),
        Value::Semver(v) => format!("the version {v}"),
        v => crate::partition::fmt_value(v),
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

pub fn ambiguous_ref(name: &str, span: crate::ast::Span, candidates: Vec<Term>) -> Term {
    let Term::Func { name, mut args } = spanned(AMBIGUOUS_REF, name, span) else {
        unreachable!("spanned is a call")
    };
    args.extend(candidates);
    Term::Func { name, args }
}

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
            Ty::Ref(want) if want != typ => {
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
                // A url's text is read as one (`read_as`); a regex pattern
                // stays a string, its text checked.
                ("url", Value::Url(_)) => true,
                ("url", Value::Str(x)) => url::Url::parse(x).is_ok(),
                ("oci", Value::Oci(_)) => true,
                ("oci", Value::Str(x)) => crate::value::OciRef::parse(x).is_ok(),
                ("iprange" | "semver", v) => crate::value::read_typed(s, v).is_ok(),
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
                ("url", Value::Str(x)) => format!(
                    "is a url: {x:?} is not one ({})",
                    url::Url::parse(x).unwrap_err()
                ),
                ("oci", Value::Str(x)) => {
                    format!("is an oci: {}", crate::value::parse_oci(x).unwrap_err())
                }
                ("iprange" | "semver", Value::Str(_)) => format!(
                    "is {}: {}",
                    if s == "iprange" {
                        "an iprange"
                    } else {
                        "a semver"
                    },
                    crate::value::read_typed(s, v).unwrap_err()
                ),
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

/// `a.b`, or `a` when `b` is empty.
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
    if !crate::value::VALUE_TYPES.contains(&s.as_str()) {
        return t;
    }
    match &t {
        Term::Func { name, .. }
            if matches!(
                name.as_str(),
                AS | AMBIGUOUS | "ref" | "cloud_ref" | "scoped"
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
        // A string in a url position is a url, parsed at compile time
        // into its canonical text (the constructor's value).
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "url" => {
            Term::Val(crate::value::parse_url(&x).unwrap_or(Value::Str(x)))
        }
        (Ty::Scalar(s), Term::Val(Value::Str(x))) if s == "oci" => {
            Term::Val(crate::value::parse_oci(&x).unwrap_or(Value::Str(x)))
        }
        (Ty::Scalar(s), Term::Val(v @ Value::Str(_))) if s == "iprange" || s == "semver" => {
            Term::Val(crate::value::read_typed(s, &v).unwrap_or(v))
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

/// Read every literal in a quantity's or a time's position as its type
/// (R-66, R-62): an attribute the schema types `bytes`, `cpu`, `duration`
/// or `time`, at its own path or nested in an object or list value
/// (`containers.resources.limits.memory`). Then a quantity literal that no
/// position read (`500m` where nothing gives a type) is an error naming
/// both readings. Runs on the program before it is evaluated, once the
/// providers' schemas are known.
pub fn read(program: &mut Program, schema: &Schema) -> Result<()> {
    let mut diags = Vec::new();
    for s in &mut program.statements {
        read_stmt(s, schema, &mut diags);
    }
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
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

fn read_stmt(s: &mut Stmt, schema: &Schema, diags: &mut Vec<Diagnostic>) {
    let at = |typ: &str, name: &Term, path: &str| match name {
        Term::Val(Value::Str(a)) => Address {
            typ: typ.to_string(),
            name: a.clone(),
        }
        .attr(path),
        _ => format!("{typ}.{path}"),
    };
    match s {
        Stmt::Resource(r) => {
            let Term::Val(Value::Str(typ)) = &r.typ else {
                return;
            };
            for f in &mut r.fields {
                if let Err((path, why)) = read_at(schema, typ, &f.key, &mut f.value) {
                    diags.push(Diagnostic::error(
                        f.span,
                        format!("{} {why}", at(typ, &r.name, &path)),
                    ));
                }
            }
        }
        Stmt::Module(m) => {
            for s in &mut m.body {
                read_stmt(s, schema, diags);
            }
        }
        // `arg(T, A, P, V, R)`, or `arg(T, A, P, V)` of a `set` with no
        // rank until the transform gives it one.
        Stmt::Fact(head) | Stmt::Rule(crate::ast::RuleStmt { head, .. })
            if head.pred == "arg" && matches!(head.args.len(), 4 | 5) =>
        {
            let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) =
                (&head.args[0], &head.args[2])
            else {
                return;
            };
            let (typ, path, name) = (typ.clone(), path.clone(), head.args[1].clone());
            if let Err((path, why)) = read_at(schema, &typ, &path, &mut head.args[3]) {
                diags.push(Diagnostic::error(
                    head.span,
                    format!("{} {why}", at(&typ, &name, &path)),
                ));
            }
        }
        _ => {}
    }
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
        // A computed value where a value type is wanted is read as one
        // at run time (R-134); a literal is `check`'s.
        if matches!(t, Term::Var(_) | Term::Func { .. }) {
            let v = std::mem::replace(t, Term::Wildcard);
            *t = at_run_time(&ty, v);
            return Ok(());
        }
    }
    match t {
        Term::Obj(m) => {
            for (k, v) in m {
                read_at(schema, typ, &format!("{path}.{k}"), v)?;
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
        None => crate::partition::fmt_term(t),
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
}
