//! A term's nodes to the term and the reads the resolver hoisted for it
//! (R-211 step 3): each node as the resolver writes it, a read before the
//! literal that holds it, in the order it was hoisted. `lower_expr` gives
//! back what the resolver's `term` gave for the same term, byte for byte.

use crate::ast::{Atom, Lit, Term, TypeExpr, str_term};
use crate::program::Program;
use crate::program::node::*;
use std::collections::BTreeMap;

/// The term `id` is, and the reads it hoists, in order.
pub fn lower_expr(program: &Program, id: ExprId) -> (Term, Vec<Lit>) {
    let mut l = Lowering {
        program,
        reads: Vec::new(),
    };
    let t = l.expr(id);
    (t, l.reads)
}

/// One term's lowering: the reads hoisted so far.
struct Lowering<'p> {
    program: &'p Program,
    reads: Vec<Lit>,
}

impl Lowering<'_> {
    fn expr(&mut self, id: ExprId) -> Term {
        let e = &self.program.exprs[id];
        match &e.kind {
            ExprKind::Lit(v) => Term::Val(v.clone()),
            ExprKind::Quantity { text } => crate::types::ambiguous_literal(text, e.span),
            ExprKind::Var(v) => self.var(*v),
            ExprKind::Hole => Term::Wildcard,
            ExprKind::Hoisted { reads, value } => {
                for &g in reads {
                    let lit = self.goal(g);
                    self.reads.push(lit);
                }
                self.expr(*value)
            }
            ExprKind::Binary { op, lhs, rhs } => {
                func(op.function(), vec![self.expr(*lhs), self.expr(*rhs)])
            }
            ExprKind::Neg(a) => func(
                "sub",
                vec![Term::Val(crate::value::Value::Int(0)), self.expr(*a)],
            ),
            ExprKind::Interp { parts } => self.interp(parts),
            ExprKind::Object { parts } => self.object(parts),
            ExprKind::List { parts } => self.list(parts),
            ExprKind::Range { lo, hi, inclusive } => func(
                crate::range::LOWERED,
                vec![
                    self.expr(*lo),
                    self.expr(*hi),
                    Term::Val(crate::value::Value::Bool(*inclusive)),
                ],
            ),
            ExprKind::As { value, ty } => {
                let TypeExpr::Name(ty) = ty else {
                    unreachable!("a value read as a type is read as a type's name: {ty:?}")
                };
                func(crate::types::AS, vec![self.expr(*value), str_term(ty)])
            }
            ExprKind::Field { base, path } => self.field(*base, path),
            ExprKind::RefOf(r) => self.reference(*r),
            ExprKind::Comprehension { item, clause } => {
                let body = self.clause(*clause);
                Term::ListComp {
                    item: Box::new(self.expr(*item)),
                    body,
                }
            }
            ExprKind::Call { callee, args } => {
                let name = match callee {
                    Callee::Std(f) => f.name.clone(),
                    Callee::Data(n) => n.clone(),
                    c => unreachable!("no builder calls {c:?} before step 5"),
                };
                let args = args.iter().map(|a| self.expr(a.value)).collect();
                Term::Func { name, args }
            }
            k => unreachable!("no builder makes {k:?} before steps 4-5"),
        }
    }

    fn var(&self, v: VarId) -> Term {
        Term::Var(self.program.vars[v].lowered.clone())
    }

    /// `"a${x}b"`: `str.format("a%sb", x)`.
    fn interp(&mut self, parts: &[Piece]) -> Term {
        let mut f = String::new();
        let mut args = vec![Term::Wildcard];
        for p in parts {
            match p {
                Piece::Text(t) => f.push_str(t),
                Piece::Hole(e) => {
                    f.push_str("%s");
                    args.push(self.expr(*e));
                }
            }
        }
        args[0] = str_term(&f);
        func(crate::ir::FORMAT, args)
    }

    /// Written fields an object; any computed key `__object` of each key
    /// and value; any spread `__merge` of the parts, each run of written
    /// fields one object.
    fn object(&mut self, parts: &[ObjPart]) -> Term {
        if parts.iter().any(|p| matches!(p, ObjPart::Computed { .. })) {
            let mut args = Vec::new();
            for p in parts {
                let (k, v) = match p {
                    ObjPart::Computed { key, value } => (self.expr(*key), *value),
                    ObjPart::Field { key, value, .. } => (str_term(key), *value),
                    ObjPart::Spread(_) => unreachable!("a computed object spreads nothing"),
                };
                args.extend([k, self.expr(v)]);
            }
            return func(crate::functions::OBJECT, args);
        }
        let mut out: Vec<Term> = Vec::new();
        let mut run: Option<BTreeMap<String, Term>> = None;
        for p in parts {
            match p {
                ObjPart::Field { key, value, .. } => {
                    let v = self.expr(*value);
                    run.get_or_insert_default().insert(key.clone(), v);
                }
                ObjPart::Spread(e) => {
                    out.extend(run.take().map(Term::Obj));
                    out.push(self.expr(*e));
                }
                ObjPart::Computed { .. } => unreachable!("handled above"),
            }
        }
        let spread = parts.iter().any(|p| matches!(p, ObjPart::Spread(_)));
        match (spread, run) {
            (false, run) => Term::Obj(run.unwrap_or_default()),
            (true, run) => {
                out.extend(run.map(Term::Obj));
                func(crate::functions::MERGE, out)
            }
        }
    }

    /// Written elements a list; any spread `__concat` of the parts, each
    /// run of written elements one list.
    fn list(&mut self, parts: &[ListPart]) -> Term {
        let mut out: Vec<Term> = Vec::new();
        let mut run: Option<Vec<Term>> = None;
        for p in parts {
            match p {
                ListPart::Elem(e) => {
                    let x = self.expr(*e);
                    run.get_or_insert_default().push(x);
                }
                ListPart::Spread(e) => {
                    out.extend(run.take().map(Term::List));
                    out.push(self.expr(*e));
                }
            }
        }
        let spread = parts.iter().any(|p| matches!(p, ListPart::Spread(_)));
        match (spread, run) {
            (false, run) => Term::List(run.unwrap_or_default()),
            (true, run) => {
                out.extend(run.map(Term::List));
                func(crate::functions::CONCAT, out)
            }
        }
    }

    /// A path into a value: `__path(v, "a.b")`, `__len(v)`.
    fn field(&mut self, base: ExprId, path: &[Step]) -> Term {
        let v = self.expr(base);
        match path {
            [Step::Len] => func(crate::ir::LEN, vec![v]),
            path => func("__path", vec![v, str_term(&stored(path))]),
        }
    }

    /// A resource, or a path into one, given as a value: `__ref(T, A,
    /// "p")`.
    fn reference(&mut self, r: ExprId) -> Term {
        let (res, path) = match &self.program.exprs[r].kind {
            ExprKind::Field { base, path } => (*base, stored(path)),
            _ => (r, String::new()),
        };
        let ExprKind::Resource { typ, addr } = &self.program.exprs[res].kind else {
            unreachable!("a reference is to a resource")
        };
        let addr = self.expr(*addr);
        func(
            crate::ir::REF,
            vec![str_term(&typ.name), addr, str_term(&path)],
        )
    }

    fn clause(&mut self, id: ClauseId) -> Vec<Lit> {
        let goals = self.program.clauses[id].goals.clone();
        goals.into_iter().map(|g| self.goal(g)).collect()
    }

    fn goal(&mut self, id: GoalId) -> Lit {
        let g = &self.program.goals[id];
        match &g.kind {
            GoalKind::Rel { rel, args } => Lit::Pos(self.atom(rel, args)),
            GoalKind::Not { clause, .. } => {
                let goals = &self.program.clauses[*clause].goals;
                let [one] = goals.as_slice() else {
                    unreachable!("no builder makes `not {{ .. }}` before step 4")
                };
                match self.goal(*one) {
                    Lit::Pos(a) => Lit::Not(a),
                    l => unreachable!("a negated literal is an atom: {l:?}"),
                }
            }
            GoalKind::Bind { pat, value } => {
                let p = self.pattern(*pat);
                Lit::Eq(p, self.expr(*value))
            }
            GoalKind::Compare { lhs, ops } => {
                let [(op, rhs)] = ops.as_slice() else {
                    unreachable!("no builder chains a comparison before step 4")
                };
                let (a, b) = (self.expr(*lhs), self.expr(*rhs));
                match op {
                    CmpOp::Eq => Lit::Eq(a, b),
                    CmpOp::Ne => Lit::Neq(a, b),
                    CmpOp::Gt => Lit::Gt(a, b),
                    CmpOp::Ge => Lit::Ge(a, b),
                    CmpOp::Lt => Lit::Lt(a, b),
                    CmpOp::Le => Lit::Le(a, b),
                }
            }
            k => unreachable!("no builder makes {k:?} before step 4"),
        }
    }

    fn atom(&mut self, rel: &RelRef, args: &RelArgs) -> Atom {
        let (args, record) = match args {
            RelArgs::Positional(ps) => (ps.iter().map(|p| self.pattern(*p)).collect(), None),
            RelArgs::Record(fs) => {
                let r = fs
                    .iter()
                    .map(|(k, p)| (k.clone(), self.pattern(*p)))
                    .collect();
                (Vec::new(), Some(r))
            }
        };
        Atom {
            pred: rel.name.clone(),
            args,
            record,
            span: rel.span,
        }
    }

    fn pattern(&mut self, id: PatternId) -> Term {
        match &self.program.patterns[id].kind {
            PatternKind::Hole => Term::Wildcard,
            PatternKind::Bind(v) => self.var(*v),
            PatternKind::Expr(e) => self.expr(*e),
            PatternKind::Tuple { elems, rest: None } => {
                Term::List(elems.iter().map(|p| self.pattern(*p)).collect())
            }
            PatternKind::Object { fields, rest: None } => Term::Obj(
                fields
                    .iter()
                    .map(|(k, _, p)| (k.clone(), self.pattern(*p)))
                    .collect(),
            ),
            k => unreachable!("no builder makes {k:?} before step 4"),
        }
    }
}

