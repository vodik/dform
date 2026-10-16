//! A value's shape: the steps into it, and what gives a value its type at
//! each of them (an attribute's schema, a declared type, a parameter).

use super::*;

/// One step into a value: an object's field, a list's element, or an
/// object's entry at any key (`(k, v) in o`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Seg {
    Key(String),
    Elem,
    Any,
}

impl Seg {
    /// Whether this step reaches what `other` does: the same, or an entry
    /// at any key beside a field.
    fn meets(&self, other: &Seg) -> bool {
        match (self, other) {
            (Seg::Any, Seg::Key(_) | Seg::Any) | (Seg::Key(_), Seg::Any) => true,
            (a, b) => a == b,
        }
    }
}

/// `q` past `path`, when `path` leads to it.
pub(super) fn past<'q>(q: &'q [Seg], path: &[Seg]) -> Option<&'q [Seg]> {
    (q.len() >= path.len() && q.iter().zip(path).all(|(a, b)| a.meets(b))).then(|| &q[path.len()..])
}

pub(super) type Path = Vec<Seg>;

/// A path as a program writes it: `requests.cpu`, `[]` for an element.
pub(super) fn shown_path(p: &[Seg]) -> String {
    let mut out = String::new();
    for s in p {
        match s {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Elem => out.push_str("[]"),
            Seg::Any => out.push_str("[k]"),
        }
    }
    out
}

/// The keys of a stored path (`spec.template.spec`).
pub(super) fn keys(path: &str) -> Path {
    crate::address::path_keys(path)
        .into_iter()
        .map(Seg::Key)
        .collect()
}

/// What gives a value its type.
#[derive(Debug, Clone)]
pub(super) enum Shape {
    /// A resource type's attribute at a path: the schema's type of each
    /// path under it.
    Attr(String, Path),
    /// A declared type (a let's, an input's, an output's, a column's).
    Declared(TypeExpr),
    /// A function's parameter.
    Ty(Ty),
    /// The other side of `+` or `-`, or its result (`base + 50m`): its
    /// type, a duration beside a time.
    Like(usize),
}

impl Shape {
    /// The type this gives the value's part at `q`, if it gives one.
    pub(super) fn at(&self, schema: &Schema, q: &[Seg]) -> Option<Ty> {
        let ty = match self {
            Shape::Attr(typ, base) => {
                let full: Vec<&Seg> = base.iter().chain(q).collect();
                if full.contains(&&Seg::Any) {
                    return None;
                }
                let path: Vec<&str> = full
                    .iter()
                    .filter_map(|s| match s {
                        Seg::Key(k) => Some(k.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut ty = Ty::parse(&schema.attr(typ, &path.join("."))?.ty);
                // The schema types a list's elements at the list's path:
                // an element past the last key is the list's element.
                let elems = full.iter().rev().take_while(|s| ***s == Seg::Elem).count();
                for _ in 0..elems {
                    ty = match ty {
                        Ty::List(inner) => *inner,
                        ty => ty,
                    };
                }
                ty
            }
            Shape::Declared(t) => declared_at(t, q)?,
            Shape::Ty(t) => ty_at(t, q)?,
            Shape::Like(_) => return None,
        };
        let ty = match ty {
            Ty::Secret(inner) => *inner,
            ty => ty,
        };
        readable(&ty).then_some(ty)
    }
}

/// The type at `q` inside the declared type `t`.
fn declared_at(t: &TypeExpr, q: &[Seg]) -> Option<Ty> {
    let Some((first, rest)) = q.split_first() else {
        return Some(types::of_expr(t));
    };
    match (t, first) {
        (TypeExpr::Apply(n, args), _) if n == "secret" => declared_at(args.first()?, q),
        (TypeExpr::Object(fs), Seg::Key(k)) => {
            declared_at(&fs.iter().find(|(f, _)| f == k)?.1, rest)
        }
        (TypeExpr::Apply(n, args), Seg::Key(_) | Seg::Any) if n == "map" => {
            declared_at(args.first()?, rest)
        }
        (TypeExpr::Apply(n, args), Seg::Elem) if n == "list" || n == "set" => {
            declared_at(args.first()?, rest)
        }
        _ => None,
    }
}

/// The type at `q` inside `t`.
fn ty_at(t: &Ty, q: &[Seg]) -> Option<Ty> {
    let Some((first, rest)) = q.split_first() else {
        return Some(t.clone());
    };
    match (t, first) {
        (Ty::Secret(inner), _) => ty_at(inner, q),
        (Ty::List(inner), Seg::Elem) | (Ty::Map(inner), Seg::Key(_) | Seg::Any) => {
            ty_at(inner, rest)
        }
        _ => None,
    }
}

/// A type a literal is read as: a quantity, a time, a value type read
/// from a string (R-31, R-66, R-134).
pub(super) fn readable(ty: &Ty) -> bool {
    match ty {
        Ty::Scalar(s) => types::measured(s) || crate::value::is_value_type(s),
        _ => false,
    }
}

/// An edge: what gives the type, and where.
#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub(super) shape: Shape,
    pub(super) what: String,
    pub(super) span: Span,
}

impl Edge {
    /// The edge as it types the value's part at `q`: an attribute's
    /// named by its whole path.
    pub(super) fn at(&self, q: &[Seg]) -> Edge {
        let mut e = self.clone();
        if let Shape::Attr(..) = e.shape {
            for s in q {
                if let Seg::Key(k) = s {
                    e.what = format!("{}.{k}", e.what);
                }
            }
        }
        e
    }
}
