//! Building the program's nodes (R-211). A [`Builder`] makes the nodes
//! of one statement's terms in a [`Program`]'s arenas: every node gets a
//! span, and what the lowering would pick as it goes (a fresh variable's
//! name, a helper's number) is picked here and kept on the node, so
//! `lower` picks nothing.
//!
//! The resolver builds each term it reads off the tree as its node
//! (step 7), each literal's goal of those nodes and each statement's item
//! of them (step 7c), lowering them for the literals its own analyses
//! still read. A term is built here from what the resolver lowered it to,
//! its `ast::Term` and the reads it hoisted ([`Builder::hoisted`]), only
//! for what the resolver writes as a term: a comprehension outside a
//! statement.

mod aggregate;
mod checks;
mod clause;
mod contributions;
mod expr;
mod gather;
mod hoist;
mod inputs;
mod lets;
mod membership;
mod modules;
mod outputs;
mod pattern;
mod providers;
mod resources;
mod rules;
mod spread;

pub use aggregate::{fold_numbers, grouped, number_folds};
pub use checks::CheckLowered;
pub use contributions::{SetFromLowered, SetLowered, TargetLowered, WriteLowered};
pub use gather::{Gather, Statement};
pub use hoist::{Keyed, ReadOf};
pub use inputs::{InputLowered, RelationInputLowered, RelationRows};
pub use lets::{LetFnLowered, LetLowered};
pub use modules::{CopyLowered, InputGiven};
pub use outputs::OutputLowered;
pub use providers::{ProviderLowered, SettingKind, SettingLowered};
pub use resources::{BodyLowered, EntryLowered, HeaderLowered, ResourceLowered};
pub use rules::RuleLowered;

use super::Program;
use super::node::{ExprId, ExprKind, Var, VarId};
use crate::ast::Span;
use std::collections::BTreeMap;

/// The builder of one statement's nodes.
pub struct Builder<'p> {
    program: &'p mut Program,
    /// Where what is built from a lowered term stands: the term's span
    /// (its parts have none of their own there), or an atom's.
    span: Span,
    /// Each variable built, by the name it lowers to: two uses of one
    /// variable are one `VarId`.
    vars: BTreeMap<String, VarId>,
    /// Whether a lowered name is a variable the program wrote; any other
    /// the compiler made (`implicit`).
    written: &'p dyn Fn(&str) -> bool,
    /// What the statement's reads hold.
    reads: Reads,
}

/// What the reads of the statement built so far hold.
#[derive(Default)]
pub struct Reads {
    /// The copies' scopes the reads test the copy in (an `Output` with
    /// its test, `of`): the resolver's test before such a read is the
    /// read's.
    held: std::collections::BTreeSet<ExprId>,
}

impl<'p> Builder<'p> {
    /// A builder into `program` of what is written at `span`.
    pub fn new(program: &'p mut Program, span: Span, written: &'p dyn Fn(&str) -> bool) -> Self {
        Builder {
            program,
            span,
            vars: BTreeMap::new(),
            written,
            reads: Reads::default(),
        }
    }

    /// The builder of a statement whose variables built so far are
    /// `vars`, and whose hoisted reads read `reads`.
    pub fn of_statement(mut self, vars: BTreeMap<String, VarId>, reads: Reads) -> Self {
        self.vars = vars;
        self.reads = reads;
        self
    }

    /// The variables built, by the name each lowers to, and the
    /// statement's reads.
    pub fn into_statement(self) -> (BTreeMap<String, VarId>, Reads) {
        (self.vars, self.reads)
    }

    pub(super) fn expr_node(&mut self, kind: ExprKind) -> ExprId {
        self.program.expr_node(self.span, kind)
    }

    /// The variable that lowers to `lowered`, one per name.
    pub fn var(&mut self, lowered: &str) -> VarId {
        if let Some(&v) = self.vars.get(lowered) {
            return v;
        }
        let v = self.program.vars.insert(Var {
            name: crate::syntax::resolve::source_name(lowered),
            lowered: lowered.to_string(),
            first: self.span,
            item: None,
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
