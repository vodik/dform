//! A clause's goals (R-211 step 4): each written literal one goal of
//! the form the tree says it is, built from what the resolver lowered it
//! to, a [`Written`]:
//!
//! | written              | goal                                                  |
//! |----------------------|-------------------------------------------------------|
//! | `p(a, b)`            | `Rel`                                                 |
//! | `x.ready`            | `Truth`: the read itself with `true`, or `x = true`   |
//! | `has r`, `has R.p`   | `Has`, `Marked` by its attribute path (R-106)         |
//! | `a < b`, `x = e`     | `Compare`, `Bind`: a pattern, an element, a read      |
//! | `n = count(x)`       | `Fold` (`aggregate.rs`)                               |
//! | `x in e`, `not in`   | `Member` (`membership.rs`)                            |
//! | `not L`, `not { B }` | `Not`, through a helper when no literal says it (`negation.rs`) |
//!
//! The reads a literal hoisted are its terms' nodes in term position, as
//! a term's are (`hoist.rs`); one whose variable no term of the literal
//! holds (a membership its terms range over) is a goal before it
//! (`GoalKind::Group`). A read the literal tests or binds in place
//! (`R.p == c`, `has n.k`) is an `ExprKind::Read`, its value column the
//! one the resolver says. A body's literal the resolver lowered (a
//! comprehension's, a hoisted read) is the goal of its `Lit`
//! ([`Builder::goal`]).

use super::Builder;
use crate::ast::{Atom, Lit, Span, Stmt, Term};
use crate::program::node::*;
use crate::value::Value;

/// A written literal as the resolver lowered it: what a clause builder
/// reads.
pub struct Written<'a> {
    /// Which literal it is, read off the tree.
    pub form: &'a Form,
    pub span: Span,
    /// The literals it lowered to, the reads it hoisted first.
    pub lits: &'a [Lit],
    /// The field reads its object patterns make after it.
    pub after: &'a [Lit],
    /// The statements it made beside its clause (a `not`'s helper rule,
    /// the facts a membership states), not those its terms made.
    pub helpers: &'a [Stmt],
    /// The goals of a `not`'s body, built already.
    pub body: &'a [GoalId],
    /// The value column of the read it tests or binds in place.
    pub read: Option<usize>,
    /// The `__neg_N` it took, and the statement's aggregate values then
    /// (lowered names), when it lowers through a helper.
    pub negation: Option<(u32, &'a [String])>,
}

/// The kinds of written literal, as the tree says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// `p(a, b)`.
    Atom,
    /// `x.ready`.
    Truth,
    /// `has x`.
    Has,
    /// `a op b [op c]`: `bind` when the first operator is `=`;
    /// `aggregate` for `n = count(x)`.
    Compare {
        bind: bool,
        ops: usize,
        aggregate: bool,
    },
    /// `x in e`; `each` when the right side is a path with `[_]`.
    In { each: bool },
    /// `x not in e`.
    NotIn { each: bool },
    /// `not L`, the form of L.
    Not(Box<Form>),
    /// `not { B }`.
    NotBlock,
}

