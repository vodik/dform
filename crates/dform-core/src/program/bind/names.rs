//! The names a negation or a comprehension writes: what it shares with the
//! clause around it, so it holds once the clause binds them.

use super::shape::coll_expr;
use super::{Binder, Inner};
use crate::program::node::*;
use std::collections::BTreeSet;

impl Binder<'_> {
    /// The source names written in clause `c`'s goals.
    pub(super) fn clause_names(&self, c: ClauseId, out: &mut BTreeSet<Name>) {
        for &g in &self.program.clauses[c].goals {
            self.goal_names(g, out);
        }
    }

    /// The names the comprehensions among `bodies` share with the clause.
    pub(super) fn shared(&self, bodies: &[Inner], out: &mut BTreeSet<Name>) {
        for b in bodies {
            if let Inner::Body(clause, items) = b {
                self.clause_names(*clause, out);
                for &i in items {
                    self.expr_names(i, out);
                }
            }
        }
    }

    pub(super) fn goal_names(&self, g: GoalId, out: &mut BTreeSet<Name>) {
        let mut exprs = Vec::new();
        let mut pats = Vec::new();
        match &self.program.goals[g].kind {
            GoalKind::Rel { args, .. } => match args {
                RelArgs::Positional(ps) => pats.extend(ps.iter().copied()),
                RelArgs::Record(fs) => pats.extend(fs.iter().map(|(_, p)| *p)),
            },
            GoalKind::Member { pat, coll } => {
                pats.push(*pat);
                exprs.extend(coll_expr(coll));
                if let Coll::TypeOf { of, .. } = coll {
                    exprs.push(*of);
                }
            }
            GoalKind::Bind { pat, value, .. } => {
                pats.push(*pat);
                exprs.push(*value);
            }
            GoalKind::Compare { lhs, ops } => {
                exprs.push(*lhs);
                exprs.extend(ops.iter().map(|(_, x)| *x));
            }
            GoalKind::Has(Has::Resource { typ, addr }) => exprs.extend([*typ, *addr]),
            GoalKind::Has(Has::Read(e) | Has::Walk { value: e, .. }) | GoalKind::Truth(e) => {
                exprs.push(*e)
            }
            GoalKind::Not { clause, .. } => self.clause_names(*clause, out),
            GoalKind::Fold { var, agg, .. } => {
                self.var_name(*var, out);
                exprs.push(*agg);
            }
            GoalKind::Group { goals, after } => {
                for &g in goals.iter().chain(after) {
                    self.goal_names(g, out);
                }
            }
            GoalKind::Marked { goal, .. } => self.goal_names(*goal, out),
        }
        for p in pats {
            self.pattern_names(p, out);
        }
        for e in exprs {
            self.expr_names(e, out);
        }
    }

    fn var_name(&self, v: VarId, out: &mut BTreeSet<Name>) {
        let v = &self.program.vars[v];
        if !v.implicit {
            out.insert(v.name.clone());
        }
    }

    pub(super) fn pattern_names(&self, p: PatternId, out: &mut BTreeSet<Name>) {
        let (parts, rest) = match &self.program.patterns[p].kind {
            PatternKind::Hole => return,
            PatternKind::Bind(v) => return self.var_name(*v, out),
            PatternKind::Expr(e) => return self.expr_names(*e, out),
            PatternKind::Tuple { elems, rest } => (elems.clone(), *rest),
            PatternKind::Object { fields, rest } => {
                (fields.iter().map(|(_, _, x)| *x).collect(), *rest)
            }
        };
        for x in parts {
            self.pattern_names(x, out);
        }
        if let Some(v) = rest {
            self.var_name(v, out);
        }
    }

    /// The source names term `e` writes, a comprehension's included.
    pub(super) fn expr_names(&self, e: ExprId, out: &mut BTreeSet<Name>) {
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Var(v) => self.var_name(*v, out),
            ExprKind::Comprehension { item, clause } => {
                self.expr_names(*item, out);
                self.clause_names(*clause, out);
            }
            _ => {
                for c in x.kind.children() {
                    self.expr_names(c, out);
                }
            }
        }
    }
}
