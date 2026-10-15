//! Aggregates (R-211 step 4, docs/grammar.md "Aggregates"): `n =
//! count(x)` in a body is a `Fold` goal, its call an `Aggregate`; a rule
//! whose body holds folds is a `Rule` item whose lowering folds it (the
//! table in syntax/resolve/aggregate.rs). Which of the two shapes it
//! takes is decided here, so `lower` decides nothing: one aggregate whose
//! value is a column of the head and read by nothing else is applied by
//! the head (its `Fold` has no helper); otherwise each fold is a rule
//! `__agg_N(group, agg(x)) :- B` over the group (the head's other
//! variables and what is read after the fold), N stamped on its `Fold`
//! from the program's counter.

use crate::ast::{Atom, Lit, Term};
use crate::program::Program;
use crate::program::node::*;
use std::collections::BTreeSet;

/// Whether the aggregate bindings of `body` (those binding `results`)
/// fold through a helper each: not one alone whose value is a column of
/// `head` that nothing else reads, which the head applies.
pub fn grouped(head: &Atom, body: &[Lit], results: &[String]) -> bool {
    let found = |l: &Lit| {
        matches!(l, Lit::Eq(Term::Var(v), Term::Func { name, .. })
        if results.contains(v) && aggregate_kind(name).is_some())
    };
    let folds = body.iter().filter(|l| found(l)).count();
    !(folds == 1 && head.record.is_none() && {
        let reads: BTreeSet<&str> = results.iter().map(String::as_str).collect();
        let post = body.iter().any(|l| !found(l) && reads_any(l, &reads));
        let v = body.iter().find(|l| found(l)).and_then(|l| match l {
            Lit::Eq(Term::Var(v), _) => Some(v),
            _ => None,
        });
        !post && v.is_some_and(|v| in_head_once(head, v))
    })
}

/// Each aggregate binding of `clause`, in order, folds through the next
/// `__agg_N` of the program's counter.
pub fn number_folds(program: &mut Program, clause: ClauseId) {
    for fold in folds(program, clause) {
        let n = u32::try_from(program.helpers.agg()).expect("a helper number");
        if let GoalKind::Fold { helper, .. } = &mut program.goals[fold].kind {
            *helper = Some(n);
        }
    }
}

/// The next `__agg_N` of the program's counter for each aggregate
/// binding of `clause`, in order, taken: one statement's of several
/// sharing the clause (a `set { .. }`'s lines).
pub fn fold_numbers(program: &mut Program, clause: ClauseId) -> Vec<u32> {
    let n = folds(program, clause).len();
    (0..n)
        .map(|_| u32::try_from(program.helpers.agg()).expect("a helper number"))
        .collect()
}

/// The aggregate bindings of `clause`, in order.
fn folds(program: &Program, clause: ClauseId) -> Vec<GoalId> {
    let fold = |g: GoalId| match &program.goals[g].kind {
        GoalKind::Fold { .. } => Some(g),
        GoalKind::Group { goals, .. }
            if let [f] = goals.as_slice()
                && matches!(program.goals[*f].kind, GoalKind::Fold { .. }) =>
        {
            Some(*f)
        }
        _ => None,
    };
    program.clauses[clause]
        .goals
        .iter()
        .filter_map(|&g| fold(g))
        .collect()
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
