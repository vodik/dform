//! `input k: T [= d] [check B] [where G]`, `key k: T` and `input k { f:
//! T .. }` as `Input` items (R-211 step 5), from what the resolver read
//! of them ([`InputLowered`]): the default the literal's node, read as
//! the type; the refinement and the clause the goals of their bodies as
//! gathered; an object input's fields items of their own. A relation's
//! input ([`RelationInputLowered`]): a module's, or a stack's the rows of
//! a document, its source's reads the goals they lowered to.

use super::{Builder, grouped, number_folds};
use crate::ast::{Atom, Lit, Span, Stmt, Term, TypeExpr};
use crate::program::node::{ClauseId, Item, ItemId, ItemKind, RelRef};
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

/// A relation's input as the resolver lowered it: `input p` in a module;
/// `input p from src [where B]` in a stack, the rule `p(columns) :- B,
/// reads` and the externs reading the document.
pub struct RelationInputLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    pub rel: String,
    pub arity: usize,
    /// A stack's own relation (not a copy's row of its module's).
    pub mixed: bool,
    pub from: Option<RelationRows<'a>>,
}

/// The rows a stack's relation input reads.
pub struct RelationRows<'a> {
    pub clause: Option<ClauseId>,
    pub externs: Vec<Stmt>,
    /// `p(columns)`, as the resolver wrote it.
    pub head: &'a Atom,
    /// B's literals, then from `seed` the document's reads.
    pub body: &'a [Lit],
    pub seed: usize,
    /// The helper statements the document's terms made.
    pub made: &'a [Stmt],
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The `RelationInput` item of `r`.
    pub fn relation_input_item(&mut self, r: RelationInputLowered) -> ItemId {
        let (source, columns, clause) = match r.from {
            Some(rows) => {
                let source = self.source(rows.externs, &rows.body[rows.seed..], rows.made);
                let columns = rows
                    .head
                    .args
                    .iter()
                    .map(|t| match t {
                        Term::Var(v) => self.var(v),
                        t => unreachable!("a relation input's column is a variable: {t:?}"),
                    })
                    .collect();
                if let Some(c) = rows.clause
                    && !rows.results.is_empty()
                    && grouped(rows.head, rows.body, rows.results)
                {
                    number_folds(self.program, c);
                }
                (Some(source), columns, rows.clause)
            }
            None => (None, Vec::new(), None),
        };
        let kind = ItemKind::RelationInput {
            rel: RelRef {
                name: r.rel,
                span: r.span,
            },
            arity: r.arity,
            mixed: r.mixed,
            source,
            columns,
            clause,
        };
        self.program.items.insert(Item {
            span: r.span,
            scope: r.scope,
            kind,
        })
    }
}
