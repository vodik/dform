//! `deny "m" [{ .. }] [where B]` and `warn ..` as a `Check` item (R-211
//! step 5), from what the resolver lowered it to ([`CheckLowered`]): its
//! message and its detail the terms' nodes after the reads each hoisted
//! (the helper statements each one's terms made its own), its clause B's
//! goals as gathered; a clause whose aggregates fold through helpers
//! takes their numbers here, as a rule's does.

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Span, Stmt};
use crate::program::NodeId;
use crate::program::node::{CheckKind, ClauseId, ExprId, Item, ItemId, ItemKind};
use crate::program::scope::ScopeId;

/// A `deny` or a `warn` as the resolver lowered it.
pub struct CheckLowered<'a> {
    pub kind: CheckKind,
    pub span: Span,
    pub scope: ScopeId,
    /// The clause of B, gathered; none without `where`.
    pub clause: Option<ClauseId>,
    /// `deny(m[, detail])`, as the resolver wrote it.
    pub head: &'a Atom,
    /// B's literals, then the reads its message and its detail hoisted.
    pub body: &'a [Lit],
    /// The message's node and the detail's, as the resolver built them.
    pub message: ExprId,
    pub detail: Option<ExprId>,
    /// The helper statements the message's terms made, and the detail's.
    pub made: (&'a [Stmt], &'a [Stmt]),
    /// The statement's aggregate values.
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `Check` item of `c`.
    pub fn check_item(&mut self, c: CheckLowered) -> ItemId {
        let message = self.holding(c.message, c.made.0);
        let detail = c.detail.map(|d| self.holding(d, c.made.1));
        if let Some(cl) = c.clause
            && !c.results.is_empty()
            && grouped(c.head, c.body, c.results)
        {
            number_folds(self.program, cl);
        }
        let kind = ItemKind::Check {
            kind: c.kind,
            message,
            detail,
            clause: c.clause,
        };
        self.program.items.insert(Item {
            span: c.span,
            scope: c.scope,
            kind,
        })
    }

    /// `made`, but the helper statements lowering the nodes `nodes` makes
    /// itself (a comprehension's `not { }` rule).
    pub(super) fn made_but(&self, made: &[Stmt], nodes: &[ExprId]) -> Vec<Stmt> {
        let own: Vec<String> = nodes
            .iter()
            .flat_map(|&e| crate::program::lower::expr_helpers(self.program, e))
            .map(|s| format!("{s:?}"))
            .collect();
        made.iter()
            .filter(|s| !own.contains(&format!("{s:?}")))
            .cloned()
            .collect()
    }

    /// The term `t` after the reads it hoisted, the helper statements its
    /// terms made its own.
    pub(super) fn made(&mut self, t: &crate::ast::Term, reads: &[Lit], made: &[Stmt]) -> ExprId {
        let e = self.hoisted(t, reads);
        self.holding(e, made)
    }

    /// The node `e` the resolver built, the helper statements its terms
    /// made its own, but those its lowering makes itself (a
    /// comprehension's `not { }` rule).
    pub(super) fn holding(&mut self, e: ExprId, made: &[Stmt]) -> ExprId {
        let made = self.made_but(made, &[e]);
        if !made.is_empty() {
            self.program
                .terms_made
                .insert(NodeId::Expr(e), made.to_vec());
        }
        e
    }
}
