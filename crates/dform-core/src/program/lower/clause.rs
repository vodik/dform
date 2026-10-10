//! A clause's goals to the literals the resolver writes for them, and
//! the helper statements they make, in the order it makes them (R-211
//! step 4): `not { B }`'s rule `__neg_N(ȳ) :- P, B` (its number the one
//! stamped at build), the facts a membership states (`__namespace`,
//! `__provider_type`, `__enum`), `has`'s mark before what it covers.
//!
//! A goal lowers onto the clause's literals so far, `out`: a `not`'s
//! helper takes the positive ones before it as its P, as the resolver's
//! `neg_helper` does.

use super::expr::Lowering;
use crate::ast::{Atom, Helper, Lit, RuleStmt, Span, Stmt, Term, str_term};
use crate::program::node::*;
use crate::program::{NodeId, Program};
use crate::value::Value;
use std::collections::BTreeSet;

/// The literals clause `id` lowers to after `context` (what its
/// statement holds before it: a let's parameters' binding, R-187), and
/// the helper statements it makes, in order.
pub fn lower_clause(program: &Program, id: ClauseId, context: &[Lit]) -> (Vec<Lit>, Vec<Stmt>) {
    let mut l = Lowering::new(program);
    let mut out = context.to_vec();
    l.clause(id, &mut out);
    out.drain(..context.len());
    (out, l.helpers)
}

/// The literals goal `id` lowers to after `context`, its clause's
/// literals before it, and the helper statements it makes.
pub fn lower_goal(program: &Program, id: GoalId, context: &[Lit]) -> (Vec<Lit>, Vec<Stmt>) {
    let mut l = Lowering::new(program);
    let mut out = context.to_vec();
    l.goal(id, &mut out);
    out.drain(..context.len());
    (out, l.helpers)
}

/// One aggregate binding of a clause: the variable, the call, and its
/// `__agg_N` when it folds through one.
pub(super) type Fold = (Term, Term, Option<u32>);

/// The rule `head :- lits` with the aggregate bindings `folds` folded, as
/// the resolver's `fold_rule` writes it: a fold with no helper number
/// applied by the head (`p(K, count(X)) :- B`); else the rule over each
/// fold's `__agg_N(group, agg(x)) :- B`, joined by the group (the head's
/// other variables and what the literals after the fold read, as B binds
/// them), the literals that read a fold's value after it, then each
/// helper.
pub(super) fn folded(mut head: Atom, lits: Vec<Lit>, folds: Vec<Fold>, span: Span) -> Vec<Stmt> {
    let results: BTreeSet<&str> = folds
        .iter()
        .filter_map(|(v, ..)| match v {
            Term::Var(v) => Some(v.as_str()),
            _ => None,
        })
        .collect();
    let (post, base): (Vec<Lit>, Vec<Lit>) = lits.into_iter().partition(|l| reads_any(l, &results));
    if let [(v, call, None)] = folds.as_slice() {
        let at = head
            .args
            .iter()
            .position(|t| t == v)
            .expect("the head applies it");
        head.args[at] = call.clone();
        return vec![Stmt::Rule(RuleStmt::new(head, base))];
    }
    let bound = crate::syntax::resolve::bound_vars(&base);
    let mut wanted = BTreeSet::new();
    for t in head
        .args
        .iter()
        .chain(head.record.iter().flat_map(|r| r.values()))
    {
        crate::syntax::resolve::lit_vars(&Lit::Eq(t.clone(), t.clone()), &mut wanted);
    }
    for l in &post {
        crate::syntax::resolve::lit_vars(l, &mut wanted);
    }
    let group: Vec<Term> = wanted
        .iter()
        .filter(|v| !results.contains(v.as_str()) && bound.contains(*v))
        .map(|v| Term::Var(v.clone()))
        .collect();
    let mut helpers = Vec::new();
    let mut body = Vec::new();
    for (v, call, n) in folds {
        let pred = format!("__agg_{}", n.expect("a grouped fold's helper number"));
        let mut args = group.clone();
        args.push(call);
        let rule = RuleStmt::helper(Helper::Aggregate, atom_at(&pred, args, span), base.clone());
        helpers.push(Stmt::Rule(rule));
        let mut args = group.clone();
        args.push(v);
        body.push(Lit::Pos(atom_at(&pred, args, span)));
    }
    body.extend(post);
    let mut out = vec![Stmt::Rule(RuleStmt::new(head, body))];
    out.extend(helpers);
    out
}

