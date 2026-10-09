//! Aggregates (R-211 step 4, docs/grammar.md "Aggregates"): `n =
//! count(x)` in a body is a `Fold` goal, its call an `Aggregate`; a rule
//! whose body holds folds is a `Rule` item whose lowering folds it as the
//! resolver's `fold_rule` does. Which of the two shapes it takes is
//! decided here, so `lower` decides nothing: one aggregate whose value is
//! a column of the head and read by nothing else is applied by the head
//! (its `Fold` has no helper); otherwise each fold is a rule
//! `__agg_N(group, agg(x)) :- B` over the group (the head's other
//! variables and what is read after the fold), N stamped on its `Fold`
//! from the program's counter.

use super::Builder;
use crate::ast::{Atom, Lit, Span, Term};
use crate::program::Counters;
use crate::program::node::*;
use std::collections::BTreeSet;

impl Builder<'_> {
    /// `n = agg(x)`: the binding of `n` to the fold.
    pub(super) fn fold(&mut self, l: &Lit) -> Option<GoalId> {
        let Lit::Eq(Term::Var(v), Term::Func { name, args }) = l else {
            return None;
        };
        let ([x], Some(kind)) = (args.as_slice(), aggregate_kind(name)) else {
            return None;
        };
        let item = self.expr(x);
        let agg = self.expr_node(ExprKind::Aggregate { kind, item });
        let var = self.var(v);
        let kind = GoalKind::Fold {
            var,
            agg,
            helper: None,
        };
        Some(self.goal_node(self.span, kind))
    }

    /// The rule `head :- body` a statement lowered to, at `span`, its
    /// aggregate bindings those binding `results`: a `Rule` item, each
    /// fold's `__agg_N` taken from `helpers` when the head does not apply
    /// it.
    pub fn folded_rule(
        &mut self,
        (head, body): (&Atom, &[Lit]),
        results: &[String],
        helpers: &mut Counters,
        span: Span,
        scope: crate::program::ScopeId,
    ) -> ItemId {
        let found = |l: &Lit| {
            matches!(l, Lit::Eq(Term::Var(v), Term::Func { name, .. })
            if results.contains(v) && aggregate_kind(name).is_some())
        };
        let folds = body.iter().filter(|l| found(l)).count();
        let grouped = !(folds == 1 && head.record.is_none() && {
            let reads: BTreeSet<&str> = results.iter().map(String::as_str).collect();
            let post = body.iter().any(|l| !found(l) && reads_any(l, &reads));
            let v = body.iter().find(|l| found(l)).and_then(|l| match l {
                Lit::Eq(Term::Var(v), _) => Some(v),
                _ => None,
            });
            !post && v.is_some_and(|v| in_head_once(head, v))
        });
        let goals = body
            .iter()
            .map(|l| match found(l) {
                true => {
                    let g = self.fold(l).expect("an aggregate's binding");
                    if grouped
                        && let GoalKind::Fold { helper, .. } = &mut self.program.goals[g].kind
                    {
                        *helper = Some(u32::try_from(helpers.agg()).expect("a helper number"));
                    }
                    g
                }
                false => self.goal(l),
            })
            .collect();
        let clause = self.clause_of(goals);
        let rel = RelRef {
            name: head.pred.clone(),
            span: head.span,
        };
        let args = match &head.record {
            Some(r) => RelArgs::Record(
                r.iter()
                    .map(|(k, t)| (k.clone(), self.pattern(t)))
                    .collect(),
            ),
            None => RelArgs::Positional(head.args.iter().map(|t| self.pattern(t)).collect()),
        };
        self.program.items.insert(Item {
            span,
            scope,
            kind: ItemKind::Rule {
                head: Head { rel, args },
                clause: Some(clause),
                rank: None,
            },
        })
    }
}

/// Whether `v` is one column of `head` and in no other.
fn in_head_once(head: &Atom, v: &str) -> bool {
    let at = head
        .args
        .iter()
        .filter(|t| matches!(t, Term::Var(x) if x == v))
        .count();
    let mut elsewhere = BTreeSet::new();
    for t in head
        .args
        .iter()
        .filter(|t| !matches!(t, Term::Var(x) if x == v))
    {
        crate::syntax::resolve::lit_vars(&Lit::Eq(t.clone(), t.clone()), &mut elsewhere);
    }
    at == 1 && !elsewhere.contains(v)
}

/// Whether `l` reads any of `vars`.
fn reads_any(l: &Lit, vars: &BTreeSet<&str>) -> bool {
    let mut seen = BTreeSet::new();
    crate::syntax::resolve::lit_vars(l, &mut seen);
    seen.iter().any(|v| vars.contains(v.as_str()))
}
