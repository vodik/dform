//! `let k[: T] = t [@rank] [where B]` as a `Let` item (R-211 step 5),
//! from what the resolver lowered it to ([`LetLowered`]): its clause B's
//! goals as gathered, its value the term's node after the reads it
//! hoisted (the helper statements the value's terms made the value's);
//! `let n = count(x) where B` the clause with the binding a `Fold` last,
//! its value the variable. A clause whose aggregates fold through helpers
//! takes their numbers here, as a rule's does. A `let`
//! with parameters ([`LetFnLowered`], R-187) is a `LetFn` item: its
//! parameters' variables, its value, its clause after the demand that
//! binds them.

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Rank, Span, Stmt, TypeExpr};
use crate::program::NodeId;
use crate::program::node::{ClauseId, ExprId, GoalId, Item, ItemId, ItemKind, Param};
use crate::program::scope::ScopeId;

/// A `let` as the resolver lowered it.
pub struct LetLowered<'a> {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub rank: Option<Rank>,
    /// The first typed row: its type is the cell's.
    pub declares: bool,
    pub span: Span,
    pub scope: ScopeId,
    /// The clause of B, gathered; none when the `let` has no `where`.
    pub clause: Option<ClauseId>,
    /// `let(k, v, rank)`, as the resolver wrote it (an aggregate's value
    /// its variable).
    pub head: &'a Atom,
    /// B's literals, then the value's reads, or the aggregate's binding
    /// after the reads its call hoisted.
    pub body: &'a [Lit],
    /// The value's node, as the resolver built it: an aggregate's its
    /// variable, bound by its fold `fold`, the clause's last goal.
    pub value: Option<ExprId>,
    pub fold: Option<GoalId>,
    /// The helper statements the value's terms made.
    pub made: &'a [Stmt],
    /// The statement's aggregate values.
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `let` item of `l`; `None` when its aggregate binding is not one
    /// a builder knows.
    pub fn let_item(&mut self, l: LetLowered) -> Option<ItemId> {
        let mut clause = l.clause;
        let value = l.value?;
        let value = match l.fold {
            Some(fold) => {
                clause = Some(self.ending(clause, fold));
                self.made_by(NodeId::Goal(fold), l.made);
                value
            }
            None => self.holding(value, l.made),
        };
        if let Some(c) = clause
            && !l.results.is_empty()
            && grouped(l.head, l.body, l.results)
        {
            number_folds(self.program, c);
        }
        let kind = ItemKind::Let {
            name: l.name,
            ty: l.ty,
            value,
            clause,
            rank: l.rank,
            declares: l.declares,
        };
        Some(self.program.items.insert(Item {
            span: l.span,
            scope: l.scope,
            kind,
        }))
    }
}

/// A `let` with parameters as the resolver lowered it (R-187): the rule
/// `f(params, v) :- demand, B, reads`.
pub struct LetFnLowered<'a> {
    pub name: String,
    pub span: Span,
    pub scope: ScopeId,
    /// The clause of B, gathered after the demand; none without `where`.
    pub clause: Option<ClauseId>,
    /// `f(params, v)`, as the resolver wrote it.
    pub head: &'a Atom,
    /// The demand, B's literals, then the value's reads.
    pub body: &'a [Lit],
    /// The value's node, as the resolver built it.
    pub value: ExprId,
    /// The helper statements the value's terms made.
    pub made: &'a [Stmt],
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `LetFn` item of `l`: its parameters the head's leading
    /// variables, its value the last column's node after its reads.
    pub fn let_fn_item(&mut self, l: LetFnLowered) -> ItemId {
        let (_, params) = l.head.args.split_last().expect("a value column");
        let params = params
            .iter()
            .map(|t| match t {
                crate::ast::Term::Var(v) => Param {
                    var: self.var(v),
                    ty: None,
                    default: None,
                },
                t => unreachable!("a parameter is a variable: {t:?}"),
            })
            .collect();
        let value = self.holding(l.value, l.made);
        if let Some(c) = l.clause
            && !l.results.is_empty()
            && grouped(l.head, l.body, l.results)
        {
            number_folds(self.program, c);
        }
        let kind = ItemKind::LetFn {
            name: l.name,
            params,
            value,
            clause: l.clause,
        };
        self.program.items.insert(Item {
            span: l.span,
            scope: l.scope,
            kind,
        })
    }
}
