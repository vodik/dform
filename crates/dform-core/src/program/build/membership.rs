//! `x in e` and `x not in e` as a `Member` goal (R-211 step 4), its
//! collection the form the resolver's `membership` lowered it to:
//!
//! | written                    | lowered                                         | `Coll`         |
//! |----------------------------|-------------------------------------------------|----------------|
//! | `x in xs`, `(k, v) in o`   | `member(xs', X)`, `member(o', K, V)`            | `Expr`         |
//! | `x in net.vpc`             | `want("net.vpc", X)`                            | `Type`         |
//! | `r in k8s`                 | `__namespace("k8s", T), want(T, R)`, its facts  | `Namespace`    |
//! | `r in ovh.instance` (R-115)| `__provider_type(.., T), want(T, R)`, its facts | `ProviderType` |
//! | `r in world.T`             | `cloud_exists("T", R)`                          | `World`        |
//! | `e in environment` (R-70)  | `__enum(..)` read, `member(L, E)`, its fact     | `Enum`         |
//! | `n in network` (R-67)      | `instance_of("network", S, N)`                  | `Copies`       |
//! | `v in T[_].p` (R-162)      | `V = e'`                                        | `Each`         |
//! | `r in T`, `r` a column's   | `Type = "T"`                                    | `TypeOf`       |
//!
//! `x not in e` is `not` around it: its last literal negated, the test a
//! namespace or a renamed type makes before it as it is.

use super::Builder;
use super::clause::Written;
use crate::ast::{Atom, Lit, Stmt, Term};
use crate::program::node::*;
use crate::value::Value;

/// The relations a membership states facts of beside it.
const ENUM: &str = "__enum";
const NAMESPACE: &str = "__namespace";
const PROVIDER_TYPE: &str = "__provider_type";

