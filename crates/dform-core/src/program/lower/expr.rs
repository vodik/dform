//! A term's nodes to the term and the reads the resolver hoisted for it
//! (R-211 step 3): each node as the resolver writes it, a read in term
//! position (`Expr::hoisted`) the literal it is, before the literal that
//! holds the term, its variable in its place, in the order the term holds
//! them (step 6b). `lower_expr` gives back what the resolver's `term`
//! gave for the same term, byte for byte.

use crate::ast::{Atom, Lit, Span, Stmt, Term, TypeExpr, str_term};
use crate::program::node::*;
use crate::program::{NodeId, Program};
use std::collections::BTreeMap;

/// The term `id` is, and the reads it hoists, in order.
pub fn lower_expr(program: &Program, id: ExprId) -> (Term, Vec<Lit>) {
    let mut l = Lowering::new(program);
    let t = l.expr(id);
    (t, l.reads)
}

/// A lowering in progress: the reads the term being lowered hoisted so
/// far, and the helper statements written so far (a `not { }`'s rule, the
/// facts a membership states), in order.
pub(super) struct Lowering<'p> {
    pub(super) program: &'p Program,
    pub(super) reads: Vec<Lit>,
    pub(super) helpers: Vec<Stmt>,
}

impl<'p> Lowering<'p> {
    pub(super) fn new(program: &'p Program) -> Self {
        Lowering {
            program,
            reads: Vec::new(),
            helpers: Vec::new(),
        }
    }

    pub(super) fn expr(&mut self, id: ExprId) -> Term {
        self.terms_made(NodeId::Expr(id));
        match self.program.exprs[id].hoisted {
            Some(v) => self.hoist(id, v),
            None => self.kind(id),
        }
    }

    /// The read in term position `id`, read into `v`: its literal onto
    /// the reads, after those its own terms hold; the term is `v`.
    fn hoist(&mut self, id: ExprId, v: VarId) -> Term {
        let span = self.program.exprs[id].span;
        if let ExprKind::Field { base, path } = &self.program.exprs[id].kind
            && let [Step::Keyed { .. }] = path.as_slice()
        {
            self.keyed(*base, &path[0], v, span);
            return self.var(v);
        }
        let lit = match self.read_atom(id, span, |l| l.var(v)) {
            Some(a) => Lit::Pos(a),
            None => {
                let t = self.kind(id);
                Lit::Eq(self.var(v), t)
            }
        };
        self.reads.push(lit);
        self.var(v)
    }

    /// The element `v` of the keyed list `base` by its key (`step`):
    /// `member(L, V), type_list_key(T, "l", Keys), Keys = [Key],
    /// __path(V, Key) == k`.
    fn keyed(&mut self, base: ExprId, step: &Step, v: VarId, span: Span) {
        let Step::Keyed {
            key,
            typ,
            list,
            keys,
            field,
        } = step
        else {
            unreachable!("a keyed step")
        };
        let (base, key, typ) = (self.expr(base), self.expr(*key), self.expr(*typ));
        let (item, keys, field) = (self.var(v), self.var(*keys), self.var(*field));
        let atom = |pred: &str, args| {
            Lit::Pos(Atom {
                pred: pred.to_string(),
                args,
                record: None,
                span,
            })
        };
        self.reads.extend([
            atom("member", vec![base, item.clone()]),
            atom("type_list_key", vec![typ, str_term(list), keys.clone()]),
            Lit::Eq(keys, Term::List(vec![field.clone()])),
            Lit::Eq(func("__path", vec![item, field]), key),
        ]);
    }

