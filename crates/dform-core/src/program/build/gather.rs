//! The goals of the statement being lowered (R-211 steps 4-5, 7c): while
//! the resolver lowers a statement's body, it builds each written
//! literal's goal of the nodes of its terms, and the body as a clause of
//! those goals, in the program. The statement's item holds that clause.
//!
//! The helper statements a literal's terms make (a comprehension's `not
//! { }` rule, a loader's extern) no node of the term lowers to yet: they
//! are the goal's, [`Program::terms_made`], lowered before its own.

use super::{Builder, Reads};
use crate::ast::{Lit, Span, Stmt};
use crate::program::node::{ClauseId, ExprId, GoalId, GoalKind, VarId};
use crate::program::{NodeId, Program, lower_clause, lower_goal};
use crate::syntax::resolve::{bound_vars, lit_vars};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// The builders' state while the resolver lowers a statement: a frame
/// per literal (and clause) being lowered, innermost last.
#[derive(Default)]
pub struct Gather {
    frames: Vec<Frame>,
    /// The statement's variables, by the name each lowers to.
    vars: BTreeMap<String, VarId>,
    /// The reads the statement's resolver kept.
    reads: Reads,
    /// The clause of the statement's body, once it is lowered whole.
    clause: Option<ClauseId>,
}

/// What a statement gathered: its variables, its reads and its clause.
pub struct Statement {
    vars: BTreeMap<String, VarId>,
    reads: Reads,
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
    /// The `__neg_N` it took, the statement's aggregate values then, and
    /// where in `goals` its body starts.
    negation: Option<(u32, Vec<String>, usize)>,
    /// The helper statements its terms made (a comprehension's `not { }`,
    /// a loader's extern), which no goal of its own makes.
    terms: BTreeSet<usize>,
    /// The goal the resolver built of it from its nodes.
    built: Option<GoalId>,
}

impl Gather {
    /// A literal's or a clause's lowering starts.
    pub fn open(&mut self) {
        self.frames.push(Frame::default());
    }

    /// The resolver built the literal being lowered as `goal`, from the
    /// nodes of its terms.
    pub fn built(&mut self, goal: GoalId) {
        if let Some(f) = self.frames.last_mut() {
            f.built = Some(goal);
        }
    }

