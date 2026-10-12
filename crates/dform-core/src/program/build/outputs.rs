//! `output k[: T] = t [where B]`, `output k { f = t .. }` and `output p`
//! as items (R-211 step 5), from what the resolver lowered them to
//! ([`OutputLowered`]): the value the term's node after the reads it
//! hoisted (the helper statements its terms made its own), the clause B's
//! goals as gathered, the type written where the scope declares the
//! output (its first row); a relation exported its columns.

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Span, Stmt, TypeExpr};
use crate::program::node::{ClauseId, ExprId, Item, ItemId, ItemKind};
use crate::program::scope::ScopeId;

/// An output row as the resolver lowered it.
pub struct OutputLowered<'a> {
    pub name: String,
    /// Its type, at the row that declares the output.
    pub ty: Option<TypeExpr>,
    pub span: Span,
    pub scope: ScopeId,
    /// The clause of B, gathered; none without `where`.
    pub clause: Option<ClauseId>,
    /// `output(k, v)`, as the resolver wrote it.
    pub head: &'a Atom,
    /// B's literals, then the value's reads.
    pub body: &'a [Lit],
    /// The value's node, as the resolver built it.
    pub value: ExprId,
    /// The helper statements the value's terms made.
    pub made: &'a [Stmt],
    /// The statement's aggregate values.
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `Output` item of `o`.
    pub fn output_item(&mut self, o: OutputLowered) -> ItemId {
        let value = self.holding(o.value, o.made);
        if let Some(c) = o.clause
            && !o.results.is_empty()
            && grouped(o.head, o.body, o.results)
        {
            number_folds(self.program, c);
        }
        let kind = ItemKind::Output {
            name: o.name,
            ty: o.ty,
            value: Some(value),
            clause: o.clause,
        };
        self.program.items.insert(Item {
            span: o.span,
            scope: o.scope,
            kind,
        })
    }
}
