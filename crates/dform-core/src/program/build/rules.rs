//! `p(a, b) [@rank] [where B]` as a `Rule` item (R-211 step 5), from what
//! the resolver lowered it to ([`RuleLowered`]): its head the relation and
//! its arguments' patterns, the reads they hoisted the head's; its clause
//! B's goals as gathered; a fact has none. A clause whose aggregates fold
//! through helpers takes their numbers here.

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Rank, Span, Stmt};
use crate::program::NodeId;
use crate::program::node::{
    ClauseId, ExprId, Head, Item, ItemId, ItemKind, PatternId, PatternKind, RelArgs, RelRef,
};
use crate::program::scope::ScopeId;

/// A rule as the resolver lowered it.
pub struct RuleLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    /// The head, its rank not among its arguments; its arguments'
    /// patterns, built of its nodes.
    pub head: &'a Atom,
    pub rel: RelRef,
    pub args: RelArgs,
    pub rank: Option<Rank>,
    /// The clause of B, gathered; none for a fact.
    pub clause: Option<ClauseId>,
    /// B's literals, then the reads the head hoisted.
    pub body: &'a [Lit],
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
        let head = Head {
            rel: r.rel,
            args: r.args,
        };
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
        let nodes = self.head_nodes(item);
        let made = self.made_but(r.made, &nodes);
        if !made.is_empty() {
            self.program.terms_made.insert(NodeId::Item(item), made);
        }
        item
    }

    /// The terms of rule `item`'s head.
    fn head_nodes(&self, item: ItemId) -> Vec<ExprId> {
        let ItemKind::Rule { head, .. } = &self.program.items[item].kind else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut walk: Vec<PatternId> = match &head.args {
            RelArgs::Positional(ps) => ps.clone(),
            RelArgs::Record(ps) => ps.iter().map(|(_, p)| *p).collect(),
        };
        while let Some(p) = walk.pop() {
            match &self.program.patterns[p].kind {
                PatternKind::Expr(e) => out.push(*e),
                PatternKind::Tuple { elems, .. } => walk.extend(elems),
                PatternKind::Object { fields, .. } => walk.extend(fields.iter().map(|f| f.2)),
                PatternKind::Hole | PatternKind::Bind(_) => {}
            }
        }
        out
    }
}
