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

use crate::program::Program;
use crate::program::bind::{self, Term};
use crate::program::node::*;

/// A column of a head, as built: a term, or a variable the statement
/// makes (a parameter, a document's leaf).
#[derive(Clone, Copy)]
pub enum Column {
    Term(Term),
    Var(VarId),
}

/// Whether the aggregate bindings of `clause` fold through a helper
/// each: not one alone whose value is one of `head`'s columns and read by
/// nothing else, which the head applies. `head` is none where a head
/// applies no aggregate (one written by column name, a list element's
/// write).
pub fn grouped(program: &Program, clause: ClauseId, head: Option<&[Column]>) -> bool {
    let folds = folds(program, clause);
    let ([fold], Some(head)) = (folds.as_slice(), head) else {
        return !folds.is_empty();
    };
    let GoalKind::Fold { var, .. } = program.goals[*fold].kind else {
        unreachable!("a fold")
    };
    let v = &program.vars[var];
    let name = (!v.implicit).then_some(&v.name);
    // What the clause reads of the value after the fold.
    let post = program.clauses[clause].goals.iter().any(|&g| {
        let names = match &program.goals[g].kind {
            GoalKind::Group { goals, after } if goals == &[*fold] => after
                .iter()
                .flat_map(|&a| bind::goal_names(program, a))
                .collect(),
            _ if g == *fold => return false,
            _ => bind::goal_names(program, g),
        };
        name.is_some_and(|n| names.contains(n))
    });
    post || !in_head_once(program, head, var)
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
    program.clauses[clause]
        .goals
        .iter()
        .filter_map(|&g| fold_of(program, g))
        .collect()
}

/// The aggregate binding goal `g` is: a fold, or a group of one.
fn fold_of(program: &Program, g: GoalId) -> Option<GoalId> {
    match &program.goals[g].kind {
        GoalKind::Fold { .. } => Some(g),
        GoalKind::Group { goals, .. }
            if let [f] = goals.as_slice()
                && matches!(program.goals[*f].kind, GoalKind::Fold { .. }) =>
        {
            Some(*f)
        }
        _ => None,
    }
}

/// Whether `var` is one column of `head`, alone, and in no other.
fn in_head_once(program: &Program, head: &[Column], var: VarId) -> bool {
    let alone = |c: &Column| match *c {
        Column::Var(v) => v == var,
        Column::Term(t) => bind::alone(program, t) == Some(var),
    };
    let v = &program.vars[var];
    let elsewhere = head.iter().filter(|c| !alone(c)).any(|c| match *c {
        Column::Var(w) => w == var,
        Column::Term(t) => !v.implicit && bind::term_names(program, t).contains(&v.name),
    });
    head.iter().filter(|c| alone(c)).count() == 1 && !elsewhere
}