impl Lowering<'_> {
    /// Clause `id`'s literals but its aggregate bindings, and those
    /// bindings: each fold's call (its reads hoisted with the literals).
    pub(super) fn unfolded(&mut self, id: ClauseId) -> (Vec<Lit>, Vec<Fold>) {
        self.unfolded_after(id, Vec::new())
    }

    /// [`Self::unfolded`] after `lits`, the literals its statement holds
    /// before it (a let's parameters' demand, R-187).
    pub(super) fn unfolded_after(
        &mut self,
        id: ClauseId,
        mut lits: Vec<Lit>,
    ) -> (Vec<Lit>, Vec<Fold>) {
        let mut folds = Vec::new();
        for &g in &self.program.clauses[id].goals {
            let (fold, after) = match &self.program.goals[g].kind {
                GoalKind::Fold { .. } => (g, &[][..]),
                GoalKind::Group { goals, after }
                    if let [f] = goals.as_slice()
                        && matches!(self.program.goals[*f].kind, GoalKind::Fold { .. }) =>
                {
                    (*f, after.as_slice())
                }
                _ => {
                    self.goal(g, &mut lits);
                    continue;
                }
            };
            self.terms_made(NodeId::Goal(g));
            let GoalKind::Fold { var, agg, helper } = &self.program.goals[fold].kind else {
                unreachable!("a fold")
            };
            let call = self.expr(*agg);
            lits.append(&mut self.reads);
            folds.push((self.var(*var), call, *helper));
            for &a in after {
                self.goal(a, &mut lits);
            }
        }
        (lits, folds)
    }

    /// Clause `id`'s literals onto `out`.
    pub(super) fn clause(&mut self, id: ClauseId, out: &mut Vec<Lit>) {
        for &g in &self.program.clauses[id].goals {
            self.goal(g, out);
        }
    }

    /// Goal `id`'s literals onto `out`, its clause's so far: the reads its
    /// terms hoist, then its own.
    pub(super) fn goal(&mut self, id: GoalId, out: &mut Vec<Lit>) {
        self.terms_made(NodeId::Goal(id));
        let g = &self.program.goals[id];
        match &g.kind {
            GoalKind::Group { goals, after } => {
                for &each in goals.iter().chain(after) {
                    self.goal(each, out);
                }
            }
            GoalKind::Marked { mark, goal } => {
                // The reads that name the resource before the mark.
                let saved = std::mem::take(&mut self.reads);
                let (typ, addr) = (self.expr(mark.typ), self.expr(mark.addr));
                out.extend(std::mem::replace(&mut self.reads, saved));
                let start = out.len();
                self.goal(*goal, out);
                let covered = (out.len() - start) as i64;
                let mark = atom_at(
                    crate::partition::HAS,
                    vec![
                        typ,
                        addr,
                        str_term(&mark.path),
                        Term::Val(Value::Int(covered)),
                    ],
                    g.span,
                );
                let negated = matches!(self.program.goals[*goal].kind, GoalKind::Not { .. });
                out.insert(
                    start,
                    if negated {
                        Lit::Not(mark)
                    } else {
                        Lit::Pos(mark)
                    },
                );
            }
            GoalKind::Not {
                clause,
                helper: None,
            } => {
                let [one] = self.program.clauses[*clause].goals.as_slice() else {
                    unreachable!("a `not` without a helper negates one goal")
                };
                self.goal(*one, out);
                let last = out.pop().expect("a goal lowers to a literal");
                out.push(negate(last));
            }
            GoalKind::Not {
                clause,
                helper: Some(h),
            } => self.negation(*clause, h, g.span, out),
            _ => {
                // A goal's terms hoist their reads before it.
                let saved = std::mem::take(&mut self.reads);
                let lits = self.literals(id);
                let reads = std::mem::replace(&mut self.reads, saved);
                out.extend(reads);
                out.extend(lits);
            }
        }
    }

    /// `not { B }` through its helper: `__neg_N(ȳ) :- P, B`, after the
    /// helpers B makes, and `not __neg_N(ȳ)`.
    fn negation(
        &mut self,
        clause: ClauseId,
        h: &NegHelper,
        span: crate::ast::Span,
        out: &mut Vec<Lit>,
    ) {
        let mut inner = Vec::new();
        self.clause(clause, &mut inner);
        let args = h.args.iter().map(|v| self.var(*v)).collect();
        let head = atom_at(&format!("__neg_{}", h.number), args, span);
        let folded: BTreeSet<&str> = h
            .folded
            .iter()
            .map(|v| self.program.vars[*v].lowered.as_str())
            .collect();
        let mut body: Vec<Lit> = out
            .iter()
            .filter(|l| !matches!(l, Lit::Not(_)) && !reads_any(l, &folded))
            .cloned()
            .collect();
        body.extend(inner);
        self.helpers.push(Stmt::Rule(RuleStmt::helper(
            Helper::Negation,
            head.clone(),
            body,
        )));
        out.push(Lit::Not(head));
    }

    /// The literals of a goal that is one written literal's own.
    fn literals(&mut self, id: GoalId) -> Vec<Lit> {
        let g = &self.program.goals[id];
        let span = g.span;
        match &g.kind {
            GoalKind::Rel { rel, args } => vec![Lit::Pos(self.atom(rel, args))],
            GoalKind::Bind { pat, value } => {
                // A read bound in place (`x = R.p`, `P = e[i]`, a value's
                // `k(V)`): the read with the pattern in its value column.
                if self.program.exprs[*value].hoisted.is_none()
                    && let Some(read) = self.read_atom(*value, span, |l| l.pattern(*pat))
                {
                    return vec![Lit::Pos(read)];
                }
                let p = self.pattern(*pat);
                vec![Lit::Eq(p, self.expr(*value))]
            }
            GoalKind::Compare { lhs, ops } => {
                if self.in_place(*lhs) {
                    let [(CmpOp::Eq, v)] = ops.as_slice() else {
                        unreachable!("a read is compared in place by `==`")
                    };
                    let v = self.expr(*v);
                    return vec![self.in_place_read(*lhs, v)];
                }
                let mut prev = self.expr(*lhs);
                let mut lits = Vec::new();
                for (op, e) in ops {
                    let next = self.expr(*e);
                    lits.push(compare(*op, prev, next.clone()));
                    prev = next;
                }
                lits
            }
            GoalKind::Truth(e) => {
                let yes = Term::Val(Value::Bool(true));
                match self.in_place(*e) {
                    true => vec![self.in_place_read(*e, yes)],
                    false => vec![Lit::Eq(self.expr(*e), yes)],
                }
            }
            GoalKind::Has(Has::Resource { typ, addr }) => {
                let args = vec![self.expr(*typ), self.expr(*addr)];
                vec![Lit::Pos(atom_at(crate::partition::IDENTITY, args, span))]
            }
            GoalKind::Has(Has::Read(e)) => vec![self.in_place_read(*e, Term::Wildcard)],
            GoalKind::Has(Has::Walk { var, value }) => {
                vec![Lit::Eq(self.var(*var), self.expr(*value))]
            }
            GoalKind::Member { pat, coll } => self.member(*pat, coll, span),
            GoalKind::Fold { var, agg, .. } => vec![Lit::Eq(self.var(*var), self.expr(*agg))],
            GoalKind::Group { .. } | GoalKind::Marked { .. } | GoalKind::Not { .. } => {
                unreachable!("lowered by `goal`")
            }
        }
    }

    /// `x in c`: the literals, the facts it states onto the helpers.
    fn member(&mut self, pat: PatternId, coll: &Coll, span: crate::ast::Span) -> Vec<Lit> {
        let fact = |l: &mut Self, pred: &str, args: Vec<Term>, at| {
            let a = atom_at(pred, args, at);
            l.helpers.push(Stmt::Fact(a));
        };
        match coll {
            Coll::Expr(e) => {
                let e = self.expr(*e);
                let mut args = vec![e];
                args.extend(self.columns(pat));
                vec![Lit::Pos(atom_at("member", args, span))]
            }
            Coll::Enum {
                name,
                values,
                def,
                list,
            } => {
                let values = values.iter().cloned().map(Value::Str).collect();
                fact(
                    self,
                    ENUM,
                    vec![str_term(name), Term::Val(Value::List(values))],
                    *def,
                );
                let mut args = vec![self.expr(*list)];
                args.extend(self.columns(pat));
                vec![Lit::Pos(atom_at("member", args, span))]
            }
            Coll::Type(t) => {
                let args = vec![self.expr(*t), self.pattern(pat)];
                vec![Lit::Pos(atom_at("want", args, span))]
            }
            Coll::Namespace {
                ns,
                types,
                typ,
                enumerate,
            } => {
                for t in types {
                    fact(self, NAMESPACE, vec![str_term(ns), str_term(t)], span);
                }
                let typ = self.expr(*typ);
                let test = atom_at(NAMESPACE, vec![str_term(ns), typ.clone()], span);
                self.enumerate(test, typ, pat, *enumerate, span)
            }
            Coll::ProviderType {
                typ,
                names,
                var,
                enumerate,
            } => {
                for n in names {
                    fact(self, PROVIDER_TYPE, vec![str_term(typ), str_term(n)], span);
                }
                let var = self.expr(*var);
                let test = atom_at(PROVIDER_TYPE, vec![str_term(typ), var.clone()], span);
                self.enumerate(test, var, pat, *enumerate, span)
            }
            Coll::World(t) => {
                let args = vec![str_term(t), self.pattern(pat)];
                vec![Lit::Pos(atom_at("cloud_exists", args, span))]
            }
            Coll::Copies { component, scope } => {
                let args = vec![str_term(component), self.expr(*scope), self.pattern(pat)];
                let pred = crate::modules::INSTANCE_OF;
                vec![Lit::Pos(atom_at(pred, args, span))]
            }
            Coll::Each(e) | Coll::TypeOf(e) => {
                let p = self.pattern(pat);
                vec![Lit::Eq(p, self.expr(*e))]
            }
        }
    }

    /// A type's test, then `want(typ, x)` when the membership enumerates.
    fn enumerate(
        &mut self,
        test: Atom,
        typ: Term,
        pat: PatternId,
        enumerate: bool,
        span: crate::ast::Span,
    ) -> Vec<Lit> {
        let mut lits = vec![Lit::Pos(test)];
        if enumerate {
            let x = self.pattern(pat);
            lits.push(Lit::Pos(atom_at("want", vec![typ, x], span)));
        }
        lits
    }

    /// The columns a membership's pattern takes: a key and a value, or
    /// the element.
    fn columns(&mut self, pat: PatternId) -> Vec<Term> {
        match &self.program.patterns[pat].kind {
            PatternKind::Tuple { elems, rest: None } if elems.len() == 2 => {
                elems.clone().into_iter().map(|p| self.pattern(p)).collect()
            }
            _ => vec![self.pattern(pat)],
        }
    }

    /// Whether `e` is a read the literal holding it tests or binds in
    /// place ([`Program::in_place`]).
    fn in_place(&self, e: ExprId) -> bool {
        self.program.in_place(e)
    }

    /// The read `e` tests or binds in place, `value` in its value column.
    fn in_place_read(&mut self, e: ExprId, value: Term) -> Lit {
        let span = self.program.exprs[e].span;
        Lit::Pos(self.read_holding(e, span, value).expect("a read"))
    }

    pub(super) fn atom(&mut self, rel: &RelRef, args: &RelArgs) -> Atom {
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

    pub(super) fn pattern(&mut self, id: PatternId) -> Term {
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
            k => unreachable!("no builder makes {k:?} before step 5"),
        }
    }
}

