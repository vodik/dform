//! The gather's check (R-211 step 7c): a goal the resolver built of a
//! written literal is taken when it lowers, after the literals before it
//! in its clause ([`lower_goal`]), to what the resolver wrote of the
//! literal: every literal with the spans in it, and the helper
//! statements made (a `not { }`'s rule, a membership's facts, those its
//! terms made).

use super::{Spans, dump};
use crate::ast::{self, Lit, Stmt};
use crate::program::{GoalId, Program, lower_goal};
use std::fmt::Write as _;

/// Whether goal `goal` lowers after `context` to `lowered`, making the
/// helper statements `own` and of `made` no other: what the resolver
/// wrote of the literal it built it of (its terms' helpers some node of
/// it makes, some the literal's goal holds).
pub fn lowers_as(
    program: &Program,
    goal: GoalId,
    lowered: &[Lit],
    (own, made): (&[Stmt], &[Stmt]),
    context: &[Lit],
) -> bool {
    let (lits, got) = lower_goal(program, goal, context);
    let texts = |ss: &[Stmt]| -> Vec<String> { ss.iter().map(|s| format!("{s:?}")).collect() };
    let (got, own, made) = (texts(&got), texts(own), texts(made));
    text(&lits, &[]) == text(lowered, &[])
        && own.iter().all(|s| got.contains(s))
        && got.iter().all(|s| made.contains(s))
}

/// Literals and the statements made beside them, as one text: each
/// literal with the spans in it, then the statements as a lowering's.
fn text(lits: &[Lit], helpers: &[Stmt]) -> String {
    let mut out = String::new();
    for l in lits {
        let mut spans = Spans::default();
        spans.lits(std::slice::from_ref(l));
        let _ = writeln!(out, "lit {l:?}\n  spans {}", spans.text());
    }
    out.push_str(&dump(&Ok(ast::Program {
        statements: helpers.to_vec(),
        stack: None,
    })));
    out
}
