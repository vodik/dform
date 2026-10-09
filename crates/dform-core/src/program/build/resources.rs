//! `resource T n [@rank] { p = v .. } [where B]` and `resource T n = v`
//! as `Resource` items (R-211 step 5), from what the resolver lowered
//! them to ([`ResourceLowered`]): its type, its header (the name as
//! written, the literal's text, or a name from the clause bound after the
//! entries' reads), each entry's value the term's node after the reads it
//! hoisted (the helper statements its terms made its own), a value body
//! the term's node; the clause B's goals as gathered.

use super::Builder;
use crate::ast::{FieldOp, Lit, Rank, Span, Stmt, Term};
use crate::program::node::{
    ClauseId, Entry, Header, Item, ItemId, ItemKind, ResourceBody, TypeRef,
};
use crate::program::scope::ScopeId;

/// A resource block as the resolver lowered it.
pub struct ResourceLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    pub typ: TypeRef,
    pub name: HeaderLowered<'a>,
    pub rank: Option<Rank>,
    /// The clause of B, gathered; none without `where`.
    pub clause: Option<ClauseId>,
    pub body: BodyLowered<'a>,
}

/// A header as the resolver read it.
pub enum HeaderLowered<'a> {
    Bare(&'a str),
    Literal(String),
    /// From the clause: its variable, the reads its holes hoisted and the
    /// binding of its segment, and the helpers its terms made.
    Interp {
        value: &'a Term,
        reads: &'a [Lit],
        made: &'a [Stmt],
    },
}

/// A body as the resolver lowered it.
pub enum BodyLowered<'a> {
    Block(Vec<EntryLowered<'a>>),
    /// `= v` at `span`: its value, the reads it hoisted, the helpers its
    /// terms made.
    Value {
        span: Span,
        value: &'a Term,
        reads: &'a [Lit],
        made: &'a [Stmt],
    },
}

/// One entry: its path, value and rank, the reads its value hoisted and
/// the helpers its terms made.
pub struct EntryLowered<'a> {
    pub path: &'a str,
    pub op: FieldOp,
    pub value: &'a Term,
    pub rank: Option<Rank>,
    pub span: Span,
    pub reads: &'a [Lit],
    pub made: &'a [Stmt],
}

impl Builder<'_> {
    /// The `Resource` item of `r`.
    pub fn resource_item(&mut self, r: ResourceLowered) -> ItemId {
        let body = match r.body {
            BodyLowered::Block(entries) => ResourceBody::Block(
                entries
                    .iter()
                    .map(|e| Entry {
                        path: e.path.to_string(),
                        op: e.op,
                        value: self.at(e.span, |b| b.made(e.value, e.reads, e.made)),
                        rank: e.rank,
                        span: e.span,
                    })
                    .collect(),
            ),
            BodyLowered::Value {
                span,
                value,
                reads,
                made,
            } => ResourceBody::Value(self.at(span, |b| b.made(value, reads, made))),
        };
        let name = match r.name {
            HeaderLowered::Bare(n) => Header::Bare(n.to_string()),
            HeaderLowered::Literal(s) => Header::Literal(s),
            HeaderLowered::Interp { value, reads, made } => {
                Header::Interp(self.made(value, reads, made))
            }
        };
        let kind = ItemKind::Resource {
            typ: r.typ,
            name,
            rank: r.rank,
            body,
            clause: r.clause,
        };
        self.program.items.insert(Item {
            span: r.span,
            scope: r.scope,
            kind,
        })
    }
}
