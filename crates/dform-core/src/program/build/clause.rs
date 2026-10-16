//! A clause's goals (R-211 steps 4, 7c). The resolver builds each written
//! literal's goal of the nodes of its terms (`syntax::resolve`); built
//! here are the goals of a kind several literals make, and the clauses of
//! goals.

use super::Builder;
use crate::ast::Span;
use crate::program::node::*;

impl Builder<'_> {
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
