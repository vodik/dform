//! `p(a, b) [@rank] [where B]` as a `Rule` item (R-211 step 5), from what
//! the resolver lowered it to ([`RuleLowered`]): its head the relation and
//! its arguments' patterns, the reads they hoisted the head's; its clause
//! B's goals as gathered; a fact has none. A clause whose aggregates fold
//! through helpers takes their numbers here (`folded_rule`'s rule).

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Rank, Span, Stmt};
use crate::program::NodeId;
use crate::program::node::{ClauseId, Head, Item, ItemId, ItemKind, RelArgs, RelRef};
use crate::program::scope::ScopeId;

/// A rule as the resolver lowered it.
pub struct RuleLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    /// The head, its rank not among its arguments.
    pub head: &'a Atom,
    pub rank: Option<Rank>,
    /// The clause of B, gathered; none for a fact.
    pub clause: Option<ClauseId>,
    /// B's literals, then from `seed` the reads the head hoisted.
    pub body: &'a [Lit],
    pub seed: usize,
    /// The helper statements the head's terms made.
    pub made: &'a [Stmt],
    /// The statement's aggregate values, and the head as the resolver
    /// folds it (its rank among its arguments).
    pub results: &'a [String],
    pub ranked: &'a Atom,
}

impl Builder<'_> {
    /// The `rule` item of `r`.
    pub fn rule_item(&mut self, r: RuleLowered) -> ItemId {
        let head = self.head(r.head, &r.body[r.seed..]);
        if let Some(c) = r.clause
            && !r.results.is_empty()
            && grouped(r.ranked, r.body, r.results)
        {
            number_folds(self.program, c);
        }
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
        if !r.made.is_empty() {
            self.program
                .terms_made
                .insert(NodeId::Item(item), r.made.to_vec());
        }
        item
    }

    /// The head `a`, after the reads `reads` its arguments hoisted.
    pub(super) fn head(&mut self, a: &Atom, reads: &[Lit]) -> Head {
        let rel = RelRef {
            name: a.pred.clone(),
            span: a.span,
        };
        let args = self.at(a.span, |b| match &a.record {
            Some(r) => RelArgs::Record(r.iter().map(|(k, t)| (k.clone(), b.pattern(t))).collect()),
            None => RelArgs::Positional(a.args.iter().map(|t| b.pattern(t)).collect()),
        });
        let reads = reads.iter().map(|l| self.goal(l)).collect();
        Head { rel, args, reads }
    }
}
