//! Building the program's nodes (R-211). A [`Builder`] makes the nodes
//! of one statement's terms in a [`Program`]'s arenas: every node gets a
//! span, and what the lowering would pick as it goes (a fresh variable's
//! name, a helper's number) is picked here and kept on the node, so
//! `lower` picks nothing.
//!
//! Step 3 builds a term from what the resolver lowered it to, its
//! `ast::Term` and the reads it hoisted ([`Builder::hoisted`]): every
//! form a term takes there has its node, and `lower` gives the term back
//! (`program::check` holds the two equal under `DFORM_CHECK_LOWER=1`).
//! The statement builders read the tree (step 5).

mod clause;
mod expr;
mod membership;
mod negation;
mod pattern;
mod spread;

pub use clause::{Form, Written};

use super::Program;
use super::node::{Expr, ExprId, ExprKind, ItemId, Var, VarId};
use crate::ast::Span;
use std::collections::BTreeMap;

/// The builder of one statement's nodes.
pub struct Builder<'p> {
    program: &'p mut Program,
    /// Where what is built from a lowered term stands: the term's span
    /// (its parts have none of their own there), or an atom's.
    span: Span,
    /// The item the variables belong to.
    item: Option<ItemId>,
    /// Each variable built, by the name it lowers to: two uses of one
    /// variable are one `VarId`.
    vars: BTreeMap<String, VarId>,
    /// Whether a lowered name is a variable the program wrote; any other
    /// the compiler made (`implicit`).
    written: &'p dyn Fn(&str) -> bool,
}

impl<'p> Builder<'p> {
    /// A builder into `program` of what is written at `span`.
    pub fn new(program: &'p mut Program, span: Span, written: &'p dyn Fn(&str) -> bool) -> Self {
        Builder {
            program,
            span,
            item: None,
            vars: BTreeMap::new(),
            written,
        }
    }

    /// The variables built belong to `item`.
    pub fn in_item(mut self, item: ItemId) -> Self {
        self.item = Some(item);
        self
    }

    pub(super) fn expr_node(&mut self, kind: ExprKind) -> ExprId {
        self.program.exprs.insert(Expr {
            span: self.span,
            kind,
        })
    }

    /// The variable that lowers to `lowered`, one per name.
    fn var(&mut self, lowered: &str) -> VarId {
        if let Some(&v) = self.vars.get(lowered) {
            return v;
        }
        let v = self.program.vars.insert(Var {
            name: crate::syntax::resolve::source_name(lowered),
            lowered: lowered.to_string(),
            first: self.span,
            item: self.item,
            implicit: !(self.written)(lowered),
        });
        self.vars.insert(lowered.to_string(), v);
        v
    }

    /// `f` with the nodes it builds at `span`.
    fn at<T>(&mut self, span: Span, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.span, span);
        let out = f(self);
        self.span = saved;
        out
    }
}
