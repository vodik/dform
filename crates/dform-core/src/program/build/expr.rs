//! A term as nodes (R-211 step 3), built from what the resolver lowered
//! it to: every form an `ast::Term` takes has its node, the internal
//! functions the resolver writes for what has no other spelling included.
//!
//! | lowered                                  | node                              |
//! |------------------------------------------|-----------------------------------|
//! | `Val(v)`                                 | `Lit(v)`                          |
//! | `__quantity("500m", file, start, end)`   | `Quantity`, at its span           |
//! | `Var(X)`, `_`                            | `Var`, `Hole`                     |
//! | `add(a, b)` .. `mod(a, b)`, `sub(0, a)`  | `Binary`, `Neg`                   |
//! | `str.format("a%sb", x)`                  | `Interp`                          |
//! | `{..}`, `__object(..)`, `__merge(..)`    | `Object` (`spread.rs`)            |
//! | `[..]`, `__concat(..)`                   | `List` (`spread.rs`)              |
//! | `__range(lo, hi, incl)`                  | `Range`                           |
//! | `__as(v, "T")`                           | `As`                              |
//! | `__path(v, "a.b")`, `__len(v)`           | `Field` of `Field` and `Len` steps |
//! | `__ref("T", a, "p")`                     | `RefOf` a `Resource` (or its `Field`) |
//! | `__scope(n)`, `__lexical("B", "n")`      | `Scoped`, `Absolute`, `Lexical("B")` |
//! | `[item \| body]`                         | `Comprehension` (`clause.rs`)     |
//! | any other `f(..)`                        | `Call` of the function, or of a name read as data |
//!
//! The reads a term hoisted are its nodes in term position
//! ([`Builder::hoisted`], `hoist.rs`). The resolver builds a term written
//! in the program as its node (R-211 step 7), the forms over its
//! arguments' nodes ([`Builder::form`]); a term is built from what it
//! lowered to where a statement's builder reads a lowered literal, and
//! for the terms the resolver makes of what it resolved.

use super::Builder;
use crate::ast::{Lit, Term, TypeExpr};
use crate::program::node::*;
use crate::program::scope::Reach;
use crate::value::Value;

