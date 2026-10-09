//! The clause differential (R-211 step 4): while the resolver lowers a
//! body under the switch, each written literal it lowers is also built as
//! a goal ([`Builder::literal`]) and lowered back after the literals
//! before it in its clause ([`lower_goal`]), and each clause of a
//! statement as a clause of those goals ([`lower_clause`]), the two
//! compared as the resolver wrote them: every literal with the spans in
//! it, and the helper statements made (a `not { }`'s rule, a
//! membership's facts). A [`Shadow`] holds the goals built so far, in a
//! program of their own.

use super::{Spans, differ, dump, place, record};
use crate::ast::{self, Lit, Span, Stmt};
use crate::program::build::{Form, Written};
use crate::program::{Builder, GoalId, Program, lower_clause, lower_goal};
use std::fmt::Write as _;
use std::ops::Range;

/// The clause builders' state while the resolver lowers a statement: a
/// frame per literal (and clause) being lowered, innermost last.
pub struct Shadow {
    program: Program,
    frames: Vec<Frame>,
}

/// One literal's (or clause's) lowering in progress.
struct Frame {
    /// The goals of the literals lowered inside it: a clause's own, a
    /// `not`'s body's (and those of the comprehensions its terms hold).
    goals: Vec<GoalId>,
    /// Whether each literal lowered inside it was built.
    complete: bool,
    /// The value column of the read the literal tests in place.
    read: Option<usize>,
    /// The `__neg_N` it took, the statement's aggregate values then, and
    /// where in `goals` its body starts.
    negation: Option<(u32, Vec<String>, usize)>,
    /// The helper statements its terms made (a comprehension's `not { }`,
    /// a loader's extern), which no goal of its own makes.
    by_terms: Vec<Range<usize>>,
}

impl Default for Shadow {
    fn default() -> Self {
        Shadow {
            program: Program::new(),
            frames: Vec::new(),
        }
    }
}

impl Shadow {
    /// A literal's or a clause's lowering starts.
    pub fn open(&mut self) {
        self.frames.push(Frame {
            goals: Vec::new(),
            complete: true,
            read: None,
            negation: None,
            by_terms: Vec::new(),
        });
    }

    /// The literal being lowered tests or binds a read in place, its
    /// value in `column`.
    pub fn read(&mut self, column: usize) {
        if let Some(f) = self.frames.last_mut() {
            f.read = Some(column);
        }
    }

    /// How many goals the literal being lowered holds so far: where a
    /// `not`'s body starts.
    pub fn goals(&self) -> usize {
        self.frames.last().map_or(0, |f| f.goals.len())
    }

    /// The literal being lowered lowers through `__neg_{number}`, its
    /// body the goals from `from` on; `folded`, the statement's aggregate
    /// values.
    pub fn negation(&mut self, number: usize, folded: Vec<String>, from: usize) {
        if let Some(f) = self.frames.last_mut() {
            let number = u32::try_from(number).expect("a helper number fits");
            f.negation = Some((number, folded, from));
        }
    }

    /// A term made the helper statements `made`: no literal around it
    /// does.
    pub fn term(&mut self, made: Range<usize>) {
        if made.is_empty() {
            return;
        }
        for f in &mut self.frames {
            f.by_terms.push(made.clone());
        }
    }

    /// The literal being lowered failed: nothing to compare, and its
    /// clause is not built.
    pub fn failed(&mut self) {
        self.frames.pop();
        if let Some(f) = self.frames.last_mut() {
            f.complete = false;
        }
    }

    /// The written literal being lowered, `form` at `span`, lowered to
    /// `out[start..mid]` (`out[mid..]` the field reads after it) after the
    /// clause's `out[..start]`, and making `helpers[made..]`: built as a
    /// goal, lowered back and compared, the goal one of the frame around
    /// it.
    pub fn literal(
        &mut self,
        form: Option<&Form>,
        span: Span,
        out: &[Lit],
        (start, mid): (usize, usize),
        (helpers, made): (&[Stmt], usize),
        written: &dyn Fn(&str) -> bool,
    ) {
        let frame = self.frames.pop().expect("a literal's frame");
        let built = form.and_then(|form| {
            let own = frame.own(helpers, made);
            let negation = frame.negation.as_ref();
            let w = Written {
                form,
                span,
                lits: &out[start..mid],
                after: &out[mid..],
                helpers: &own,
                body: negation.map_or(&[], |(_, _, from)| &frame.goals[*from..]),
                read: frame.read,
                negation: negation.map(|(n, folded, _)| (*n, folded.as_slice())),
            };
            let goal = Builder::new(&mut self.program, span, written).literal(&w);
            self.compare(goal, &w, &out[..start], || {
                format!("the literal at {}: {form:?}", place(span))
            })
        });
        if let Some(f) = self.frames.last_mut() {
            match built {
                Some(g) => f.goals.push(g),
                None => f.complete = false,
            }
        }
    }

