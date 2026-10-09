//! `input k: T [= d] [check B] [where G]`, `key k: T` and `input k { f:
//! T .. }` as `Input` items (R-211 step 5), from what the resolver read
//! of them ([`InputLowered`]): the default the literal's node, read as
//! the type; the refinement and the clause the goals of their bodies as
//! gathered; an object input's fields items of their own.

use super::Builder;
use crate::ast::{Span, Term, TypeExpr};
use crate::program::node::{ClauseId, Item, ItemId, ItemKind};
use crate::program::scope::ScopeId;

/// An input as the resolver read it.
pub struct InputLowered<'a> {
    pub name: String,
    pub key: bool,
    pub ty: TypeExpr,
    pub default: Option<&'a Term>,
    pub refinement: Option<ClauseId>,
    pub guard: Option<ClauseId>,
    pub fields: Vec<ItemId>,
    pub rows: Option<(usize, Vec<Span>)>,
    pub span: Span,
    pub scope: ScopeId,
}

impl Builder<'_> {
    /// The `Input` item of `i`.
    pub fn input_item(&mut self, i: InputLowered) -> ItemId {
        let default = i.default.map(|d| self.expr(d));
        let kind = ItemKind::Input {
            name: i.name,
            key: i.key,
            ty: i.ty,
            default,
            refinement: i.refinement,
            guard: i.guard,
            fields: i.fields,
            rows: i.rows,
        };
        self.program.items.insert(Item {
            span: i.span,
            scope: i.scope,
            kind,
        })
    }
}
