//! `not L` and `not { B }` as a `Not` goal (R-211 step 4): a `not` one
//! literal says is that literal's goal with its last literal negated (an
//! atom, a membership, a read tested in place: `not R.ready`, `not has
//! R.p`, `not R.p == c`); any other is the helper `__neg_N(ȳ) :- P, B`
//! the resolver's `neg_helper` writes, its body the goals of B built as
//! it lowered them, N the number it took (`Counters::neg`, stamped on the
//! node here), ȳ the variables B shares with what binds before it.

use super::Builder;
use super::clause::{Form, Written, split_mark};
use crate::ast::{Lit, Term};
use crate::program::node::*;
use crate::value::Value;

impl Builder<'_> {
    /// `not L` (`inner`, the form of L) or `not { B }` (`None`).
    pub(super) fn negation(&mut self, w: &Written, inner: Option<&Form>) -> Option<GoalId> {
        if let Some((number, folded)) = w.negation {
            return self.through_helper(w, number, folded);
        }
        match inner? {
            Form::Atom => {
                let (Lit::Not(a), reads) = w.lits.split_last()? else {
                    return None;
                };
                let rel = self.rel(a);
                let not = self.not_one(rel);
                Some(self.hoisted_goal(reads, vec![not], w.after))
            }
            Form::In { each } => self.membership(w, *each, true),
            form @ (Form::Truth | Form::Has | Form::Compare { .. }) => self.not_in_place(w, form),
            _ => None,
        }
    }

    /// `not R.ready`, `not has R.p`, `not R.p == c`: the read itself,
    /// negated, after the reads that name it; a `has` marked.
    fn not_in_place(&mut self, w: &Written, form: &Form) -> Option<GoalId> {
        let (outer, mark, lits) = split_mark(w.lits);
        let (Lit::Not(a), reads) = lits.split_last()? else {
            return None;
        };
        let column = w.read?;
        let value = a.args.get(column)?;
        let read = self.at(a.span, |b| b.read(a, column));
        let kind = match form {
            Form::Truth if *value == Term::Val(Value::Bool(true)) => GoalKind::Truth(read),
            Form::Has if *value == Term::Wildcard => GoalKind::Has(Has::Read(read)),
            Form::Compare { .. } => GoalKind::Compare {
                lhs: read,
                ops: vec![(CmpOp::Eq, self.expr(value))],
            },
            _ => return None,
        };
        let goal = self.goal_node(a.span, kind);
        let not = self.not_one(goal);
        let not = self.hoisted_goal(reads, vec![not], &[]);
        let marked = match mark {
            Some(m) => self.marked(m, not)?,
            None => not,
        };
        Some(self.hoisted_goal(outer, vec![marked], w.after))
    }

    /// `not { B }` (or a `not` no literal says) through `__neg_{number}`:
    /// `not __neg_N(ȳ)`, B the body's goals; a `not has` of a walk marked.
    fn through_helper(&mut self, w: &Written, number: u32, folded: &[String]) -> Option<GoalId> {
        let (outer, mark, lits) = split_mark(w.lits);
        let [Lit::Not(head)] = lits else {
            return None;
        };
        if head.pred != format!("__neg_{number}") {
            return None;
        }
        let args = head
            .args
            .iter()
            .map(|t| match t {
                Term::Var(v) => Some(self.var(v)),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let folded = folded.iter().map(|v| self.var(v)).collect();
        let clause = self.at(head.span, |b| b.clause_of(w.body.to_vec()));
        let helper = NegHelper {
            number,
            args,
            folded,
        };
        let not = self.goal_node(
            head.span,
            GoalKind::Not {
                clause,
                helper: Some(helper),
            },
        );
        let marked = match mark {
            Some(m) => self.marked(m, not)?,
            None => not,
        };
        Some(self.hoisted_goal(outer, vec![marked], w.after))
    }
}
