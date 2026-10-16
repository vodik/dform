//! `set target (=|+=) v [@rank] [where B]`, `set { .. }` and `set from
//! doc` as items (R-211 step 5), from what the resolver lowered them to
//! ([`SetLowered`], [`SetFromLowered`]): each line's target the cell it
//! writes, its terms' nodes, the reads it hoisted in them (a `[_]`'s
//! binding among them), the value the term's node after the reads it
//! hoisted; the clause B's goals as
//! gathered, once for a block's lines. A clause whose aggregates fold
//! through helpers takes their numbers per line, as each line's rule
//! folds on its own.

use super::{Builder, fold_numbers, grouped};
use crate::ast::{Atom, FieldOp, Lit, Rank, Span, Stmt};
use crate::program::NodeId;
use crate::program::node::{
    ClauseId, ExprId, GoalId, Item, ItemId, ItemKind, Source, Target, VarId, Write,
};
use crate::program::scope::ScopeId;

/// A `set` as the resolver lowered it: its lines, under one clause.
pub struct SetLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    /// The clause of B, gathered; none without `where`.
    pub clause: Option<ClauseId>,
    pub writes: Vec<WriteLowered<'a>>,
    /// The statement's aggregate values.
    pub results: &'a [String],
}

/// One line of a `set` as the resolver lowered it.
pub struct WriteLowered<'a> {
    pub span: Span,
    pub target: TargetLowered<'a>,
    pub add: bool,
    pub rank: Option<Rank>,
    /// Its value's node, as the resolver built it.
    pub value: ExprId,
    /// The line's head as the resolver wrote it, and its body: B's
    /// literals, then the reads its target and its value hoisted.
    pub head: &'a Atom,
    pub body: &'a [Lit],
    /// The helper statements its target's terms made, and its value's.
    pub made: (&'a [Stmt], &'a [Stmt]),
}

/// A line's target as the resolver lowered it.
pub enum TargetLowered<'a> {
    Attr {
        typ: ExprId,
        addr: ExprId,
        path: &'a str,
    },
    Element {
        typ: ExprId,
        addr: ExprId,
        list: &'a str,
        key: ExprId,
        rest: &'a [String],
    },
    Input(&'a str),
}

/// `set from doc` as the resolver lowered it.
pub struct SetFromLowered<'a> {
    pub span: Span,
    pub scope: ScopeId,
    pub clause: Option<ClauseId>,
    pub rank: Option<Rank>,
    /// The leaf's path and value variables.
    pub path: &'a str,
    pub value: &'a str,
    /// The externs reading the document, the rule's body (B's literals,
    /// then the document's reads) and the goal reading its rows.
    pub externs: Vec<Stmt>,
    pub head: &'a Atom,
    pub body: &'a [Lit],
    pub rows: GoalId,
    /// The helper statements the document's terms made.
    pub made: &'a [Stmt],
    pub results: &'a [String],
}

impl Builder<'_> {
    /// The rows of a document: the externs reading it, and the goal of
    /// its table's extern, `rows`; the helpers its terms made the rows'.
    pub(super) fn source(&mut self, externs: Vec<Stmt>, rows: GoalId, made: &[Stmt]) -> Source {
        self.made_by(NodeId::Goal(rows), made);
        Source { externs, rows }
    }

    /// The `Set` item of `s`.
    pub fn set_item(&mut self, s: SetLowered) -> ItemId {
        let writes = s
            .writes
            .iter()
            .map(|w| self.write(w, s.clause, s.results))
            .collect();
        let kind = ItemKind::Set {
            writes,
            clause: s.clause,
        };
        self.program.items.insert(Item {
            span: s.span,
            scope: s.scope,
            kind,
        })
    }

    /// One line, its aggregates numbered when they fold through helpers.
    fn write(&mut self, w: &WriteLowered, clause: Option<ClauseId>, results: &[String]) -> Write {
        self.at(w.span, |b| {
            let target = match &w.target {
                TargetLowered::Attr { typ, addr, path } => Target::Attr {
                    typ: *typ,
                    addr: *addr,
                    path: path.to_string(),
                },
                TargetLowered::Element {
                    typ,
                    addr,
                    list,
                    key,
                    rest,
                } => Target::Element {
                    typ: *typ,
                    addr: *addr,
                    list: list.to_string(),
                    key: *key,
                    rest: rest.to_vec(),
                },
                TargetLowered::Input(path) => Target::Input {
                    path: path.to_string(),
                },
            };
            let value = w.value;
            // The helpers the target's terms made before the value's.
            let made_by = match &target {
                Target::Attr { typ, .. } | Target::Element { typ, .. } => vec![
                    (NodeId::Expr(*typ), w.made.0.to_vec()),
                    (NodeId::Expr(value), w.made.1.to_vec()),
                ],
                Target::Input { .. } => {
                    vec![(NodeId::Expr(value), [w.made.0, w.made.1].concat())]
                }
            };
            for (node, made) in made_by {
                b.made_by(node, &made);
            }
            let folds = match clause {
                Some(c) if !results.is_empty() && grouped(w.head, w.body, results) => {
                    fold_numbers(b.program, c)
                }
                _ => Vec::new(),
            };
            Write {
                target,
                op: if w.add { FieldOp::Add } else { FieldOp::Assign },
                value,
                rank: w.rank,
                folds,
                span: w.span,
            }
        })
    }

    /// The `SetFrom` item of `s`.
    pub fn set_from_item(&mut self, s: SetFromLowered) -> ItemId {
        let source = self.source(s.externs, s.rows, s.made);
        let (path, value): (VarId, VarId) = (self.var(s.path), self.var(s.value));
        if let Some(c) = s.clause
            && !s.results.is_empty()
            && grouped(s.head, s.body, s.results)
        {
            super::number_folds(self.program, c);
        }
        let kind = ItemKind::SetFrom {
            source,
            path,
            value,
            rank: s.rank,
            clause: s.clause,
        };
        self.program.items.insert(Item {
            span: s.span,
            scope: s.scope,
            kind,
        })
    }
}
