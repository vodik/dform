//! The goals of the statement being lowered (R-211 steps 4-5): while the
//! resolver lowers a statement's body, each written literal it lowers is
//! built as a goal ([`Builder::literal`]) from what it lowered to, and
//! the body as a clause of those goals, in the program. A ported
//! statement's item holds that clause; under `DFORM_CHECK_LOWER=1` every
//! statement's is built, each goal and clause lowered back and compared
//! (`program::check`).
//!
//! The helper statements a literal's terms make (a comprehension's `not
//! { }` rule, a loader's extern) no node of the term lowers to yet: they
//! are the goal's, [`Program::terms_made`], lowered before its own.

use super::{Builder, Form, Written};
use crate::ast::{Lit, Span, Stmt};
use crate::program::node::{ClauseId, GoalId, VarId};
use crate::program::{NodeId, Program, check};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// The builders' state while the resolver lowers a statement: a frame
/// per literal (and clause) being lowered, innermost last.
#[derive(Default)]
pub struct Gather {
    frames: Vec<Frame>,
    /// The statement's variables, by the name each lowers to.
    vars: BTreeMap<String, VarId>,
    /// The clause of the statement's body, once it is lowered whole.
    clause: Option<ClauseId>,
    /// Whether each goal and clause built is lowered back and compared.
    compare: bool,
}

/// What a statement gathered: its variables and its clause.
pub struct Statement {
    vars: BTreeMap<String, VarId>,
    clause: Option<ClauseId>,
}

/// One literal's (or clause's) lowering in progress.
#[derive(Default)]
struct Frame {
    /// The goals of the literals lowered inside it: a clause's own, a
    /// `not`'s body's (and those of the comprehensions its terms hold),
    /// each with the helper statements made inside it.
    goals: Vec<(GoalId, BTreeSet<usize>)>,
    /// Whether each literal lowered inside it was built.
    failed: bool,
    /// The value column of the read the literal tests in place.
    read: Option<usize>,
    /// The `__neg_N` it took, the statement's aggregate values then, and
    /// where in `goals` its body starts.
    negation: Option<(u32, Vec<String>, usize)>,
    /// The helper statements its terms made (a comprehension's `not { }`,
    /// a loader's extern), which no goal of its own makes.
    terms: BTreeSet<usize>,
}

impl Gather {
    /// A gathering; `compare`, each goal and clause is lowered back and
    /// compared with what the resolver lowered it to.
    pub fn new(compare: bool) -> Self {
        Gather {
            compare,
            ..Gather::default()
        }
    }

    /// A literal's or a clause's lowering starts.
    pub fn open(&mut self) {
        self.frames.push(Frame::default());
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
        for f in &mut self.frames {
            f.terms.extend(made.clone());
        }
    }

    /// The literal being lowered failed: nothing to build, and its clause
    /// is not built.
    pub fn failed(&mut self) {
        self.frames.pop();
        if let Some(f) = self.frames.last_mut() {
            f.failed = true;
        }
    }

    /// The clause of the statement's body, built when it was lowered
    /// whole: taken.
    pub fn take_clause(&mut self) -> Option<ClauseId> {
        self.clause.take()
    }

    /// The clause of the statement's body just lowered, at `span`: as
    /// gathered, or empty when no literal is written in it.
    pub fn body_clause(&mut self, program: &mut Program, span: Span) -> ClauseId {
        if let Some(c) = self.clause.take() {
            return c;
        }
        let mut b = self.builder(program, span, &|_| true);
        let c = b.clause_of(Vec::new());
        self.done(b);
        c
    }

