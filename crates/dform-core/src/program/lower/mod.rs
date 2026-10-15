//! The program to the rules the engine runs (R-211): `lower` walks the
//! items from `roots` and emits, for each, the statements the resolver
//! has always emitted for it, the same relations (`arg`, `attr`, `want`,
//! `__path`, ..) for `partition::compile` and the engine. It is the one
//! consumer that combines the passes' tables, and the function the
//! resolver calls once the migration ends.
//!
//! An item is a module (`Stmt::Module` around its items' statements) or
//! a statement of its kind (`items.rs`).

mod clause;
mod expr;
mod items;
mod records;
pub use clause::{lower_clause, lower_goal};
pub(crate) use expr::stored;
pub use expr::{expr_helpers, lower_expr, read_expr};
pub use items::{element_write, lower_item, value_entries};
pub use records::Records;

use super::{Origin, Program};
use crate::ast;
use crate::diag::Diagnostic;
use std::cell::RefCell;

/// What the nodes are lowered under: the program's own, or, for the
/// evaluation, each record atom's fields in their columns by its scope's
/// `decl`s ([`Records`]), what is wrong with one said in `errors`.
#[derive(Clone, Copy)]
pub(crate) struct Env<'p> {
    pub program: &'p Program,
    pub records: Option<&'p Records>,
    pub errors: Option<&'p RefCell<Vec<Diagnostic>>>,
}

impl<'p> Env<'p> {
    /// The program's nodes as written.
    pub fn plain(program: &'p Program) -> Env<'p> {
        Env {
            program,
            records: None,
            errors: None,
        }
    }

    /// `atom` with its record's fields in their columns when this
    /// environment says them (`complete`: a head's); a field it cannot
    /// place is said, and the atom left as written.
    fn columns(&self, atom: ast::Atom, complete: bool) -> ast::Atom {
        let (Some(records), true) = (self.records, atom.record.is_some()) else {
            return atom;
        };
        match records.resolve(atom.clone(), complete) {
            Ok(a) => a,
            Err(d) => {
                if let Some(errors) = self.errors {
                    errors.borrow_mut().push(*d);
                }
                atom
            }
        }
    }
}

/// What the program lowers to: the resolver's output, and where each
/// statement came from, one origin per statement in the order a walk
/// meets them (a module's own, then its body's).
#[derive(Debug)]
pub struct LoweredStack {
    pub rules: Result<ast::Program, Vec<Diagnostic>>,
    pub origins: Vec<Origin>,
}

impl LoweredStack {
    pub fn into_result(self) -> Result<ast::Program, Vec<Diagnostic>> {
        self.rules
    }
}

pub fn lower(program: &Program) -> LoweredStack {
    if !program.diags.is_empty() {
        return LoweredStack {
            rules: Err(program.diags.clone()),
            origins: Vec::new(),
        };
    }
    let mut origins = Vec::new();
    let mut statements = Vec::new();
    for &id in &program.roots {
        items::item(Env::plain(program), id, &mut statements, &mut origins);
    }
    LoweredStack {
        rules: Ok(ast::Program {
            statements,
            stack: program.stack.clone(),
            nodes: None,
        }),
        origins,
    }
}

/// The statements of the item `id` as the evaluation lowers them: each
/// record atom's fields in their columns by the `decl`s of the scope it
/// is written in, `records` those around the item; or the first record
/// the walk meets that is wrong.
pub fn lower_placed(
    program: &Program,
    id: super::ItemId,
    records: &Records,
) -> Result<Vec<ast::Stmt>, Vec<Diagnostic>> {
    let errors = RefCell::new(Vec::new());
    let env = Env {
        program,
        records: Some(records),
        errors: Some(&errors),
    };
    let mut out = Vec::new();
    items::item(env, id, &mut out, &mut Vec::new());
    match errors.into_inner().into_iter().next() {
        None => Ok(out),
        Some(e) => Err(vec![e]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::dump::dump;

    /// A program the build found wrong lowers to its diagnostics.
    #[test]
    fn a_program_of_diagnostics_lowers_to_them() {
        let err = vec![Diagnostic::error(Default::default(), "bad")];
        let program = Program {
            diags: err.clone(),
            ..Program::new()
        };
        assert_eq!(dump(&lower(&program).rules), dump(&Err(err)));
    }
}
