//! `x in e` as a `Member` goal (R-211 step 4), its collection the form
//! the resolver's `membership` lowers it to; the resolver builds it of
//! its nodes (step 7c), with the element's pattern and a negation built
//! here:
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

use super::Builder;
use crate::program::node::*;

impl Builder<'_> {
    /// `x in xs`'s element `x`: a list written there is a value, not a
    /// tuple (a tuple is a key and a value, `member/3`).
    pub fn element_of(&mut self, x: ExprId) -> PatternId {
        let e = &self.program.exprs[x];
        match (&e.kind, e.hoisted) {
            (ExprKind::List { parts }, None)
                if parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                self.pattern_node(PatternKind::Expr(x))
            }
            _ => self.pattern_of(x),
        }
    }

    /// `not g` written `written`, `g` one goal whose last literal is
    /// negated.
    pub fn not_one(&mut self, g: GoalId, written: Negated) -> GoalId {
        let span = self.program.goals[g].span;
        let clause = self.at(span, |b| b.clause_of(vec![g]));
        let kind = GoalKind::Not {
            clause,
            helper: None,
            written,
        };
        self.goal_node(span, kind)
    }
}