    /// A builder into `program` of the statement's nodes, its variables
    /// the statement's: [`Gather::done`] gives them back.
    pub fn builder<'p>(
        &mut self,
        program: &'p mut Program,
        span: Span,
        written: &'p dyn Fn(&str) -> bool,
    ) -> Builder<'p> {
        Builder::new(program, span, written).with_vars(std::mem::take(&mut self.vars))
    }

    /// `b`'s variables are the statement's again.
    pub fn done(&mut self, b: Builder) {
        self.vars = b.into_vars();
    }

    /// A statement starts: what the one around it gathered so far, for
    /// [`Gather::resume`].
    pub fn begin(&mut self) -> Statement {
        Statement {
            vars: std::mem::take(&mut self.vars),
            clause: self.clause.take(),
        }
    }

    /// The statement ends: the one around it gathers on.
    pub fn resume(&mut self, outer: Statement) {
        self.vars = outer.vars;
        self.clause = outer.clause;
    }

    /// The written literal being lowered, `form` at `span`, lowered to
    /// `out[start..mid]` (`out[mid..]` the field reads after it) after the
    /// clause's `out[..start]`, and making `helpers[made..]`: built as a
    /// goal in `program`, one of the frame around it, the helpers its
    /// terms made the goal's.
    #[allow(clippy::too_many_arguments)]
    pub fn literal(
        &mut self,
        program: &mut Program,
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
            let body: Vec<GoalId> = negation.map_or(Vec::new(), |(_, _, from)| {
                frame.goals[*from..].iter().map(|(g, _)| *g).collect()
            });
            let w = Written {
                form,
                span,
                lits: &out[start..mid],
                after: &out[mid..],
                helpers: &own,
                body: &body,
                read: frame.read,
                negation: negation.map(|(n, folded, _)| (*n, folded.as_slice())),
            };
            let mut b = self.builder(program, span, written);
            let goal = b.literal(&w);
            self.done(b);
            if let Some(g) = goal {
                let made_by_terms = frame.attached(helpers);
                if !made_by_terms.is_empty() {
                    program.terms_made.insert(NodeId::Goal(g), made_by_terms);
                }
            }
            if self.compare {
                let context = &out[..start];
                let lowered: Vec<Lit> = out[start..].to_vec();
                check::literal(program, goal, &lowered, &helpers[made..], context, || {
                    format!("the literal at {}: {form:?}", check::place(span))
                });
            }
            goal
        });
        if let Some(f) = self.frames.last_mut() {
            match built {
                Some(g) => f.goals.push((g, frame.terms)),
                None => f.failed = true,
            }
        }
    }

    /// A `set` target's `[_]` at `span` bound by `body[start..]` after the
    /// statement's `body[..start]`, making `helpers[made..]`: built as the
    /// membership goal it is.
    pub fn each(
        &mut self,
        program: &mut Program,
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
        let mut b = self.builder(program, span, &implicit);
        let goal = b.each(&w);
        self.done(b);
        if self.compare {
            check::literal(program, goal, &body[start..], &own, &body[..start], || {
                format!("the `[_]` at {}", check::place(span))
            });
        }
    }

    /// The clause being lowered, `out[seed..]` after the literals its
    /// statement holds before it, `out[..seed]`, making `helpers[made..]`:
    /// its literals' goals one clause, the statement's.
    pub fn clause(
        &mut self,
        program: &mut Program,
        span: Span,
        out: &[Lit],
        seed: usize,
        (helpers, made): (&[Stmt], usize),
    ) {
        let frame = self.frames.pop().expect("a clause's frame");
        if frame.failed {
            return;
        }
        let goals = frame.goals.into_iter().map(|(g, _)| g).collect();
        let mut b = self.builder(program, span, &|_| true);
        let clause = b.clause_of(goals);
        self.done(b);
        self.clause = Some(clause);
        if self.compare {
            check::clause(
                program,
                clause,
                span,
                &out[..seed],
                &out[seed..],
                &helpers[made..],
            );
        }
    }
}

impl Frame {
    /// The helper statements of `helpers[made..]` the frame's own goals
    /// made: not its terms'.
    fn own(&self, helpers: &[Stmt], made: usize) -> Vec<Stmt> {
        (made..helpers.len())
            .filter(|i| !self.terms.contains(i))
            .map(|i| helpers[i].clone())
            .collect()
    }

    /// The helper statements its terms made that no goal of its body
    /// holds already.
    fn attached(&self, helpers: &[Stmt]) -> Vec<Stmt> {
        let from = self.negation.as_ref().map_or(self.goals.len(), |n| n.2);
        let held: BTreeSet<usize> = self.goals[from..]
            .iter()
            .flat_map(|(_, made)| made.iter().copied())
            .collect();
        self.terms
            .iter()
            .filter(|i| !held.contains(i))
            .map(|&i| helpers[i].clone())
            .collect()
    }
}
