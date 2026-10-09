//! A copy, `use m [as n] { .. } [where B]` or `resource C n { .. }`, as a
//! `Copy` item (R-211 step 5), from what the resolver lowered it to
//! ([`CopyLowered`]): a name from its clause (R-191) the variable bound
//! after the reads its holes hoisted, each input's value the term's node
//! after the reads it hoisted (the helper statements its terms made its
//! own), the rows its block gives (items built as rules and relation
//! inputs are), its clause B's goals as gathered.

use super::Builder;
use crate::ast::{self, Lit, Span, Stmt, Term};
use crate::program::node::{ClauseId, CopyKind, Item, ItemId, ItemKind};
use crate::program::scope::ScopeId;

/// A copy as the resolver lowered it.
pub struct CopyLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    pub kind: CopyKind,
    pub module: String,
    pub name: String,
    /// A name from the clause: its variable, the reads its holes hoisted
    /// and the binding of its segment, the helpers its terms made.
    pub named: Option<(&'a Term, &'a [Lit], &'a [Stmt])>,
    pub inputs: Vec<InputGiven<'a>>,
    pub rows: Vec<ItemId>,
    pub clause: Option<ClauseId>,
    pub via: Option<ast::Via>,
}

/// An input a copy's block gives: its value, the reads it hoisted, the
/// helpers its terms made.
pub struct InputGiven<'a> {
    pub key: String,
    pub span: Span,
    pub value: &'a Term,
    pub reads: &'a [Lit],
    pub made: &'a [Stmt],
}

impl Builder<'_> {
    /// The `Copy` item of `c`.
    pub fn copy_item(&mut self, c: CopyLowered) -> ItemId {
        let named = c
            .named
            .map(|(value, reads, made)| self.made(value, reads, made));
        let inputs = c
            .inputs
            .iter()
            .map(|i| {
                let value = self.at(i.span, |b| b.made(i.value, i.reads, i.made));
                (i.key.clone(), i.span, value)
            })
            .collect();
        let kind = ItemKind::Copy {
            kind: c.kind,
            module: c.module,
            name: c.name,
            named,
            inputs,
            rows: c.rows,
            clause: c.clause,
            via: c.via,
        };
        self.program.items.insert(Item {
            span: c.span,
            scope: c.scope,
            kind,
        })
    }
}