impl Builder<'_> {
    /// The goal written literal `w` is; `None` when what it lowered to is
    /// not a form its builder knows (the differential names it).
    pub fn literal(&mut self, w: &Written) -> Option<GoalId> {
        self.at(w.span, |b| b.literal_at(w))
    }

    fn literal_at(&mut self, w: &Written) -> Option<GoalId> {
        let (main, left) = match w.form {
            Form::Atom => {
                let (Lit::Pos(a), reads) = w.lits.split_last()? else {
                    return None;
                };
                self.reading(reads, |b| Some(b.rel(a)))
            }
            Form::Truth => {
                let (last, reads) = w.lits.split_last()?;
                self.reading(reads, |b| b.truth(last, w.read))
            }
            Form::Has => return self.has(w),
            Form::Compare { bind, ops, .. } if *ops > 1 => {
                return self.chain(w, *bind, *ops);
            }
            Form::Compare {
                bind, aggregate, ..
            } => {
                let (last, reads) = w.lits.split_last()?;
                self.reading(reads, |b| b.compare(last, *bind, *aggregate, w.read))
            }
            Form::In { each } => return self.membership(w, *each, false),
            // `v not in PATH[_]`: through a helper over the membership.
            Form::NotIn { .. } if w.negation.is_some() => return self.negation(w, None),
            Form::NotIn { each } => return self.membership(w, *each, true),
            Form::Not(inner) => return self.negation(w, Some(inner)),
            Form::NotBlock => return self.negation(w, None),
        };
        Some(self.group(&left, vec![main?], w.after))
    }

    /// `goals` after the reads `left` no term of theirs holds (each its
    /// goal) and before `after`, one goal.
    pub(super) fn group(&mut self, left: &[Lit], goals: Vec<GoalId>, after: &[Lit]) -> GoalId {
        if left.is_empty() && after.is_empty() && goals.len() == 1 {
            return goals[0];
        }
        let mut all: Vec<GoalId> = left.iter().map(|l| self.goal(l)).collect();
        all.extend(goals);
        let after = after.iter().map(|l| self.goal(l)).collect();
        let kind = GoalKind::Group { goals: all, after };
        self.goal_node(self.span, kind)
    }

    /// `x.ready`: the read with `true` in its value column, or `t = true`.
    fn truth(&mut self, l: &Lit, read: Option<usize>) -> Option<GoalId> {
        let yes = Term::Val(Value::Bool(true));
        let e = match (l, read) {
            (Lit::Pos(a), Some(c)) if a.args.get(c) == Some(&yes) => self.read(a, c),
            (Lit::Eq(t, v), None) if *v == yes => self.expr(t),
            _ => return None,
        };
        Some(self.goal_node(self.span, GoalKind::Truth(e)))
    }

    /// `has x`: the resource's identity, the read with `_` in its value
    /// column, or the walk to it bound; marked when it tests a resource's
    /// attribute path, the mark after the reads that name the resource
    /// and before those of the walk.
    fn has(&mut self, w: &Written) -> Option<GoalId> {
        let (outer, mark, lits) = split_mark(w.lits);
        let (last, reads) = lits.split_last()?;
        let (mark, outer) = self.reading(outer, |b| mark.map(|m| b.mark(m)));
        let (test, left) = self.reading(reads, |b| b.has_test(last, w.read, mark.is_none()));
        let test = test?;
        let span = match last {
            Lit::Pos(a) => a.span,
            _ => self.span,
        };
        let has = self.goal_node(span, GoalKind::Has(test));
        let inner = self.group(&left, vec![has], &[]);
        let marked = match mark {
            Some(m) => self.marked(m?, inner),
            None => inner,
        };
        Some(self.group(&outer, vec![marked], w.after))
    }

    /// What `has` tests, written `last` (`unmarked`: no attribute path's
    /// mark before it).
    fn has_test(&mut self, last: &Lit, read: Option<usize>, unmarked: bool) -> Option<Has> {
        Some(match (last, read) {
            (Lit::Pos(a), None) if a.pred == crate::partition::IDENTITY && unmarked => {
                let [typ, addr] = a.args.as_slice() else {
                    return None;
                };
                Has::Resource {
                    typ: self.expr(typ),
                    addr: self.expr(addr),
                }
            }
            (Lit::Pos(a), Some(c)) if a.args.get(c) == Some(&Term::Wildcard) => {
                Has::Read(self.read(a, c))
            }
            (Lit::Eq(Term::Var(v), t), None) if crate::syntax::resolve::is_has_var(v) => {
                Has::Walk {
                    var: self.var(v),
                    value: self.expr(t),
                }
            }
            _ => return None,
        })
    }

    /// The mark `__has(T, A, "PATH", N)`, at its span.
    pub(super) fn mark(&mut self, mark: &Atom) -> Option<(HasMark, Span)> {
        let [typ, addr, Term::Val(Value::Str(path)), _] = mark.args.as_slice() else {
            return None;
        };
        let m = HasMark {
            typ: self.expr(typ),
            addr: self.expr(addr),
            path: path.clone(),
        };
        Some((m, mark.span))
    }

    /// `goal` marked by `mark`.
    pub(super) fn marked(&mut self, (mark, span): (HasMark, Span), goal: GoalId) -> GoalId {
        self.goal_node(span, GoalKind::Marked { mark, goal })
    }

    /// `a = b`, `a == b`, `a < b`: a pattern bound, an element, the read
    /// itself, an aggregate's binding, a comparison.
    fn compare(
        &mut self,
        l: &Lit,
        bind: bool,
        aggregate: bool,
        read: Option<usize>,
    ) -> Option<GoalId> {
        if aggregate {
            return self.fold(l);
        }
        let span = self.span;
        let kind = match (l, read) {
            (Lit::Pos(a), Some(c)) => {
                let v = a.args.get(c)?;
                let goal = self.at(a.span, |b| b.read(a, c));
                match bind {
                    true => GoalKind::Bind {
                        pat: self.pattern(v),
                        value: goal,
                    },
                    false => GoalKind::Compare {
                        lhs: goal,
                        ops: vec![(CmpOp::Eq, self.expr(v))],
                    },
                }
            }
            // `P = e[i]`: the element `member(e, i, P)`.
            (Lit::Pos(a), None) if a.pred == "member" && a.args.len() == 3 => {
                let [list, i, p] = a.args.as_slice() else {
                    unreachable!("three columns")
                };
                let path = vec![Step::Index(self.expr(i))];
                let base = self.expr(list);
                let value = self.expr_node(ExprKind::Field { base, path });
                let pat = self.pattern(p);
                return Some(self.goal_node(a.span, GoalKind::Bind { pat, value }));
            }
            (Lit::Eq(a, b), _) if bind => GoalKind::Bind {
                pat: self.pattern(a),
                value: self.expr(b),
            },
            (l, None) => {
                let (op, a, b) = comparison(l)?;
                GoalKind::Compare {
                    lhs: self.expr(a),
                    ops: vec![(op, self.expr(b))],
                }
            }
            _ => return None,
        };
        Some(self.goal_node(span, kind))
    }

    /// `a op b op c ..`: one comparison of the chain, or a goal per pair
    /// when a middle term was read as a different quantity on each side.
    fn chain(&mut self, w: &Written, bind: bool, ops: usize) -> Option<GoalId> {
        let at = w.lits.len().checked_sub(ops)?;
        let (reads, written) = w.lits.split_at(at);
        let pairs: Vec<(CmpOp, &Term, &Term)> =
            written.iter().map(comparison).collect::<Option<_>>()?;
        let shared = pairs.windows(2).all(|p| p[0].2 == p[1].1);
        let (goals, left) = self.reading(reads, |b| match shared {
            true => {
                let lhs = b.expr(pairs[0].1);
                let ops = pairs.iter().map(|(op, _, x)| (*op, b.expr(x))).collect();
                Some(vec![b.goal_node(b.span, GoalKind::Compare { lhs, ops })])
            }
            false => written
                .iter()
                .map(|l| b.compare(l, bind, false, None))
                .collect::<Option<_>>(),
        });
        Some(self.group(&left, goals?, w.after))
    }

    /// The lowered body `lits`.
    pub(super) fn clause(&mut self, lits: &[Lit]) -> ClauseId {
        let goals = lits.iter().map(|l| self.goal(l)).collect();
        self.clause_of(goals)
    }

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

    /// The lowered literal `l`, as the goal it lowers from: an atom a
    /// relation's goal, a negated one `not` of it, `=` a binding, any
    /// other comparison a comparison.
    pub(super) fn goal(&mut self, l: &Lit) -> GoalId {
        let (op, lhs, rhs) = match l {
            Lit::Pos(a) => return self.rel(a),
            Lit::Not(a) => {
                let g = self.rel(a);
                let clause = self.at(a.span, |b| b.clause_of(vec![g]));
                let kind = GoalKind::Not {
                    clause,
                    helper: None,
                };
                return self.goal_node(a.span, kind);
            }
            Lit::Eq(a, b) => {
                let kind = GoalKind::Bind {
                    pat: self.pattern(a),
                    value: self.expr(b),
                };
                return self.goal_node(self.span, kind);
            }
            l => comparison(l).expect("not an atom"),
        };
        let kind = GoalKind::Compare {
            lhs: self.expr(lhs),
            ops: vec![(op, self.expr(rhs))],
        };
        self.goal_node(self.span, kind)
    }

    /// The atom `a`: the relation's goal, its columns by position or by
    /// name.
    pub(super) fn rel(&mut self, a: &Atom) -> GoalId {
        if let Some(g) = self.hoisted_read(a) {
            return g;
        }
        self.at(a.span, |b| {
            let args = b.args(a);
            let rel = RelRef {
                name: a.pred.clone(),
                span: a.span,
            };
            b.goal_node(a.span, GoalKind::Rel { rel, args })
        })
    }

    /// The columns of `a`, by position or by name (a record's in the
    /// order its reads were hoisted).
    pub(super) fn args(&mut self, a: &Atom) -> RelArgs {
        match &a.record {
            Some(r) => {
                debug_assert!(a.args.is_empty(), "a record atom has no positional column");
                let cols = self.in_read_order(r);
                RelArgs::Record(
                    cols.into_iter()
                        .map(|(k, t)| (k.clone(), self.pattern(t)))
                        .collect(),
                )
            }
            None => RelArgs::Positional(a.args.iter().map(|t| self.pattern(t)).collect()),
        }
    }

    pub(super) fn goal_node(&mut self, span: Span, kind: GoalKind) -> GoalId {
        self.program.goals.insert(Goal { span, kind })
    }

    /// `p(.., V, ..)`, a read the front end hoisted, as a literal of a
    /// body: the goal binding `V` to the node of what it reads
    /// (`Program::reads`). `None` for any other atom, and an attribute of
    /// a resource whose type is a variable (`x in T` over several types),
    /// which stays its goal.
    fn hoisted_read(&mut self, a: &Atom) -> Option<GoalId> {
        let (at, read) = self.recorded(a)?;
        self.at(a.span, |b| {
            let value = b.read_kind(a, at, read)?;
            let pat = b.pattern(&a.args[at]);
            let value = b.expr_node(value);
            Some(b.goal_node(a.span, GoalKind::Bind { pat, value }))
        })
    }

    /// The read `a` with its value column `column` left open: the value
    /// the literal holding it tests or binds in place.
    pub(super) fn read(&mut self, a: &Atom, column: usize) -> ExprId {
        // A copy's output by its key, its copy tested to exist first.
        if let ("output", [copy, Term::Val(Value::Str(key)), _], 2) =
            (a.pred.as_str(), a.args.as_slice(), column)
            && let Some(of) = self.instance_test(copy)
        {
            let copy = self.expr(copy);
            let key = key.clone();
            let of = Some(of);
            return self.expr_node(ExprKind::Output { copy, key, of });
        }
        let mut open = a.clone();
        open.args[column] = Term::Wildcard;
        let goal = self.rel(&open);
        self.expr_node(ExprKind::Read { goal, column })
    }
}