impl Builder<'_> {
    /// `x in e` (`negated`, `x not in e`): the membership, after the reads
    /// it hoisted.
    pub(super) fn membership(&mut self, w: &Written, each: bool, negated: bool) -> Option<GoalId> {
        let (last, rest) = w.lits.split_last()?;
        let last = match negated {
            true => negate(last),
            false => last.clone(),
        };
        let (member, reads) = self.member(&last, rest, w, each)?;
        let goal = match negated {
            true => self.not_one(member),
            false => member,
        };
        Some(self.hoisted_goal(reads, vec![goal], w.after))
    }

    /// The membership `last` is (after `before`, the literals before it),
    /// and the reads before it.
    fn member<'l>(
        &mut self,
        last: &Lit,
        before: &'l [Lit],
        w: &Written,
        each: bool,
    ) -> Option<(GoalId, &'l [Lit])> {
        let a = match last {
            Lit::Eq(x, e) => {
                let pat = self.pattern(x);
                let e = self.expr(e);
                let coll = if each { Coll::Each(e) } else { Coll::TypeOf(e) };
                return Some((
                    self.goal_node(self.span, GoalKind::Member { pat, coll }),
                    before,
                ));
            }
            Lit::Pos(a) => a,
            _ => return None,
        };
        let test = before.last().and_then(|l| match l {
            Lit::Pos(t) if matches!(t.pred.as_str(), NAMESPACE | PROVIDER_TYPE) => Some(t),
            _ => None,
        });
        let (pat, coll, reads) = match (a.pred.as_str(), a.args.as_slice()) {
            ("want", [typ, x]) => match test {
                Some(t) if t.args.get(1) == Some(typ) => {
                    let coll = self.tested(t, w.helpers, true)?;
                    (self.pattern(x), coll, &before[..before.len() - 1])
                }
                _ => (self.pattern(x), Coll::Type(self.expr(typ)), before),
            },
            (NAMESPACE | PROVIDER_TYPE, [_, _]) => {
                let coll = self.tested(a, w.helpers, false)?;
                (self.hole(), coll, before)
            }
            ("cloud_exists", [Term::Val(Value::Str(t)), x]) => {
                (self.pattern(x), Coll::World(t.clone()), before)
            }
            (crate::modules::INSTANCE_OF, [Term::Val(Value::Str(c)), scope, x]) => {
                let coll = Coll::Copies {
                    component: c.clone(),
                    scope: self.expr(scope),
                };
                (self.pattern(x), coll, before)
            }
            ("member", [list, elem @ ..]) if !elem.is_empty() && elem.len() <= 2 => {
                let pat = match elem {
                    [x] => self.element(x),
                    _ => self.tuple(elem),
                };
                let list = self.expr(list);
                let coll = match fact(w.helpers, ENUM) {
                    Some(f) => self.enum_values(f, list)?,
                    None => Coll::Expr(list),
                };
                (pat, coll, before)
            }
            _ => return None,
        };
        let goal = self.goal_node(a.span, GoalKind::Member { pat, coll });
        Some((goal, reads))
    }

    /// The namespace's or renamed type's test `t`, its facts among
    /// `helpers`: enumerated by a `want` after it, or a test alone.
    fn tested(&mut self, t: &Atom, helpers: &[Stmt], enumerate: bool) -> Option<Coll> {
        let [Term::Val(Value::Str(of)), typ] = t.args.as_slice() else {
            return None;
        };
        let names = facts(helpers, &t.pred)
            .map(|f| match f.args.as_slice() {
                [Term::Val(Value::Str(o)), Term::Val(Value::Str(n))] if o == of => Some(n.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let typ = self.expr(typ);
        Some(match t.pred.as_str() {
            NAMESPACE => Coll::Namespace {
                ns: of.clone(),
                types: names,
                typ,
                enumerate,
            },
            _ => Coll::ProviderType {
                typ: of.clone(),
                names,
                var: typ,
                enumerate,
            },
        })
    }

    /// An enum type's values, from its fact `__enum(name, values)`, over
    /// the list read from it.
    fn enum_values(&mut self, f: &Atom, list: ExprId) -> Option<Coll> {
        let [Term::Val(Value::Str(name)), Term::Val(Value::List(vs))] = f.args.as_slice() else {
            return None;
        };
        let values = vs
            .iter()
            .map(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Option<_>>()?;
        Some(Coll::Enum {
            name: name.clone(),
            values,
            def: f.span,
            list,
        })
    }

    /// `x in xs`'s element: a list written there is a value, not a tuple
    /// (a tuple is a key and a value, `member/3`).
    fn element(&mut self, x: &Term) -> PatternId {
        match x {
            Term::List(_) => {
                let e = self.expr(x);
                self.pattern_node(PatternKind::Expr(e))
            }
            x => self.pattern(x),
        }
    }

    /// `(k, v)`.
    fn tuple(&mut self, elems: &[Term]) -> PatternId {
        let elems = elems.iter().map(|t| self.pattern(t)).collect();
        self.pattern_node(PatternKind::Tuple { elems, rest: None })
    }

    fn hole(&mut self) -> PatternId {
        self.pattern_node(PatternKind::Hole)
    }

    /// `not g`, `g` one goal whose last literal is negated.
    pub(super) fn not_one(&mut self, g: GoalId) -> GoalId {
        let span = self.program.goals[g].span;
        let clause = self.at(span, |b| b.clause_of(vec![g]));
        let kind = GoalKind::Not {
            clause,
            helper: None,
        };
        self.goal_node(span, kind)
    }
}

/// The facts of `pred` among `helpers`.
fn facts<'h>(helpers: &'h [Stmt], pred: &'h str) -> impl Iterator<Item = &'h Atom> {
    helpers.iter().filter_map(move |s| match s {
        Stmt::Fact(a) if a.pred == pred => Some(a),
        _ => None,
    })
}

fn fact<'h>(helpers: &'h [Stmt], pred: &'h str) -> Option<&'h Atom> {
    facts(helpers, pred).next()
}

/// `not l` as the resolver writes it: a read negated, a negation read; a
/// comparison as it is.
pub(super) fn negate(l: &Lit) -> Lit {
    match l {
        Lit::Pos(a) => Lit::Not(a.clone()),
        Lit::Not(a) => Lit::Pos(a.clone()),
        other => other.clone(),
    }
}
