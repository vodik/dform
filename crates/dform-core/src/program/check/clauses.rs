//! The clause differential (R-211 step 4): each written literal the
//! resolver lowers is built as a goal (`build::Gather`) and lowered back
//! after the literals before it in its clause ([`lower_goal`]), and each
//! clause of a statement as a clause of those goals ([`lower_clause`]),
//! the two compared as the resolver wrote them: every literal with the
//! spans in it, and the helper statements made (a `not { }`'s rule, a
//! membership's facts, those its terms made).

use super::{Spans, differ, dump, place, record};
use crate::ast::{self, Lit, Span, Stmt};
use crate::program::{Builder, ClauseId, GoalId, Program, lower_clause, lower_goal};
use std::fmt::Write as _;

/// `goal`, built of a literal the resolver lowered to `lowered`, making
/// `helpers` (none: no builder knew its form), lowered after `context`
/// and compared; `what` names it.
pub fn literal(
    program: &Program,
    goal: Option<GoalId>,
    lowered: &[Lit],
    helpers: &[Stmt],
    context: &[Lit],
    what: impl FnOnce() -> String,
) {
    let expected = text(lowered, helpers);
    let got = match goal {
        Some(g) => {
            let (lits, made) = lower_goal(program, g, context);
            text(&lits, &made)
        }
        None => String::new(),
    };
    let d = differ(&expected, &got).map(|d| super::Difference {
        statement: match goal {
            Some(_) => what(),
            None => format!("{}: no goal is built of it", what()),
        },
        ..d
    });
    let mut kinds = Vec::new();
    if let Some(g) = goal {
        kinds_of(program, g, &mut kinds);
    }
    record(d, |c| {
        c.literals += 1;
        c.built.extend(kinds);
    });
}

/// The clause `clause` at `span`, built of a statement's body the
/// resolver lowered to `lits` after `context` (what its statement holds
/// before it), making `helpers`: lowered back and compared.
pub fn clause(
    program: &Program,
    clause: ClauseId,
    span: Span,
    context: &[Lit],
    lits: &[Lit],
    helpers: &[Stmt],
) {
    let (got, made) = lower_clause(program, clause, context);
    let d = differ(&text(lits, helpers), &text(&got, &made)).map(|d| super::Difference {
        statement: format!("the clause at {}", place(span)),
        ..d
    });
    record(d, |c| c.clauses += 1);
}

/// The rule `r` a statement lowered to at `span`, its aggregate bindings
/// those binding `results`, which the resolver folded to `folded` taking
/// numbers from `counters`: built as a rule item from the same counter,
/// lowered and compared.
pub fn fold(
    r: &ast::RuleStmt,
    results: &[String],
    mut counters: crate::program::Counters,
    span: Span,
    folded: &[Stmt],
) {
    let mut program = Program::new();
    let any = |_: &str| true;
    let scope = program.scope;
    let mut b = Builder::new(&mut program, span, &any);
    let item = b.folded_rule((&r.head, &r.body), results, &mut counters, span, scope);
    let lowered = crate::program::lower_item(&program, item);
    let text = |stmts: &[Stmt]| {
        dump(&Ok(ast::Program {
            statements: stmts.to_vec(),
            stack: None,
        }))
    };
    let d = differ(&text(folded), &text(&lowered)).map(|d| super::Difference {
        statement: format!("the folded rule at {}", place(span)),
        ..d
    });
    use crate::program::node::{GoalKind, ItemKind};
    let ItemKind::Rule {
        clause: Some(clause),
        ..
    } = &program.items[item].kind
    else {
        unreachable!("a folded rule has a body")
    };
    let grouped = program.clauses[*clause].goals.iter().any(|g| {
        matches!(
            program.goals[*g].kind,
            GoalKind::Fold {
                helper: Some(_),
                ..
            }
        )
    });
    let kind = match grouped {
        true => "Rule/grouped",
        false => "Rule/head",
    };
    record(d, |c| {
        c.folds += 1;
        c.built.insert(kind.into());
    });
}

/// The kinds of goal `g` is and holds: `Has/Read`, `Member/Enum`,
/// `Not/helper`, ..
fn kinds_of(p: &Program, g: GoalId, out: &mut Vec<String>) {
    use crate::program::node::{Coll, GoalKind, Has};
    let kind = match &p.goals[g].kind {
        GoalKind::Rel { .. } => "Rel",
        GoalKind::Member { coll, .. } => match coll {
            Coll::Expr(_) => "Member/Expr",
            Coll::Type(_) => "Member/Type",
            Coll::Namespace { .. } => "Member/Namespace",
            Coll::ProviderType { .. } => "Member/ProviderType",
            Coll::World(_) => "Member/World",
            Coll::Enum { .. } => "Member/Enum",
            Coll::Copies { .. } => "Member/Copies",
            Coll::Each(_) => "Member/Each",
            Coll::TypeOf(_) => "Member/TypeOf",
        },
        GoalKind::Bind { value, .. } => match p.exprs[*value].kind {
            crate::program::node::ExprKind::Read { .. } => "Bind/Read",
            crate::program::node::ExprKind::Field { .. } => "Bind/Field",
            _ => "Bind",
        },
        GoalKind::Compare { lhs, ops } => match (&p.exprs[*lhs].kind, ops.len()) {
            (crate::program::node::ExprKind::Read { .. }, _) => "Compare/Read",
            (_, 1) => "Compare",
            _ => "Compare/chain",
        },
        GoalKind::Has(h) => match h {
            Has::Resource { .. } => "Has/Resource",
            Has::Read(_) => "Has/Read",
            Has::Walk { .. } => "Has/Walk",
        },
        GoalKind::Truth(e) => match p.exprs[*e].kind {
            crate::program::node::ExprKind::Read { .. } => "Truth/Read",
            _ => "Truth",
        },
        GoalKind::Not { clause, helper } => {
            for &g in &p.clauses[*clause].goals {
                kinds_of(p, g, out);
            }
            match helper {
                Some(_) => "Not/helper",
                None => "Not",
            }
        }
        GoalKind::Fold { .. } => "Fold",
        GoalKind::Hoisted { goals, .. } => {
            goals.iter().for_each(|g| kinds_of(p, *g, out));
            return;
        }
        GoalKind::Marked { goal, .. } => {
            kinds_of(p, *goal, out);
            "Marked"
        }
    };
    out.push(kind.to_string());
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
