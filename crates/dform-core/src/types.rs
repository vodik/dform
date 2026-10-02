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
//! A term the compiler cannot see the value of (a variable, a call) is
//! checked when it has one, at evaluation.

use crate::ast::{Program, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::ir::Address;
use crate::schema::Schema;
use crate::value::Value;
use anyhow::Result;

/// A schema attribute's type, as `type_attr` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    /// `ref(T)`: a reference to a `T`.
    Ref(String),
    /// `list(T)`, `set(T)`.
    List(Box<Ty>),
    /// `secret(T)`.
    Secret(Box<Ty>),
    /// `enum(a, b, ..)`.
    Enum(Vec<String>),
    /// `string`, `int`, `bool`, `inet`.
    Scalar(String),
    /// Anything the check does not judge (`map`, `object`, `any`, an
    /// untyped `list`).
    Any,
}

impl Ty {
    /// Read a type's text: `ref(net.vpc)`, `list(ref(net.subnet))`,
    /// `enum("a", "b")`, `inet`.
    pub fn parse(s: &str) -> Ty {
        let s = s.trim();
        let Some((head, rest)) = s.split_once('(') else {
            return match s {
                "string" | "int" | "bool" | "inet" | "ip" => Ty::Scalar(s.to_string()),
                _ => Ty::Any,
            };
        };
        let Some(inner) = rest.strip_suffix(')') else {
            return Ty::Any;
        };
        match head.trim() {
            "ref" => Ty::Ref(inner.trim().trim_matches('"').to_string()),
            "list" | "set" => Ty::List(Box::new(Ty::parse(inner))),
            "secret" => Ty::Secret(Box::new(Ty::parse(inner))),
            "enum" => Ty::Enum(
                inner
                    .split(',')
                    .map(|m| m.trim().trim_matches('"').to_string())
                    .collect(),
            ),
            _ => Ty::Any,
        }
    }
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Ref(t) => write!(f, "ref({t})"),
            Ty::List(t) => write!(f, "list({t})"),
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
        Value::Bool(b) => format!("the bool {b}"),
        v => crate::partition::fmt_value(v),
    }
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
        (Ty::Ref(want), Term::Val(v)) => Some(format!(
            "takes a ref({want}): a resource, by its name, `{want}[\"a\"]` or a variable, not {}",
            shown_literal(v)
        )),
        (Ty::Enum(ms), Term::Val(Value::Str(s))) if !ms.contains(s) => {
            Some(format!("is {ty}: {s:?} is not one of its members"))
        }
        (Ty::Enum(_), Term::Val(Value::Str(_))) => None,
        (Ty::Scalar(s), Term::Val(v)) => {
            let fits = match (s.as_str(), v) {
                ("string", Value::Str(_)) => true,
                ("int", Value::Int(_)) => true,
                ("bool", Value::Bool(_)) => true,
                ("inet", Value::IpNet { .. }) => true,
                ("inet", Value::Str(x)) => crate::value::parse_ipnet(x).is_some(),
                ("ip", Value::Ip(_)) => true,
                ("ip", Value::Str(x)) => crate::value::ipv4_to_u32(x).is_some(),
                // A null is not known yet; a computed value fits its type.
                (_, Value::Null { .. }) => true,
                _ => false,
            };
            (!fits).then(|| match (s.as_str(), v) {
                ("inet", Value::Str(x)) => {
                    format!("is an inet: {x:?} is not a network (`a.b.c.d/n`)")
                }
                ("ip", Value::Str(x)) => format!("is an ip: {x:?} is not an address (`a.b.c.d`)"),
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

/// A literal where `ty` is expected (R-31): read as that type (a string
/// is an `inet` or an `ip` where one is expected), or why it cannot be.
/// What is not a literal is left as it is.
pub fn literal(ty: &Ty, t: Term) -> Result<Term, String> {
    if let Some(why) = mismatch(ty, &t) {
        return Err(why);
    }
    Ok(read(ty, t))
}

fn read(ty: &Ty, t: Term) -> Term {
    match (ty, t) {
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
        (Ty::Secret(inner), t) => read(inner, t),
        (Ty::List(inner), Term::List(xs)) => {
            Term::List(xs.into_iter().map(|x| read(inner, x)).collect())
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
        assert_eq!(Ty::parse("list"), Ty::Any);
        assert_eq!(
            Ty::parse("enum(\"a\", \"b\")"),
            Ty::Enum(vec!["a".into(), "b".into()])
        );
    }
}