    /// A `set` target's `[_]` at `span` bound by `body[start..]` after the
    /// statement's `body[..start]`, making `helpers[made..]`: built as the
    /// membership goal it is, lowered back and compared.
    pub fn each(
        &mut self,
        span: Span,
        body: &[Lit],
        start: usize,
        (helpers, made): (&[Stmt], usize),
    ) {
        let frame = self.frames.pop().expect("a `[_]`'s frame");
        let own = frame.own(helpers, made);
        let form = Form::In { each: false };
        let w = Written {
            form: &form,
            span,
            lits: &body[start..],
            after: &[],
            helpers: &own,
            body: &[],
            read: None,
            negation: None,
        };
        let implicit = |v: &str| !v.starts_with("Each_");
        let goal = Builder::new(&mut self.program, span, &implicit).each(&w);
        self.compare(goal, &w, &body[..start], || {
            format!("the `[_]` at {}", place(span))
        });
    }

    /// `goal`, built of `w` (none: no builder knew its form), lowered after
    /// `context` and compared with what `w` lowered to; `what` names it.
    fn compare(
        &mut self,
        goal: Option<GoalId>,
        w: &Written,
        context: &[Lit],
        what: impl FnOnce() -> String,
    ) -> Option<GoalId> {
        let lowered: Vec<Lit> = w.lits.iter().chain(w.after).cloned().collect();
        let expected = text(&lowered, w.helpers);
        let (got, goal) = match goal {
            Some(g) => {
                let (lits, made) = lower_goal(&self.program, g, context);
                (text(&lits, &made), Some(g))
            }
            None => (String::new(), None),
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
            kinds_of(&self.program, g, &mut kinds);
        }
        record(d, |c| {
            c.literals += 1;
            c.built.extend(kinds);
        });
        goal
    }

    /// The clause being lowered, `out[seed..]` after the literals its
    /// statement holds before it, `out[..seed]`, making `helpers[made..]`:
    /// its literals' goals one clause, lowered back and compared.
    pub fn clause(
        &mut self,
        span: Span,
        out: &[Lit],
        seed: usize,
        (helpers, made): (&[Stmt], usize),
    ) {
        let frame = self.frames.pop().expect("a clause's frame");
        if !frame.complete {
            return;
        }
        let own = frame.own(helpers, made);
        let mut b = Builder::new(&mut self.program, span, &|_| true);
        let clause = b.clause_of(frame.goals);
        let (lits, made) = lower_clause(&self.program, clause, &out[..seed]);
        let d = differ(&text(&out[seed..], &own), &text(&lits, &made)).map(|d| super::Difference {
            statement: format!("the clause at {}", place(span)),
            ..d
        });
        record(d, |c| c.clauses += 1);
    }
}

impl Shadow {
    /// The rule `r` a statement lowered to at `span`, its aggregate
    /// bindings those binding `results`, which the resolver folded to
    /// `folded` taking numbers from `counters`: built as a rule item from
    /// the same counter, lowered and compared.
    pub fn fold(
        &mut self,
        r: &ast::RuleStmt,
        results: &[String],
        mut counters: crate::program::Counters,
        span: Span,
        folded: &[Stmt],
    ) {
        let any = |_: &str| true;
        let scope = self.program.scope;
        let mut b = Builder::new(&mut self.program, span, &any);
        let item = b.folded_rule((&r.head, &r.body), results, &mut counters, span, scope);
        let lowered = crate::program::lower_folded_rule(&self.program, item);
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
        } = &self.program.items[item].kind
        else {
            unreachable!("a folded rule has a body")
        };
        let grouped = self.program.clauses[*clause].goals.iter().any(|g| {
            matches!(
                self.program.goals[*g].kind,
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
}

impl Frame {
    /// The helper statements of `helpers[made..]` the frame's own goals
    /// made: not its terms'.
    fn own(&self, helpers: &[Stmt], made: usize) -> Vec<Stmt> {
        (made..helpers.len())
            .filter(|i| !self.by_terms.iter().any(|r| r.contains(i)))
            .map(|i| helpers[i].clone())
            .collect()
    }
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
