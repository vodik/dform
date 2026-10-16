//! A term's nodes to the term and the reads the resolver hoisted for it
//! (R-211 step 3): each node as the resolver writes it, a read in term
//! position (`Expr::hoisted`) the literal it is, before the literal that
//! holds the term, its variable in its place, in the order the term holds
//! them (step 6b). `lower_expr` gives back what the resolver's `term`
//! gave for the same term, byte for byte.

use super::Env;
use super::instances::From;
use crate::ast::{Atom, Lit, Span, Stmt, Term, TypeExpr, str_term};
use crate::program::node::*;
use crate::program::scope::Reach;
use crate::program::{NodeId, Program, Readings};
use std::collections::{BTreeMap, BTreeSet};

/// The term `id` is, and the reads it hoists, in order.
pub fn lower_expr(program: &Program, id: ExprId) -> (Term, Vec<Lit>) {
    let mut l = Lowering::new(Env::plain(program));
    let t = l.expr(id);
    (t, l.reads)
}

/// The term `id` is, its nodes read as `readings` reads them (over the
/// program's own): what a reading being decided makes of it.
pub fn read_expr(program: &Program, readings: &Readings, id: ExprId) -> Term {
    let mut l = Lowering {
        readings,
        ..Lowering::new(Env::plain(program))
    };
    l.expr(id)
}

/// The helper statements lowering the term `id` makes (a comprehension's
/// `not { }` rule).
pub fn expr_helpers(program: &Program, id: ExprId) -> Vec<Stmt> {
    let mut l = Lowering::new(Env::plain(program));
    l.expr(id);
    l.helpers
}

/// A lowering in progress: the reads the term being lowered hoisted so
/// far, and the helper statements written so far (a `not { }`'s rule, the
/// facts a membership states), in order.
pub(super) struct Lowering<'p> {
    pub(super) program: &'p Program,
    /// What the nodes are lowered under ([`Env`]).
    pub(super) env: Env<'p>,
    /// What each node is read as, the program's own unless a reading is
    /// being decided ([`read_expr`]).
    readings: &'p Readings,
    pub(super) reads: Vec<Lit>,
    pub(super) helpers: Vec<Stmt>,
    /// The relations a copy's block gives rows of (R-55), under the
    /// copy's scope: each such head is the copy's own relation.
    pub(super) taken: Option<(&'p str, &'p BTreeSet<String>)>,
}

impl<'p> Lowering<'p> {
    pub(super) fn new(env: Env<'p>) -> Self {
        Lowering {
            program: env.program,
            env,
            readings: &env.program.readings,
            reads: Vec::new(),
            helpers: Vec::new(),
            taken: None,
        }
    }

    /// `atom` as the copies being instanced write it (`Chain::atom`):
    /// each part from where this lowering starts.
    pub(super) fn put(&self, atom: Atom) -> Atom {
        match self.env.chain {
            None => atom,
            Some(c) => c.atom_from(atom, self.env.from),
        }
    }

    /// [`Self::put`], the relation's name from `pred` and each column from
    /// its own start.
    pub(super) fn put_at(&self, atom: Atom, pred: Option<From>, args: &[Option<From>]) -> Atom {
        match self.env.chain {
            None => atom,
            Some(c) => c.atom(atom, pred, args),
        }
    }

    /// A statement no node lowers to (a resolver's helper) as the copies
    /// write it.
    pub(super) fn put_raw(&self, s: Stmt) -> Stmt {
        match self.env.chain {
            None => s,
            Some(c) => super::instances::raw_stmt(c, s, self.env.from),
        }
    }

    /// The relation `rel` reads: an enclosing definition's (R-186) the
    /// relation of the instance it is taken from; and where the copies'
    /// parts start for it.
    pub(super) fn relation(&self, rel: &RelRef) -> (String, Option<From>) {
        match (self.env.chain, &rel.via) {
            (Some(c), Reach::Lexical(body)) => c.lexical_pred(body, &rel.name, rel.span),
            _ => (rel.via.relation(&rel.name), self.env.from),
        }
    }

