//! `p(a, b) [@rank] [where B]` as a `Rule` item (R-211 step 5), from what
//! the resolver lowered it to ([`RuleLowered`]): its head the relation and
//! its arguments' patterns, the reads they hoisted the head's; its clause
//! B's goals as gathered; a fact has none. A clause whose aggregates fold
//! through helpers takes their numbers here.

use super::{Builder, Column, grouped, number_folds};
use crate::ast::{Rank, Span, Stmt};
use crate::program::NodeId;
use crate::program::bind::Term;
use crate::program::node::{ClauseId, Head, Item, ItemId, ItemKind, RelArgs, RelRef};
use crate::program::scope::ScopeId;

/// A rule as the resolver lowered it.
pub struct RuleLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    /// The head: the relation and its arguments' patterns, built of its
    /// nodes; its rank not among them.
    pub rel: RelRef,
    pub args: RelArgs,
    pub rank: Option<Rank>,
    /// The clause of B, gathered; none for a fact.
    pub clause: Option<ClauseId>,
    /// The helper statements the head's terms made.
    pub made: &'a [Stmt],
}

impl Builder<'_> {
    /// The `rule` item of `r`.
    pub fn rule_item(&mut self, r: RuleLowered) -> ItemId {
        let columns: Option<Vec<Column>> = match &r.args {
            RelArgs::Positional(ps) => {
                Some(ps.iter().map(|&p| Column::Term(Term::Pattern(p))).collect())
            }
            RelArgs::Record(_) => None,
        };
        if let Some(c) = r.clause
            && grouped(self.program, c, columns.as_deref())
        {
            number_folds(self.program, c);
        }
        let head = Head {
            rel: r.rel,
            args: r.args,
        };
        let kind = ItemKind::Rule {
            head,
            clause: r.clause,
            rank: r.rank,
        };
        let item = self.program.items.insert(Item {
            span: r.span,
            scope: r.scope,
            kind,
        });
        self.made_by(NodeId::Item(item), r.made);
        item
    }
}
