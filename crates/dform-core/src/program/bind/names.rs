//! The names a negation or a comprehension writes: what it shares with the
//! clause around it, so it holds once the clause binds them; and the
//! variables the program wrote under a node, each once.

use super::shape::coll_expr;
use super::{Binder, Inner};
use crate::program::node::*;
use std::collections::BTreeSet;

impl Binder<'_> {
    /// The source names written in clause `c`'s goals.
    pub(super) fn clause_names(&self, c: ClauseId, out: &mut BTreeSet<Name>) {
        let mut vars = BTreeSet::new();
        self.clause_vars(c, &mut vars);
        out.extend(self.names(&vars));
    }

    /// The names of `vars`.
    pub(super) fn names(&self, vars: &BTreeSet<VarId>) -> Vec<Name> {
        vars.iter()
            .map(|&v| self.program.vars[v].name.clone())
            .collect()
    }

    /// The variables the program wrote in clause `c`'s goals.
    pub(super) fn clause_vars(&self, c: ClauseId, out: &mut BTreeSet<VarId>) {
        for &g in &self.program.clauses[c].goals {
            self.goal_vars(g, out);
        }
    }

    /// The names the comprehensions among `bodies` share with the clause.
    pub(super) fn shared(&self, bodies: &[Inner], out: &mut BTreeSet<Name>) {
        for b in bodies {
            if let Inner::Body(clause, items) = b {
                let mut vars = BTreeSet::new();
                self.clause_vars(*clause, &mut vars);
                for &i in items {
                    self.expr_vars(i, &mut vars);
                }
                out.extend(self.names(&vars));
            }
        }
    }

    pub(super) fn goal_vars(&self, g: GoalId, out: &mut BTreeSet<VarId>) {
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
                // The type a membership ranges a variable over is the
                // compiler's variable.
                if self.implicit {
                    exprs.extend(type_expr(coll));
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
            GoalKind::Not { clause, .. } => self.clause_vars(*clause, out),
            GoalKind::Fold { var, agg, .. } => {
                self.var(*var, out);
                exprs.push(*agg);
            }
            GoalKind::Group { goals, after } => {
                for &g in goals.iter().chain(after) {
                    self.goal_vars(g, out);
                }
            }
            GoalKind::Marked { goal, .. } => self.goal_vars(*goal, out),
        }
        for p in pats {
            self.pattern_vars(p, out);
        }
        for e in exprs {
            self.expr_vars(e, out);
        }
    }

    fn var(&self, v: VarId, out: &mut BTreeSet<VarId>) {
        if self.counts(&self.program.vars[v]) {
            out.insert(v);
        }
    }

    pub(super) fn pattern_vars(&self, p: PatternId, out: &mut BTreeSet<VarId>) {
        let (parts, rest) = match &self.program.patterns[p].kind {
            PatternKind::Hole => return,
            PatternKind::Bind(v) => return self.var(*v, out),
            PatternKind::Expr(e) => return self.expr_vars(*e, out),
            PatternKind::Tuple { elems, rest } => (elems.clone(), *rest),
            PatternKind::Object { fields, rest } => {
                (fields.iter().map(|(_, _, x)| *x).collect(), *rest)
            }
        };
        for x in parts {
            self.pattern_vars(x, out);
        }
        if let Some(v) = rest {
            self.var(v, out);
        }
    }

    /// The variables term `e` writes, a comprehension's included.
    pub(super) fn expr_vars(&self, e: ExprId, out: &mut BTreeSet<VarId>) {
        let x = &self.program.exprs[e];
        match &x.kind {
            ExprKind::Var(v) => self.var(*v, out),
            ExprKind::Comprehension { item, clause } => {
                self.expr_vars(*item, out);
                self.clause_vars(*clause, out);
            }
            _ => {
                for c in x.kind.children() {
                    self.expr_vars(c, out);
                }
            }
        }
    }
}

/// The term naming the type a membership ranges over, when it is one.
pub(super) fn type_expr(c: &Coll) -> Option<ExprId> {
    match c {
        Coll::Type(e)
        | Coll::Namespace { typ: e, .. }
        | Coll::ProviderType { var: e, .. }
        | Coll::TypeOf { ty: e, .. } => Some(*e),
        Coll::Expr(_)
        | Coll::Each(_)
        | Coll::World(_)
        | Coll::Enum { .. }
        | Coll::Copies { .. } => None,
    }
}