    /// Where the copies' parts start for the term `e` in an atom's
    /// column: a name its scope reaches it by (`Reach::Absolute`, an
    /// enclosing definition's) its own, any other this lowering's.
    pub(super) fn start_of(&self, e: ExprId) -> Option<From> {
        let c = self.env.chain?;
        let x = &self.program.exprs[e];
        if self.readings.contains_key(e) {
            return self.env.from;
        }
        match &x.kind {
            ExprKind::Scoped {
                via: Reach::Absolute,
                ..
            } => None,
            ExprKind::Scoped {
                name,
                via: Reach::Lexical(body),
            } => c.lexical_term(body, &self.segment(*name), x.span).1,
            _ => self.env.from,
        }
    }

    /// [`Self::start_of`] a pattern's: an expression's, else this
    /// lowering's.
    pub(super) fn pattern_from(&self, p: PatternId) -> Option<From> {
        match &self.program.patterns[p].kind {
            PatternKind::Expr(e) => self.start_of(*e),
            _ => self.env.from,
        }
    }

    /// The term `id` is, as its position reads it (its reading, R-192):
    /// its reads hoisted and its helpers made either way.
    pub(super) fn expr(&mut self, id: ExprId) -> Term {
        self.terms_made(NodeId::Expr(id));
        let t = match self.program.exprs[id].hoisted {
            Some(v) => self.hoist(id, v),
            None => self.kind(id),
        };
        let t = match (self.readings.get(id), self.env.chain) {
            (Some(read), Some(c)) => c.raw_term(read.clone(), self.env.from),
            (Some(read), None) => read.clone(),
            (None, _) => t,
        };
        match self.env.visit {
            Some(visit) => visit.term(id, t),
            None => t,
        }
    }

    /// The segment an enclosing body's name `name` is: its own term, as
    /// written (no pass's).
    fn segment(&self, name: ExprId) -> String {
        let Term::Val(crate::value::Value::Str(n)) =
            Lowering::new(Env::plain(self.program)).expr(name)
        else {
            unreachable!("an enclosing body's name is its segment")
        };
        n
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
            Lit::Pos(self.put(Atom {
                pred: pred.to_string(),
                args,
                record: None,
                span,
            }))
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
        let all = self.env.from;
        let ((pred, pred_at), args, at) = match &self.program.exprs[value].kind {
            ExprKind::Value { decl, via } => {
                (self.value_relation(&decl.name, via, span), Vec::new(), 0)
            }
            ExprKind::Field { base, path } => self.field_read(*base, path)?,
            ExprKind::Output { copy, key, of } => self.output_read(*copy, key, of.as_ref()),
            ExprKind::World { typ, addr, path } => {
                let path = stored(path);
                let args = vec![
                    (str_term(&typ.name), all),
                    (self.expr(*addr), self.start_of(*addr)),
                    (str_term(&path), all),
                ];
                (("cloud_attr".to_string(), all), args, 3)
            }
            ExprKind::Lookup { rel, args, out, .. } => {
                let args = args
                    .iter()
                    .map(|a| (self.expr(*a), self.start_of(*a)))
                    .collect();
                (self.relation(rel), args, *out)
            }
            ExprKind::Address { of, typ } => {
                let v = value_of(self);
                let r = func(
                    crate::address::REF,
                    vec![str_term(&typ.name), v, str_term("")],
                );
                let at = self.program.exprs[*of].span;
                return self.read_holding(*of, at, r);
            }
            _ => return None,
        };
        let (mut args, mut froms): (Vec<Term>, Vec<Option<From>>) = args.into_iter().unzip();
        args.insert(at, value_of(self));
        froms.insert(at, all);
        let atom = Atom {
            pred,
            args,
            record: None,
            span,
        };
        Some(self.put_at(atom, pred_at, &froms))
    }