    /// The read `value` is, its value `value_of`, as the atom the front
    /// end hoists it as at `span`: a value's `k(V)`, a resource's
    /// attribute's `attr("T", A, "p", V)`, a copy's output's `output(C,
    /// "k", V)`, a live object's `cloud_attr("T", A, "p", V)`, a lookup's
    /// relation with `V` its `out`th column, an element's `member(L, i,
    /// V)`, a relation's read in its column. `None` for any other value.
    pub(super) fn read_atom(
        &mut self,
        value: ExprId,
        span: Span,
        value_of: impl FnOnce(&mut Self) -> Term,
    ) -> Option<Atom> {
        let (pred, args, at) = match &self.program.exprs[value].kind {
            ExprKind::Value { decl, via } => (via.relation(&decl.name), Vec::new(), 0),
            ExprKind::Field { base, path } => match (&self.program.exprs[*base].kind, &path[..]) {
                (ExprKind::Resource { typ, addr }, [Step::Field(p, _)]) => {
                    let args = vec![str_term(&typ.name), self.expr(*addr), str_term(p)];
                    ("attr".to_string(), args, 3)
                }
                (_, [Step::Index(i)]) => {
                    let args = vec![self.expr(*base), self.expr(*i)];
                    ("member".to_string(), args, 2)
                }
                _ => return None,
            },
            ExprKind::Output { copy, key, of } => {
                let copy = self.expr(*copy);
                if let Some((of, scope)) = of {
                    // At the test's own place, its scope's.
                    let at = self.program.exprs[*scope].span;
                    let args = vec![str_term(of), self.expr(*scope), copy.clone()];
                    let test = Atom {
                        pred: crate::modules::INSTANCE_OF.to_string(),
                        args,
                        record: None,
                        span: at,
                    };
                    self.reads.push(Lit::Pos(test));
                }
                ("output".to_string(), vec![copy, str_term(key)], 2)
            }
            ExprKind::World { typ, addr, path } => {
                let path = stored(path);
                let args = vec![str_term(&typ.name), self.expr(*addr), str_term(&path)];
                ("cloud_attr".to_string(), args, 3)
            }
            ExprKind::Lookup { rel, args, out, .. } => {
                let args = args.iter().map(|a| self.expr(*a)).collect();
                (rel.name.clone(), args, *out)
            }
            ExprKind::Address { of, typ } => {
                let v = value_of(self);
                let mark = func(crate::modules::ABSOLUTE, vec![v]);
                let r = func(
                    crate::address::REF,
                    vec![str_term(&typ.name), mark, str_term("")],
                );
                let at = self.program.exprs[*of].span;
                return self.read_holding(*of, at, r);
            }
            _ => return None,
        };
        let mut args = args;
        args.insert(at, value_of(self));
        Some(Atom {
            pred,
            args,
            record: None,
            span,
        })
    }

    /// The read `value` is with `held` in its value column, at `span`
    /// ([`Self::read_atom`]).
    pub(super) fn read_holding(&mut self, value: ExprId, span: Span, held: Term) -> Option<Atom> {
        self.read_atom(value, span, |_| held)
    }

    /// The term node `id` is, its reads lowered where they stand.
    fn kind(&mut self, id: ExprId) -> Term {
        let e = &self.program.exprs[id];
        match &e.kind {
            ExprKind::Lit(v) => Term::Val(v.clone()),
            ExprKind::Quantity { text } => crate::types::ambiguous_literal(text, e.span),
            ExprKind::Var(v) => self.var(*v),
            ExprKind::Hole => Term::Wildcard,
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
            ExprKind::Alias(e) => self.expr(*e),
            ExprKind::Comprehension { item, clause } => {
                let mut body = Vec::new();
                self.clause(*clause, &mut body);
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
            ExprKind::Aggregate { kind, item } => func(
                super::super::node::aggregate_name(*kind),
                vec![self.expr(*item)],
            ),
            k => unreachable!("no builder makes {k:?} as a term before step 5"),
        }
    }

    /// The helper statements the terms under `node` made, written before
    /// its own.
    pub(super) fn terms_made(&mut self, node: NodeId) {
        if let Some(made) = self.program.terms_made.get(&node) {
            self.helpers.extend(made.iter().cloned());
        }
    }

    pub(super) fn var(&self, v: VarId) -> Term {
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
        func(crate::address::FORMAT, args)
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
            [Step::Len] => func(crate::address::LEN, vec![v]),
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
            crate::address::REF,
            vec![str_term(&typ.name), addr, str_term(&path)],
        )
    }
}

/// A path of field steps as stored: its segments joined by `.`.
pub(super) fn stored(path: &[Step]) -> String {
    let segs: Vec<&str> = path
        .iter()
        .map(|s| match s {
            Step::Field(k, _) => k.as_str(),
            s => unreachable!("a stored path is of fields: {s:?}"),
        })
        .collect();
    segs.join(".")
}

pub(super) fn func(name: &str, args: Vec<Term>) -> Term {
    Term::Func {
        name: name.to_string(),
        args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Atom, Span, var};
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
    /// comprehension's body and a record's columns in it, a term's reads
    /// in term position, each read at its span.
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
                        Lit::Pos(record),
                    ],
                },
                vec![],
            ),
            (
                var("V"),
                vec![
                    Lit::Pos(at("zone", vec![str_term("a"), var("N")], 9)),
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