impl Builder<'_> {
    /// The term `t` the resolver lowered, after the reads it hoisted,
    /// `reads`: each the node of the first use of its variable in `t`.
    pub fn hoisted(&mut self, t: &Term, reads: &[Lit]) -> ExprId {
        let (e, left) = self.reading(reads, |b| b.expr(t));
        assert!(
            left.is_empty(),
            "a term uses every read it hoisted: {t:?} after {left:?}"
        );
        e
    }

    /// The lowered term `t`.
    pub fn expr(&mut self, t: &Term) -> ExprId {
        let kind = match t {
            Term::Val(v) => ExprKind::Lit(v.clone()),
            Term::Var(x) => match self.pending_read(x) {
                Some(read) => return read,
                None => ExprKind::Var(self.var(x)),
            },
            Term::Wildcard => ExprKind::Hole,
            Term::List(xs) => return self.list_of(xs),
            Term::Obj(m) => return self.object_of(m),
            Term::ListComp { item, body } => return self.comprehension(item, body),
            Term::Func { name, args } => return self.func(name, args),
        };
        self.expr_node(kind)
    }

    /// `[item | body]` as lowered: its clause the body's literals, its
    /// reads its own.
    pub fn comprehension(&mut self, item: &Term, body: &[Lit]) -> ExprId {
        let kind = self.sealed(|b| {
            let clause = b.clause(body);
            let item = b.expr(item);
            ExprKind::Comprehension { item, clause }
        });
        self.expr_node(kind)
    }

    /// `name(args)`: its arguments' nodes, the form of the language it
    /// lowers ([`Builder::form`]).
    fn func(&mut self, name: &str, args: &[Term]) -> ExprId {
        let args = args.iter().map(|a| self.expr(a)).collect();
        self.form(name, args)
    }

    /// `name(args)` of the nodes `args`: the form of the language the
    /// lowering writes it as (an operator, an interpolation, a spread, a
    /// range, a typing, a path, a reference), else a call of the function
    /// the standard library or the lowering declares, or of a name read as
    /// data.
    pub fn form(&mut self, name: &str, args: Vec<ExprId>) -> ExprId {
        use crate::address::{FORMAT, LEN, REF};
        use crate::functions::{CONCAT, MERGE, OBJECT};
        let kind = match (name, args.as_slice()) {
            ("sub", [zero, a]) if self.constant(*zero) == Some(&Value::Int(0)) => ExprKind::Neg(*a),
            (_, [a, b]) if let Ok(op) = BinOp::try_from(name) => ExprKind::Binary {
                op,
                lhs: *a,
                rhs: *b,
            },
            (FORMAT, [f, holes @ ..])
                if let Some(Value::Str(f)) = self.constant(*f)
                    && f.matches("%s").count() == holes.len() =>
            {
                interp(&f.clone(), holes)
            }
            (MERGE, parts) => return self.merge(parts),
            (CONCAT, parts) => return self.concat(parts),
            (OBJECT, pairs) if pairs.len() % 2 == 0 => return self.computed(pairs),
            (crate::range::LOWERED, [lo, hi, incl])
                if let Some(Value::Bool(inclusive)) = self.constant(*incl) =>
            {
                ExprKind::Range {
                    lo: *lo,
                    hi: *hi,
                    inclusive: *inclusive,
                }
            }
            (crate::types::AS, [v, ty]) if let Some(Value::Str(ty)) = self.constant(*ty) => {
                ExprKind::As {
                    value: *v,
                    ty: TypeExpr::Name(ty.clone()),
                }
            }
            (crate::types::AMBIGUOUS, [text, file, start, end])
                if let Some(Value::Str(text)) = self.constant(*text)
                    && let (Some(file), Some(start), Some(end)) =
                        (self.index(*file), self.index(*start), self.index(*end)) =>
            {
                let span = crate::ast::Span {
                    file,
                    start,
                    end,
                    origin: 0,
                };
                let text = text.clone();
                return self.at(span, |b| b.expr_node(ExprKind::Quantity { text }));
            }
            (crate::modules::ABSOLUTE, [name]) => ExprKind::Scoped {
                name: *name,
                via: Reach::Absolute,
            },
            (crate::modules::LEXICAL, [body, seg])
                if let (Some(Value::Str(body)), Some(Value::Str(_))) =
                    (self.constant(*body), self.constant(*seg)) =>
            {
                ExprKind::Scoped {
                    name: *seg,
                    via: Reach::Lexical(body.clone()),
                }
            }
            (LEN, [v]) => ExprKind::Field {
                base: *v,
                path: vec![Step::Len],
            },
            ("__path", [v, p]) if let Some(Value::Str(p)) = self.constant(*p) => ExprKind::Field {
                base: *v,
                path: self.fields(&p.clone()),
            },
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

    /// The constant `u32` node `id` is: a quantity's place.
    fn index(&self, id: ExprId) -> Option<u32> {
        match self.constant(id) {
            Some(Value::Int(i)) => u32::try_from(*i).ok(),
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
    use crate::ast::{Span, str_term, var};
    use crate::program::Program;

    fn f(name: &str, args: Vec<Term>) -> Term {
        Term::Func {
            name: name.into(),
            args,
        }
    }

    /// Each internal function the resolver writes is built as the node of
    /// the form it lowers.
    #[test]
    fn a_lowered_term_is_the_node_of_its_form() {
        let mut p = Program::new();
        let written = |v: &str| v == "X";
        let mut b = Builder::new(&mut p, Span::default(), &written);
        let int = |i| Term::Val(Value::Int(i));
        let cases = [
            (f("add", vec![var("X"), int(1)]), "Binary"),
            (f("sub", vec![int(0), var("X")]), "Neg"),
            (f("str.format", vec![str_term("a%sb"), var("X")]), "Interp"),
            (
                f("__merge", vec![var("X"), Term::Obj(Default::default())]),
                "Object",
            ),
            (
                f("__concat", vec![var("X"), Term::List(vec![int(1)])]),
                "List",
            ),
            (
                f(
                    "__object",
                    vec![f("str.format", vec![str_term("%s"), var("X")]), int(1)],
                ),
                "Object",
            ),
            (
                f(
                    "__range",
                    vec![int(0), var("X"), Term::Val(Value::Bool(true))],
                ),
                "Range",
            ),
            (f("__as", vec![var("X"), str_term("int")]), "As"),
            (f("__len", vec![var("X")]), "Field"),
            (
                f("__ref", vec![str_term("net.vpc"), var("X"), str_term("")]),
                "RefOf",
            ),
            (
                f("__quantity", vec![str_term("500m"), int(1), int(2), int(6)]),
                "Quantity",
            ),
            (f("inet.subnet", vec![var("X"), int(4), int(1)]), "Call"),
            (Term::Wildcard, "Hole"),
        ];
        for (t, kind) in cases {
            let id = b.expr(&t);
            let got = format!("{:?}", b.program.exprs[id].kind);
            assert!(got.starts_with(kind), "{t:?}: {got}");
        }
        let quoted = f("__path", vec![var("X"), str_term("a.\"b.c\"")]);
        let id = b.expr(&quoted);
        let ExprKind::Field { path, .. } = &b.program.exprs[id].kind else {
            panic!("not a field");
        };
        assert_eq!(path.len(), 2, "a quoted segment is one step");
    }
}
