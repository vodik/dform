//! A term's form as its node (R-211 step 3): what a call of a function
//! of the language or the lowering is, over its arguments' nodes
//! ([`Builder::form`]), and a term with the reads it holds made already
//! ([`Builder::settled`]).
//!
//! | call                                     | node                              |
//! |------------------------------------------|-----------------------------------|
//! | `__quantity("500m", file, start, end)`   | `Quantity`, at its span           |
//! | `add(a, b)` .. `mod(a, b)`, `sub(0, a)`  | `Binary`, `Neg`                   |
//! | `str.format("a%sb", x)`                  | `Interp`                          |
//! | `__object(..)`, `__merge(..)`            | `Object` (`spread.rs`)            |
//! | `__concat(..)`                           | `List` (`spread.rs`)              |
//! | `__range(lo, hi, incl)`                  | `Range`                           |
//! | `__as(v, "T")`                           | `As`                              |
//! | `__path(v, "a.b")`, `__len(v)`           | `Field` of `Field` and `Len` steps |
//! | `__ref("T", a, "p")`                     | `RefOf` a `Resource` (or its `Field`) |
//! | any other `f(..)`                        | `Call` of the function, or of a name read as data |

use super::Builder;
use crate::program::node::*;
use crate::value::Value;

impl Builder<'_> {
    /// `name(args)` of the nodes `args`: the form of the language the
    /// resolver writes it as (an interpolation, a reference), else a call
    /// of the function the standard library or the lowering declares, or
    /// of a name read as data. The resolver builds every other form's node
    /// itself (an operator, a spread, a range, a typing, a field).
    pub fn form(&mut self, name: &str, args: Vec<ExprId>) -> ExprId {
        use crate::address::{FORMAT, REF};
        let kind = match (name, args.as_slice()) {
            (FORMAT, [f, holes @ ..])
                if let Some(Value::Str(f)) = self.constant(*f)
                    && f.matches("%s").count() == holes.len() =>
            {
                interp(&f.clone(), holes)
            }
            (REF, [typ, addr, p])
                if let (Some(Value::Str(typ)), Some(Value::Str(p))) =
                    (self.constant(*typ), self.constant(*p)) =>
            {
                let typ = TypeRef {
                    name: typ.clone(),
                    span: self.span,
                };
                let path = (!p.is_empty()).then(|| self.fields(&p.clone()));
                let mut res = self.expr_node(ExprKind::Resource { typ, addr: *addr });
                if let Some(path) = path {
                    res = self.expr_node(ExprKind::Field { base: res, path });
                }
                ExprKind::RefOf(res)
            }
            _ => {
                let callee = match crate::functions::get(name) {
                    Some(f) => Callee::Std(f),
                    None => Callee::Data(name.to_string()),
                };
                let args = args.into_iter().map(|value| Arg { name: None, value });
                ExprKind::Call {
                    callee,
                    args: args.collect(),
                }
            }
        };
        self.expr_node(kind)
    }

    /// A copy of the node `id` whose reads are made already, before what
    /// holds it: each read in it is its variable.
    pub fn settled(&mut self, id: ExprId) -> ExprId {
        let e = &self.program.exprs[id];
        match e.hoisted {
            Some(v) => self.at(e.span, |b| b.expr_node(ExprKind::Var(v))),
            None => self.copy_settled(id),
        }
    }

    /// The reads [`Self::settled`] takes out of the node `id`, each bound
    /// to its variable: `x = read`, in the order they stand.
    pub fn settled_reads(&mut self, id: ExprId) -> Vec<GoalId> {
        let e = &self.program.exprs[id];
        let span = e.span;
        let children = match (&e.kind, e.hoisted) {
            (_, Some(v)) => {
                return self.at(span, |b| {
                    let value = b.copy_settled(id);
                    let pat = b.pattern_node(PatternKind::Bind(v));
                    vec![b.bind_goal(span, pat, value)]
                });
            }
            (ExprKind::Comprehension { .. } | ExprKind::Deployed { .. }, None) => {
                return Vec::new();
            }
            (kind, None) => kind.children(),
        };
        children
            .into_iter()
            .flat_map(|c| self.settled_reads(c))
            .collect()
    }

    /// A copy of the node `id`, a read in place when it is one, whose
    /// parts' reads are made already ([`Self::settled`]).
    pub fn copy_settled(&mut self, id: ExprId) -> ExprId {
        let e = self.program.exprs[id].clone();
        let s = |b: &mut Self, x: ExprId| b.settled(x);
        let kind = match e.kind {
            ExprKind::Resource { typ, addr } => ExprKind::Resource {
                typ,
                addr: s(self, addr),
            },
            ExprKind::RefOf(x) => ExprKind::RefOf(s(self, x)),
            ExprKind::Scoped { name, via } => ExprKind::Scoped {
                name: s(self, name),
                via,
            },
            ExprKind::Alias(x) => ExprKind::Alias(s(self, x)),
            ExprKind::Field { base, path } => ExprKind::Field {
                base: s(self, base),
                path: self.settled_path(path),
            },
            ExprKind::Lookup {
                rel,
                args,
                out,
                path,
            } => ExprKind::Lookup {
                rel,
                args: args.into_iter().map(|a| s(self, a)).collect(),
                out,
                path: self.settled_path(path),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee,
                args: args
                    .into_iter()
                    .map(|a| Arg {
                        name: a.name,
                        value: s(self, a.value),
                    })
                    .collect(),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op,
                lhs: s(self, lhs),
                rhs: s(self, rhs),
            },
            ExprKind::Neg(x) => ExprKind::Neg(s(self, x)),
            ExprKind::Interp { parts } => ExprKind::Interp {
                parts: parts
                    .into_iter()
                    .map(|p| match p {
                        Piece::Hole(x) => Piece::Hole(s(self, x)),
                        t => t,
                    })
                    .collect(),
            },
            ExprKind::List { parts } => ExprKind::List {
                parts: parts
                    .into_iter()
                    .map(|p| match p {
                        ListPart::Elem(x) => ListPart::Elem(s(self, x)),
                        ListPart::Spread(x) => ListPart::Spread(s(self, x)),
                    })
                    .collect(),
            },
            ExprKind::Object { parts } => ExprKind::Object {
                parts: parts
                    .into_iter()
                    .map(|p| match p {
                        ObjPart::Field {
                            key,
                            key_span,
                            value,
                            pun,
                        } => ObjPart::Field {
                            key,
                            key_span,
                            value: s(self, value),
                            pun,
                        },
                        ObjPart::Computed { key, value } => ObjPart::Computed {
                            key: s(self, key),
                            value: s(self, value),
                        },
                        ObjPart::Spread(x) => ObjPart::Spread(s(self, x)),
                    })
                    .collect(),
            },
            ExprKind::Range { lo, hi, inclusive } => ExprKind::Range {
                lo: s(self, lo),
                hi: s(self, hi),
                inclusive,
            },
            ExprKind::As { value, ty } => ExprKind::As {
                value: s(self, value),
                ty,
            },
            ExprKind::Address { of, typ } => ExprKind::Address {
                of: self.copy_settled(of),
                typ,
            },
            ExprKind::Output { copy, key, of } => ExprKind::Output {
                copy: s(self, copy),
                key,
                of: of.map(|(c, scope)| (c, s(self, scope))),
            },
            ExprKind::World { typ, addr, path } => ExprKind::World {
                typ,
                addr: s(self, addr),
                path: self.settled_path(path),
            },
            ExprKind::Aggregate { kind, item } => ExprKind::Aggregate {
                kind,
                item: s(self, item),
            },
            kind @ (ExprKind::Missing
            | ExprKind::Hole
            | ExprKind::Lit(_)
            | ExprKind::Quantity { .. }
            | ExprKind::Var(_)
            | ExprKind::Value { .. }
            | ExprKind::Type(_)
            | ExprKind::Setting { .. }) => kind,
            // A clause of its own; read by its keys.
            ExprKind::Comprehension { .. } | ExprKind::Deployed { .. } => return id,
        };
        self.at(e.span, |b| b.expr_node(kind))
    }

    /// The steps `path` with the reads in them made already.
    fn settled_path(&mut self, path: Vec<Step>) -> Vec<Step> {
        path.into_iter()
            .map(|st| match st {
                Step::Index(x) => Step::Index(self.settled(x)),
                Step::Key(x) => Step::Key(self.settled(x)),
                Step::Keyed {
                    key,
                    typ,
                    list,
                    keys,
                    field,
                } => Step::Keyed {
                    key: self.settled(key),
                    typ: self.settled(typ),
                    list,
                    keys,
                    field,
                },
                st => st,
            })
            .collect()
    }

    /// The constant node `id` is, when it is one.
    pub(super) fn constant(&self, id: ExprId) -> Option<&Value> {
        let e = &self.program.exprs[id];
        match (&e.kind, e.hoisted) {
            (ExprKind::Lit(v), None) => Some(v),
            _ => None,
        }
    }

    /// A stored path, `a."b.c".d`, as a field step per segment, each as
    /// the path stores it (quoted, with its index suffix).
    pub(super) fn fields(&self, p: &str) -> Vec<Step> {
        crate::address::path_segments(p)
            .into_iter()
            .map(|k| Step::Field(k.to_string(), self.span))
            .collect()
    }
}

