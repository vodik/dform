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
//! | `[item \| body]`                         | `Comprehension` (`clause.rs`)     |
//! | any other `f(..)`                        | `Call` of the function, or of a name read as data |
//!
//! The reads a term hoisted are its nodes in term position
//! ([`Builder::hoisted`], `hoist.rs`).

use super::Builder;
use crate::ast::{Lit, Term, TypeExpr};
use crate::program::node::*;
use crate::value::Value;

impl Builder<'_> {
    /// The term `t` the resolver lowered, after the reads it hoisted,
    /// `reads`: each the node of the first use of its variable in `t`.
    pub fn hoisted(&mut self, t: &Term, reads: &[Lit]) -> ExprId {
        let (e, left) = self.part(t, reads);
        assert!(
            left.is_empty(),
            "a term uses every read it hoisted: {t:?} after {left:?}"
        );
        e
    }

    /// The term `t`, a part of a statement's term, after the reads
    /// `reads` hoisted while it was lowered, and those of them it does not
    /// use (a read of the term around it).
    pub fn part(&mut self, t: &Term, reads: &[Lit]) -> (ExprId, Vec<Lit>) {
        self.reading(reads, |b| b.expr(t))
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
            Term::ListComp { item, body } => self.sealed(|b| {
                let clause = b.clause(body);
                let item = b.expr(item);
                ExprKind::Comprehension { item, clause }
            }),
            Term::Func { name, args } => return self.func(name, args),
        };
        self.expr_node(kind)
    }

    /// `name(args)`: the form of the language it lowers, else a call.
    fn func(&mut self, name: &str, args: &[Term]) -> ExprId {
        use crate::address::{FORMAT, LEN, REF};
        use crate::functions::{CONCAT, MERGE, OBJECT};
        let kind = match (name, args) {
            ("sub", [Term::Val(Value::Int(0)), a]) => ExprKind::Neg(self.expr(a)),
            (_, [a, b]) if let Ok(op) = BinOp::try_from(name) => ExprKind::Binary {
                op,
                lhs: self.expr(a),
                rhs: self.expr(b),
            },
            (FORMAT, [Term::Val(Value::Str(f)), holes @ ..])
                if f.matches("%s").count() == holes.len() =>
            {
                self.interp(f, holes)
            }
            (MERGE, parts) => return self.merge(parts),
            (CONCAT, parts) => return self.concat(parts),
            (OBJECT, pairs) if pairs.len() % 2 == 0 => return self.computed(pairs),
            (crate::range::LOWERED, [lo, hi, Term::Val(Value::Bool(inclusive))]) => {
                ExprKind::Range {
                    lo: self.expr(lo),
                    hi: self.expr(hi),
                    inclusive: *inclusive,
                }
            }
            (crate::types::AS, [v, Term::Val(Value::Str(ty))]) => ExprKind::As {
                value: self.expr(v),
                ty: TypeExpr::Name(ty.clone()),
            },
            (crate::types::AMBIGUOUS, [Term::Val(Value::Str(text)), file, start, end]) => {
                let n = |t: &Term| match t {
                    Term::Val(Value::Int(i)) => u32::try_from(*i).ok(),
                    _ => None,
                };
                match (n(file), n(start), n(end)) {
                    (Some(file), Some(start), Some(end)) => {
                        let span = crate::ast::Span {
                            file,
                            start,
                            end,
                            origin: 0,
                        };
                        let text = text.clone();
                        return self.at(span, |b| b.expr_node(ExprKind::Quantity { text }));
                    }
                    _ => self.call(name, args),
                }
            }
            (LEN, [v]) => ExprKind::Field {
                base: self.expr(v),
                path: vec![Step::Len],
            },
            ("__path", [v, Term::Val(Value::Str(p))]) => ExprKind::Field {
                base: self.expr(v),
                path: self.fields(p),
            },
            (REF, [Term::Val(Value::Str(typ)), addr, Term::Val(Value::Str(p))]) => {
                let typ = TypeRef {
                    name: typ.clone(),
                    span: self.span,
                };
                let addr = self.expr(addr);
                let mut res = self.expr_node(ExprKind::Resource { typ, addr });
                if !p.is_empty() {
                    let path = self.fields(p);
                    res = self.expr_node(ExprKind::Field { base: res, path });
                }
                ExprKind::RefOf(res)
            }
            _ => self.call(name, args),
        };
        self.expr_node(kind)
    }

    /// `f(args)`: a function the standard library or the lowering
    /// declares, else a name read as data.
    fn call(&mut self, name: &str, args: &[Term]) -> ExprKind {
        let callee = match crate::functions::get(name) {
            Some(f) => Callee::Std(f),
            None => Callee::Data(name.to_string()),
        };
        let args = args
            .iter()
            .map(|a| Arg {
                name: None,
                value: self.expr(a),
            })
            .collect();
        ExprKind::Call { callee, args }
    }

    /// `str.format(f, holes..)` as the string it was written as: `f`'s
    /// text between its `%s`, each the next hole.
    fn interp(&mut self, f: &str, holes: &[Term]) -> ExprKind {
        let mut parts = Vec::new();
        let mut holes = holes.iter();
        for (i, text) in f.split("%s").enumerate() {
            if i > 0 {
                let hole = holes.next().expect("one hole per %s");
                parts.push(Piece::Hole(self.expr(hole)));
            }
            if !text.is_empty() {
                parts.push(Piece::Text(text.to_string()));
            }
        }
        ExprKind::Interp { parts }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Atom, Span, str_term, var};
    use crate::program::Program;

    fn f(name: &str, args: Vec<Term>) -> Term {
        Term::Func {
            name: name.into(),
            args,
        }
    }

    /// Each internal function the resolver writes is built as the node of
    /// the form it lowers, and a term's read as its node in term position.
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

        let read = Lit::Pos(Atom {
            pred: "attr".into(),
            args: vec![
                str_term("net.vpc"),
                str_term("vpc"),
                str_term("cidr"),
                var("Cidr"),
            ],
            record: None,
            span: Span::default(),
        });
        let id = b.hoisted(&var("Cidr"), &[read]);
        let e = &b.program.exprs[id];
        assert!(
            matches!(e.kind, ExprKind::Lookup { out: 3, .. }),
            "{:?}",
            e.kind
        );
        let v = &b.program.vars[e.hoisted.expect("read into its variable")];
        assert!(
            v.implicit && v.lowered == "Cidr" && v.name == "cidr",
            "{v:?}"
        );
    }
}
