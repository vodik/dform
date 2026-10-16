//! A clause's goals (R-211 steps 4, 7c). The resolver builds each written
//! literal's goal of the nodes of its terms (`syntax::resolve`); built
//! here of what it lowered to are only the goals of literals it wrote
//! itself, a comprehension's body outside a statement
//! ([`Builder::goal`]), and the clauses of goals.

use super::Builder;
use crate::ast::{Atom, Lit, Span, Term};
use crate::program::node::*;

impl Builder<'_> {
    /// The lowered body `lits`.
    pub(super) fn clause(&mut self, lits: &[Lit]) -> ClauseId {
        let goals = lits.iter().map(|l| self.goal(l)).collect();
        self.clause_of(goals)
    }

    /// `clause` (a new one when none) with `goal` its last.
    pub fn ending(&mut self, clause: Option<ClauseId>, goal: GoalId) -> ClauseId {
        match clause {
            Some(c) => {
                self.program.clauses[c].goals.push(goal);
                c
            }
            None => self.clause_of(vec![goal]),
        }
    }

    /// The clause of `goals`.
    pub fn clause_of(&mut self, goals: Vec<GoalId>) -> ClauseId {
        self.program.clauses.insert(Clause {
            span: self.span,
            goals,
        })
    }

    /// The lowered literal `l`, as the goal it lowers from: an atom a
    /// relation's goal, a negated one `not` of it, `=` a binding, any
    /// other comparison a comparison.
    pub(super) fn goal(&mut self, l: &Lit) -> GoalId {
        let (op, lhs, rhs) = match l {
            Lit::Pos(a) => return self.rel(a),
            Lit::Not(a) => {
                let g = self.rel(a);
                let clause = self.at(a.span, |b| b.clause_of(vec![g]));
                let kind = GoalKind::Not {
                    clause,
                    helper: None,
                    written: Negated::Literal,
                };
                return self.goal_node(a.span, kind);
            }
            Lit::Eq(a, b) => {
                let (pat, value) = (self.pattern(a), self.expr(b));
                return self.bind_goal(self.span, pat, value);
            }
            l => comparison(l).expect("not an atom"),
        };
        let kind = GoalKind::Compare {
            lhs: self.expr(lhs),
            ops: vec![(op, self.expr(rhs))],
        };
        self.goal_node(self.span, kind)
    }

    /// The atom `a`: the relation's goal, its columns by position or by
    /// name.
    pub(super) fn rel(&mut self, a: &Atom) -> GoalId {
        self.at(a.span, |b| {
            let args = b.args(a);
            let rel = RelRef::written(a.pred.clone(), a.span);
            b.goal_node(a.span, GoalKind::Rel { rel, args })
        })
    }

    /// The columns of `a`, by position or by name (a record's in the
    /// order its reads were hoisted).
    pub(super) fn args(&mut self, a: &Atom) -> RelArgs {
        match &a.record {
            Some(r) => {
                debug_assert!(a.args.is_empty(), "a record atom has no positional column");
                let cols = self.in_read_order(r);
                RelArgs::Record(
                    cols.into_iter()
                        .map(|(k, t)| (k.clone(), self.pattern(t)))
                        .collect(),
                )
            }
            None => RelArgs::Positional(a.args.iter().map(|t| self.pattern(t)).collect()),
        }
    }

    pub fn goal_node(&mut self, span: Span, kind: GoalKind) -> GoalId {
        self.program.goals.insert(Goal { span, kind })
    }

    /// `pat = value` at `span`, with where its `=` stands.
    pub fn bind_goal(&mut self, span: Span, pat: PatternId, value: ExprId) -> GoalId {
        let ps = self.program.patterns[pat].span;
        let vs = self.program.exprs[value].span;
        let eq = eq_between(span, if ps.start <= vs.start { vs } else { ps });
        self.goal_node(span, GoalKind::Bind { pat, value, eq })
    }

    /// `var = agg` at `span`, the aggregate's binding, with where its `=`
    /// stands.
    pub fn fold_goal(&mut self, span: Span, var: VarId, agg: ExprId) -> GoalId {
        let eq = eq_between(span, self.program.exprs[agg].span);
        let kind = GoalKind::Fold {
            var,
            agg,
            helper: None,
            eq,
        };
        self.goal_node(span, kind)
    }
}

/// Where `=` stands in the goal written at `span`, between its sides:
/// before the side at `other` when that side is written last, else after
/// it; none where the goal writes no `=` (an object pattern's field).
fn eq_between(span: Span, other: Span) -> Option<Span> {
    let (_, text) = crate::diag::source_of(span)?;
    let (from, to) = match other.start > span.start {
        true => (span.start as usize, other.start as usize),
        false => (other.end as usize, span.end as usize),
    };
    let bytes = text.get(from..to)?.as_bytes();
    let at = (0..bytes.len()).find(|&i| {
        let prev = i.checked_sub(1).map(|j| bytes[j]);
        bytes[i] == b'='
            && !matches!(prev, Some(b'=' | b'!' | b'<' | b'>'))
            && bytes.get(i + 1) != Some(&b'=')
    })?;
    let at = (from + at) as u32;
    Some(Span {
        start: at,
        end: at + 1,
        ..span
    })
}

/// A comparison's operator and sides.
pub(super) fn comparison(l: &Lit) -> Option<(CmpOp, &Term, &Term)> {
    Some(match l {
        Lit::Eq(a, b) => (CmpOp::Eq, a, b),
        Lit::Neq(a, b) => (CmpOp::Ne, a, b),
        Lit::Gt(a, b) => (CmpOp::Gt, a, b),
        Lit::Ge(a, b) => (CmpOp::Ge, a, b),
        Lit::Lt(a, b) => (CmpOp::Lt, a, b),
        Lit::Le(a, b) => (CmpOp::Le, a, b),
        Lit::Pos(_) | Lit::Not(_) => return None,
    })
}
