//! A body the resolver lowered, as goals (R-211 step 3): a
//! comprehension's, and the reads a term hoisted. Each literal is the
//! goal it lowers from, at its atom's span: an atom a relation's goal, a
//! negated one `not` of it, `=` a binding, any other comparison a
//! comparison. Step 4 builds clauses from the tree.

use super::Builder;
use crate::ast::{Atom, Lit};
use crate::program::node::*;

impl Builder<'_> {
    /// The lowered body `lits`.
    pub(super) fn clause(&mut self, lits: &[Lit]) -> ClauseId {
        let goals = lits.iter().map(|l| self.goal(l)).collect();
        self.program.clauses.insert(Clause {
            span: self.span,
            goals,
        })
    }

    /// The lowered literal `l`.
    pub(super) fn goal(&mut self, l: &Lit) -> GoalId {
        let (lhs, op, rhs) = match l {
            Lit::Pos(a) => return self.rel(a),
            Lit::Not(a) => {
                let g = self.rel(a);
                let clause = self.program.clauses.insert(Clause {
                    span: a.span,
                    goals: vec![g],
                });
                return self.goal_node(
                    a.span,
                    GoalKind::Not {
                        clause,
                        helper: None,
                    },
                );
            }
            Lit::Eq(a, b) => {
                let kind = GoalKind::Bind {
                    pat: self.pattern(a),
                    value: self.expr(b),
                };
                return self.goal_node(self.span, kind);
            }
            Lit::Neq(a, b) => (a, CmpOp::Ne, b),
            Lit::Gt(a, b) => (a, CmpOp::Gt, b),
            Lit::Ge(a, b) => (a, CmpOp::Ge, b),
            Lit::Lt(a, b) => (a, CmpOp::Lt, b),
            Lit::Le(a, b) => (a, CmpOp::Le, b),
        };
        let kind = GoalKind::Compare {
            lhs: self.expr(lhs),
            ops: vec![(op, self.expr(rhs))],
        };
        self.goal_node(self.span, kind)
    }

    /// The atom `a`: the relation's goal, its columns by position or by
    /// name.
    fn rel(&mut self, a: &Atom) -> GoalId {
        self.at(a.span, |b| {
            let args = match &a.record {
                Some(r) => {
                    debug_assert!(a.args.is_empty(), "a record atom has no positional column");
                    RelArgs::Record(r.iter().map(|(k, t)| (k.clone(), b.pattern(t))).collect())
                }
                None => RelArgs::Positional(a.args.iter().map(|t| b.pattern(t)).collect()),
            };
            let rel = RelRef {
                name: a.pred.clone(),
                span: a.span,
            };
            b.goal_node(a.span, GoalKind::Rel { rel, args })
        })
    }

    fn goal_node(&mut self, span: crate::ast::Span, kind: GoalKind) -> GoalId {
        self.program.goals.insert(Goal { span, kind })
    }
}