    /// A field read: a resource's attribute's `attr("T", A, "p", V)`, an
    /// element's `member(L, i, V)`.
    fn field_read(&mut self, base: ExprId, path: &[Step]) -> Option<ReadParts> {
        let all = self.env.from;
        Some(match (&self.program.exprs[base].kind, path) {
            (ExprKind::Resource { typ, addr }, [Step::Field(p, _)]) => {
                let args = vec![
                    (str_term(&typ.name), all),
                    (self.expr(*addr), self.start_of(*addr)),
                    (str_term(p), all),
                ];
                (("attr".to_string(), all), args, 3)
            }
            (_, [Step::Index(i)]) => {
                let args = vec![(self.expr(base), all), (self.expr(*i), all)];
                (("member".to_string(), all), args, 2)
            }
            _ => return None,
        })
    }

    /// A copy's output's read, `output(C, "k", V)`, after the test that
    /// `C` is a copy of the component `of` in its scope, at the test's own
    /// place.
    fn output_read(&mut self, copy: ExprId, key: &str, of: Option<&(String, ExprId)>) -> ReadParts {
        let all = self.env.from;
        let copy_at = self.start_of(copy);
        let copy = self.expr(copy);
        if let Some((of, scope)) = of {
            let at = self.program.exprs[*scope].span;
            let scope_at = self.start_of(*scope);
            let args = vec![str_term(of), self.expr(*scope), copy.clone()];
            let test = Atom {
                pred: crate::modules::INSTANCE_OF.to_string(),
                args,
                record: None,
                span: at,
            };
            let test = self.put_at(test, all, &[all, scope_at, copy_at]);
            self.reads.push(Lit::Pos(test));
        }
        (
            ("output".to_string(), all),
            vec![(copy, copy_at), (str_term(key), all)],
            2,
        )
    }

    /// The relation a value read `via` reads `name` by, and where the
    /// copies' parts start for it: an enclosing definition's (R-186), the
    /// instance's it is taken from.
    fn value_relation(&self, name: &str, via: &Reach, span: Span) -> (String, Option<From>) {
        match (self.env.chain, via) {
            (Some(c), Reach::Lexical(body)) => c.lexical_pred(body, name, span),
            _ => (via.relation(name), self.env.from),
        }
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
            ExprKind::Scoped { name, via } => self.scoped(*name, via),
            ExprKind::Alias(e) => self.expr(*e),
            // The item's reads are the body's, after its literals.
            ExprKind::Comprehension { item, clause } => {
                let mut body = Vec::new();
                self.clause(*clause, &mut body);
                let outer = std::mem::take(&mut self.reads);
                let item = self.expr(*item);
                body.extend(std::mem::replace(&mut self.reads, outer));
                Term::ListComp {
                    item: Box::new(item),
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
            let made: Vec<Stmt> = made.iter().map(|s| self.put_raw(s.clone())).collect();
            self.helpers.extend(made);
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

    /// The name `name` as a read reaches it: as written when its user's
    /// (no copy puts its scope in front of it); an enclosing body's the
    /// instance's under the copies being instanced, else as written.
    fn scoped(&mut self, name: ExprId, via: &Reach) -> Term {
        let span = self.program.exprs[name].span;
        match (self.env.chain, via) {
            (Some(c), Reach::Lexical(body)) => c.lexical_term(body, &self.segment(name), span).0,
            (_, Reach::Absolute | Reach::Lexical(_)) => self.expr(name),
            (_, r) => unreachable!("a name {r:?} is written bare"),
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
        let at = self.start_of(*addr);
        let addr = self.expr(*addr);
        match self.env.chain {
            Some(c) => c.reference(str_term(&typ.name), addr, at, str_term(&path)),
            None => func(
                crate::address::REF,
                vec![str_term(&typ.name), addr, str_term(&path)],
            ),
        }
    }
}

/// A read's relation and where the copies' parts start for it, its
/// columns but the value's each with its own start, and the value's
/// column.
type ReadParts = ((String, Option<From>), Vec<(Term, Option<From>)>, usize);

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