/// The relations a membership states facts of beside it.
const ENUM: &str = "__enum";
const NAMESPACE: &str = "__namespace";
const PROVIDER_TYPE: &str = "__provider_type";

fn atom_at(pred: &str, args: Vec<Term>, span: crate::ast::Span) -> Atom {
    Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span,
    }
}

fn compare(op: CmpOp, a: Term, b: Term) -> Lit {
    match op {
        CmpOp::Eq => Lit::Eq(a, b),
        CmpOp::Ne => Lit::Neq(a, b),
        CmpOp::Gt => Lit::Gt(a, b),
        CmpOp::Ge => Lit::Ge(a, b),
        CmpOp::Lt => Lit::Lt(a, b),
        CmpOp::Le => Lit::Le(a, b),
    }
}

/// `not l`: a read negated, a negation read, an equality a difference
/// (a reference column's type test, R-219).
fn negate(l: Lit) -> Lit {
    match l {
        Lit::Pos(a) => Lit::Not(a),
        Lit::Not(a) => Lit::Pos(a),
        Lit::Eq(a, b) => Lit::Neq(a, b),
        Lit::Neq(a, b) => Lit::Eq(a, b),
        other => other,
    }
}

/// Whether `l` reads any of `vars`.
fn reads_any(l: &Lit, vars: &BTreeSet<&str>) -> bool {
    if vars.is_empty() {
        return false;
    }
    let mut seen = BTreeSet::new();
    crate::syntax::resolve::lit_vars(l, &mut seen);
    seen.iter().any(|v| vars.contains(v.as_str()))
}