/// `str.format(f, holes..)` as the string it was written as: `f`'s text
/// between its `%s`, each the next hole.
fn interp(f: &str, holes: &[ExprId]) -> ExprKind {
    let mut parts = Vec::new();
    let mut holes = holes.iter();
    for (i, text) in f.split("%s").enumerate() {
        if i > 0 {
            let hole = holes.next().expect("one hole per %s");
            parts.push(Piece::Hole(*hole));
        }
        if !text.is_empty() {
            parts.push(Piece::Text(text.to_string()));
        }
    }
    ExprKind::Interp { parts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::program::Program;

    /// Each function of the language or the lowering a call names is
    /// built as the node of its form.
    #[test]
    fn a_call_is_the_node_of_its_form() {
        let mut p = Program::new();
        let written = |v: &str| v == "X";
        let mut b = Builder::new(&mut p, Span::default(), &written);
        let x = b.variable("X");
        let int = |b: &mut Builder, i| b.lit(Value::Int(i));
        let s = |b: &mut Builder, t: &str| b.lit(Value::Str(t.into()));
        let one = int(&mut b, 1);
        let fmt = s(&mut b, "a%sb");
        let typ = s(&mut b, "net.vpc");
        let empty = s(&mut b, "");
        let cases = [
            ("str.format", vec![fmt, x], "Interp"),
            ("__ref", vec![typ, x, empty], "RefOf"),
            ("inet.subnet", vec![x, one, one], "Call"),
        ];
        for (name, args, kind) in cases {
            let id = b.form(name, args);
            let got = format!("{:?}", b.program.exprs[id].kind);
            assert!(got.starts_with(kind), "{name}: {got}");
        }
        let quoted = s(&mut b, "a.\"b.c\"");
        let id = b.form("__ref", vec![typ, x, quoted]);
        let ExprKind::RefOf(res) = &b.program.exprs[id].kind else {
            panic!("not a reference");
        };
        let ExprKind::Field { path, .. } = &b.program.exprs[*res].kind else {
            panic!("not a field");
        };
        assert_eq!(path.len(), 2, "a quoted segment is one step");
    }
}