/// `lits` around the `has` mark among them, `__has(T, A, "PATH", N)`
/// before the N literals it covers, which end them: the literals before
/// it, the mark, and those it covers. No mark: all of them are covered.
pub(super) fn split_mark(lits: &[Lit]) -> (&[Lit], Option<&Atom>, &[Lit]) {
    let at = lits.iter().enumerate().find_map(|(i, l)| match l {
        Lit::Pos(a) | Lit::Not(a) if a.pred == crate::partition::HAS => match a.args.last() {
            Some(Term::Val(Value::Int(n))) if *n as usize == lits.len() - i - 1 => Some((i, a)),
            _ => None,
        },
        _ => None,
    });
    match at {
        Some((i, a)) => (&lits[..i], Some(a), &lits[i + 1..]),
        None => (&[], None, lits),
    }
}

/// A comparison's operator and sides.
pub(super) fn comparison(l: &Lit) -> Option<(CmpOp, &Term, &Term)> {
    Some(match l {
        Lit::Eq(a, b) => (CmpOp::Eq, a, b),
        Lit::Neq(a, b) => (CmpOp::Ne, a, b),
        Lit::Gt(a, b) => (CmpOp::Gt, a, b),
        Lit::Ge(a, b) => (CmpOp::Ge, a, b),
        Lit::Lt(a, b) => (CmpOp::Lt, a, b),
        Lit::Le(a, b) => (CmpOp::Le, a, b),
        Lit::Pos(_) | Lit::Not(_) => return None,
    })
}
