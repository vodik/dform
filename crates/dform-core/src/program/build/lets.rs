//! `let k[: T] = t [@rank] [where B]` as a `Let` item (R-211 step 5),
//! from what the resolver lowered it to ([`LetLowered`]): its clause B's
//! goals as gathered, its value the term's node after the reads it
//! hoisted (the helper statements the value's terms made the value's);
//! `let n = count(x) where B` the clause with the binding a `Fold` last,
//! its value the variable. A clause whose aggregates fold through helpers
//! takes their numbers here, as a rule's does (`folded_rule`).

use super::{Builder, Form, Written, grouped, number_folds};
use crate::ast::{Atom, Lit, Rank, Span, Stmt, TypeExpr};
use crate::program::NodeId;
use crate::program::node::{ClauseId, Item, ItemId, ItemKind};
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
    /// B's literals, then from `seed` the value's reads, or the
    /// aggregate's binding after the reads its call hoisted.
    pub body: &'a [Lit],
    pub seed: usize,
    pub aggregate: bool,
    /// The helper statements the value's terms made.
    pub made: &'a [Stmt],
    /// The statement's aggregate values.
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `let` item of `l`; `None` when its aggregate binding is not one
    /// a builder knows.
    pub fn let_item(&mut self, l: LetLowered) -> Option<ItemId> {
        let value = &l.head.args[1];
        let mut clause = l.clause;
        let (value, made_by) = match l.aggregate {
            true => {
                let form = Form::Compare {
                    bind: true,
                    ops: 1,
                    aggregate: true,
                };
                let w = Written {
                    form: &form,
                    span: self.span,
                    lits: &l.body[l.seed..],
                    after: &[],
                    helpers: &[],
                    body: &[],
                    read: None,
                    negation: None,
                };
                let fold = self.literal(&w)?;
                clause = Some(self.ending(clause, fold));
                (self.expr(value), NodeId::Goal(fold))
            }
            false => {
                let e = self.hoisted(value, &l.body[l.seed..]);
                (e, NodeId::Expr(e))
            }
        };
        if !l.made.is_empty() {
            self.program.terms_made.insert(made_by, l.made.to_vec());
        }
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
