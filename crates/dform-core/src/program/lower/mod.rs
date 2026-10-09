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
pub use clause::{lower_clause, lower_goal};
pub use expr::lower_expr;
pub use items::{element_write, lower_item, value_entries};

use super::{Origin, Program};
use crate::ast;
use crate::diag::Diagnostic;

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
        items::item(program, id, &mut statements, &mut origins);
    }
    LoweredStack {
        rules: Ok(ast::Program {
            statements,
            stack: program.stack.clone(),
        }),
        origins,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::check;

    /// A program the build found wrong lowers to its diagnostics.
    #[test]
    fn a_program_of_diagnostics_lowers_to_them() {
        let err = vec![Diagnostic::error(Default::default(), "bad")];
        let program = Program {
            diags: err.clone(),
            ..Program::new()
        };
        assert_eq!(check::dump(&lower(&program).rules), check::dump(&Err(err)));
    }
}