/// A path of field steps as stored: its segments joined by `.`.
fn stored(path: &[Step]) -> String {
    let segs: Vec<&str> = path
        .iter()
        .map(|s| match s {
            Step::Field(k, _) => k.as_str(),
            s => unreachable!("a stored path is of fields: {s:?}"),
        })
        .collect();
    segs.join(".")
}

fn func(name: &str, args: Vec<Term>) -> Term {
    Term::Func {
        name: name.to_string(),
        args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Span, var};
    use crate::program::Builder;
    use crate::value::Value;

    fn f(name: &str, args: Vec<Term>) -> Term {
        Term::Func {
            name: name.into(),
            args,
        }
    }

    fn at(pred: &str, args: Vec<Term>, start: u32) -> Atom {
        Atom {
            pred: pred.into(),
            args,
            record: None,
            span: Span {
                file: 1,
                start,
                end: start + 1,
                origin: 2,
            },
        }
    }

    /// The forms the corpus seldom writes lower back as the resolver wrote
    /// them: an empty spread part, a computed key, holes at a string's
    /// ends, a quantity's span, a path into a path, a reference's path, a
    /// comprehension's body, a record's columns, each read at its span.
    #[test]
    fn a_term_lowers_back_to_what_it_was_built_from() {
        let int = |i| Term::Val(Value::Int(i));
        let obj = |kv: &[(&str, Term)]| {
            Term::Obj(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
        };
        let mut record = at("zone", vec![], 9);
        record.record = Some([("name".to_string(), var("N"))].into());
        let cases = [
            (f("__merge", vec![var("X"), obj(&[])]), vec![]),
            (
                f(
                    "__merge",
                    vec![obj(&[("a", int(1))]), var("X"), obj(&[("b", var("Y"))])],
                ),
                vec![],
            ),
            (
                f(
                    "__concat",
                    vec![
                        Term::List(vec![]),
                        var("X"),
                        Term::List(vec![int(1), int(2)]),
                    ],
                ),
                vec![],
            ),
            (
                f(
                    "__object",
                    vec![
                        str_term("a"),
                        int(1),
                        f("str.format", vec![str_term("k-%s"), var("K")]),
                        var("V"),
                    ],
                ),
                vec![],
            ),
            (
                f("str.format", vec![str_term("%s-%s"), var("A"), var("B")]),
                vec![],
            ),
            (
                f(
                    "__quantity",
                    vec![str_term("500m"), int(3), int(10), int(14)],
                ),
                vec![],
            ),
            (
                f(
                    "__path",
                    vec![
                        f("__path", vec![var("X"), str_term("a.b")]),
                        str_term("\"c.d\"[0]"),
                    ],
                ),
                vec![],
            ),
            (
                f(
                    "__ref",
                    vec![str_term("net.vpc"), var("A"), str_term("spec.cidr")],
                ),
                vec![],
            ),
            (
                Term::ListComp {
                    item: Box::new(f("__len", vec![var("X")])),
                    body: vec![
                        Lit::Pos(at(
                            "p",
                            vec![var("X"), Term::Wildcard, Term::List(vec![var("Y"), int(1)])],
                            1,
                        )),
                        Lit::Not(at("q", vec![var("X")], 2)),
                        Lit::Eq(obj(&[("a", var("A"))]), var("X")),
                        Lit::Gt(var("A"), int(0)),
                    ],
                },
                vec![],
            ),
            (
                var("V"),
                vec![
                    Lit::Pos(record),
                    Lit::Eq(
                        var("V"),
                        f("add", vec![var("N"), f("sub", vec![int(0), int(1)])]),
                    ),
                ],
            ),
        ];
        for (t, reads) in cases {
            let mut p = Program::new();
            let written = |_: &str| true;
            let id = Builder::new(&mut p, Span::default(), &written).hoisted(&t, &reads);
            let (back, rs) = lower_expr(&p, id);
            assert_eq!(format!("{back:?}"), format!("{t:?}"));
            assert_eq!(format!("{rs:?}"), format!("{reads:?}"));
            let spans = |ls: &[Lit]| -> Vec<(u32, u32)> {
                ls.iter()
                    .filter_map(|l| match l {
                        Lit::Pos(a) => Some((a.span.start, a.span.origin)),
                        _ => None,
                    })
                    .collect()
            };
            assert_eq!(spans(&rs), spans(&reads));
        }
    }
}