    /// The goal the resolver built of the literal being lowered, taken.
    pub fn take_built(&mut self) -> Option<GoalId> {
        self.frames.last_mut().and_then(|f| f.built.take())
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

    /// The literal being lowered is `not __neg_{number}(args)` at `span`,
    /// written `written`, its helper's body the goals its body's literals
    /// were built as ([`Gather::negation`]): built as that goal.
    pub fn negated(
        &mut self,
        program: &mut Program,
        args: &[String],
        span: Span,
        written: crate::program::node::Negated,
    ) {
        let Some(f) = self.frames.last() else { return };
        let Some((number, folded, from)) = f.negation.clone() else {
            return;
        };
        if f.failed {
            return;
        }
        let body: Vec<GoalId> = f.goals[from..].iter().map(|(g, _)| *g).collect();
        let mut b = self.builder(program, span, &|_| true);
        let helper = crate::program::node::NegHelper {
            number,
            args: args.iter().map(|v| b.var(v)).collect(),
            folded: folded.iter().map(|v| b.var(v)).collect(),
        };
        let clause = b.clause_of(body);
        let kind = crate::program::node::GoalKind::Not {
            clause,
            helper: Some(helper),
            written,
        };
        let g = b.goal_node(span, kind);
        self.done(b);
        self.built(g);
    }

    /// A term made the helper statements `made`: no literal around it
    /// does.
    pub fn term(&mut self, made: Range<usize>) {
        for f in &mut self.frames {
            f.terms.extend(made.clone());
        }
    }

    /// The goals of the literals lowered since [`Gather::open`], a
    /// comprehension's body: a clause in `program` at `span`, or none when
    /// one failed.
    pub fn close(&mut self, program: &mut Program, span: Span) -> Option<ClauseId> {
        let frame = self.frames.pop().expect("a body's frame");
        if frame.failed {
            return None;
        }
        let goals = frame.goals.into_iter().map(|(g, _)| g).collect();
        let mut b = self.builder(program, span, &|_| true);
        let clause = b.clause_of(goals);
        self.done(b);
        Some(clause)
    }

    /// The literal being lowered failed: nothing to build, and its clause
    /// is not built.
    pub fn failed(&mut self) {
        self.frames.pop();
        if let Some(f) = self.frames.last_mut() {
            f.failed = true;
        }
    }

    /// The statement's clause in the order its goals hold (R-10,
    /// [`crate::program::bind::order`], `order`): what binds before what
    /// reads it, a negation's helper taking what its body shares with
    /// what the goals before it bind and what the statement around binds
    /// (`outer`).
    pub fn order(&mut self, program: &mut Program, order: &[usize], outer: &BTreeSet<String>) {
        let Some(c) = self.clause else { return };
        if order.iter().enumerate().all(|(i, &j)| i == j) {
            return;
        }
        let written = program.clauses[c].goals.clone();
        let goals: Vec<GoalId> = order.iter().map(|&i| written[i]).collect();
        program.clauses[c].goals = goals.clone();
        let mut before: Vec<Lit> = Vec::new();
        for g in goals {
            if let GoalKind::Not {
                clause,
                helper: Some(_),
                ..
            } = &program.goals[g].kind
            {
                let args = self.helper_args(program, *clause, &before, outer);
                if let GoalKind::Not {
                    helper: Some(h), ..
                } = &mut program.goals[g].kind
                {
                    h.args = args;
                }
            }
            let (lits, _) = lower_goal(program, g, &before);
            before.extend(lits);
        }
    }

    /// The arguments of a negation's helper whose body is `clause`: the
    /// variables it shares with what `before` and `outer` bind.
    fn helper_args(
        &self,
        program: &Program,
        clause: ClauseId,
        before: &[Lit],
        outer: &BTreeSet<String>,
    ) -> Vec<VarId> {
        let (inner, _) = lower_clause(program, clause, &[]);
        let mut bound = bound_vars(before);
        bound.extend(outer.iter().cloned());
        let mut used = BTreeSet::new();
        for l in &inner {
            lit_vars(l, &mut used);
        }
        used.intersection(&bound)
            .filter_map(|v| self.vars.get(v).copied())
            .collect()
    }

    /// Whether a read built so far in the statement tests the copy in the
    /// scope `scope` itself ([`Reads::holds`]).
    pub fn holds(&self, scope: ExprId) -> bool {
        self.reads.holds(scope)
    }

    /// The clause of the statement's body, once it is lowered whole.
    pub fn body(&self) -> Option<ClauseId> {
        self.clause
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
        let (vars, reads) = (
            std::mem::take(&mut self.vars),
            std::mem::take(&mut self.reads),
        );
        Builder::new(program, span, written).of_statement(vars, reads)
    }

    /// `b`'s variables and reads are the statement's again.
    pub fn done(&mut self, b: Builder) {
        (self.vars, self.reads) = b.into_statement();
    }

    /// A statement starts: what the one around it gathered so far, for
    /// [`Gather::resume`].
    pub fn begin(&mut self) -> Statement {
        Statement {
            vars: std::mem::take(&mut self.vars),
            reads: std::mem::take(&mut self.reads),
            clause: self.clause.take(),
        }
    }

    /// The statement ends: the one around it gathers on.
    pub fn resume(&mut self, outer: Statement) {
        self.vars = outer.vars;
        self.reads = outer.reads;
        self.clause = outer.clause;
    }

    /// The written literal being lowered at `span`, the resolver's
    /// `helpers` made so far: the goal the resolver built of it, after the
    /// `tests` it made that no read of it holds, one of the frame around
    /// it; the helpers its terms made that no node lowers to the goal's.
    pub fn literal(
        &mut self,
        program: &mut Program,
        span: Span,
        tests: &[(Option<ExprId>, GoalId)],
        helpers: &[Stmt],
    ) {
        let frame = self.frames.pop().expect("a literal's frame");
        let goal = frame
            .built
            .map(|g| self.after_tests(program, span, tests, g));
        if let Some(g) = goal {
            let made = super::unlowered(&frame.attached(helpers));
            if !made.is_empty() {
                program.terms_made.insert(NodeId::Goal(g), made);
            }
        }
        if let Some(f) = self.frames.last_mut() {
            match goal {
                Some(g) => f.goals.push((g, frame.terms)),
                None => f.failed = true,
            }
        }
    }

    /// `goal` after the tests `tests` that no read of it holds: a group
    /// of them and it, at `span`.
    fn after_tests(
        &mut self,
        program: &mut Program,
        span: Span,
        tests: &[(Option<ExprId>, GoalId)],
        goal: GoalId,
    ) -> GoalId {
        let mut goals: Vec<GoalId> = tests
            .iter()
            .filter(|(by, _)| by.is_none_or(|s| !self.reads.holds(s)))
            .map(|&(_, g)| g)
            .collect();
        if goals.is_empty() {
            return goal;
        }
        goals.push(goal);
        let mut b = self.builder(program, span, &|_| true);
        let kind = GoalKind::Group {
            goals,
            after: Vec::new(),
        };
        let g = b.goal_node(span, kind);
        self.done(b);
        g
    }

    /// The clause being lowered, at `span`: its literals' goals one
    /// clause, the statement's.
    pub fn clause(&mut self, program: &mut Program, span: Span) {
        let frame = self.frames.pop().expect("a clause's frame");
        if frame.failed {
            return;
        }
        let goals = frame.goals.into_iter().map(|(g, _)| g).collect();
        let mut b = self.builder(program, span, &|_| true);
        let clause = b.clause_of(goals);
        self.done(b);
        self.clause = Some(clause);
    }
}

impl Frame {
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
