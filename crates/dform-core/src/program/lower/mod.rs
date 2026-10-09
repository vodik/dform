//! The program to the rules the engine runs (R-211): `lower` walks the
//! items from `roots` and emits, for each, the statements the resolver
//! has always emitted for it, the same relations (`arg`, `attr`, `want`,
//! `__path`, ..) for `partition::compile` and the engine. It is the one
//! consumer that combines the passes' tables, and the function the
//! resolver calls once the migration ends.
//!
//! An item is opaque (its statements, emitted as they are), a module
//! (`Stmt::Module` around its items' statements), or a ported statement:
//! `let k = LITERAL [@rank]` (step 3).

mod expr;
pub use expr::lower_expr;

use super::node::{ExprId, ItemId, ItemKind};
use super::{Origin, Program};
use crate::ast::{self, Atom, Rank, Span, Stmt, str_term};
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
        item(program, id, &mut statements, &mut origins);
    }
    LoweredStack {
        rules: Ok(ast::Program {
            statements,
            stack: program.stack.clone(),
        }),
        origins,
    }
}

/// The statements of the item `id`, onto `out`.
fn item(program: &Program, id: ItemId, out: &mut Vec<Stmt>, origins: &mut Vec<Origin>) {
    let it = &program.items[id];
    match &it.kind {
        ItemKind::Opaque(stmts) => {
            origins.extend(stmts.iter().map(|_| Origin::of(id)));
            out.extend(stmts.iter().cloned());
        }
        ItemKind::Module {
            path,
            component,
            items,
            ..
        } => {
            origins.push(Origin::of(id));
            let mut body = Vec::new();
            for &i in items {
                item(program, i, &mut body, origins);
            }
            out.push(Stmt::Module(ast::Module {
                name: path.clone(),
                component: *component,
                body,
                span: it.span,
            }));
        }
        ItemKind::Let {
            name,
            value,
            ty: None,
            clause: None,
            rank,
        } => {
            origins.push(Origin::of(id));
            out.push(Stmt::Fact(let_fact(program, name, *value, *rank, it.span)));
        }
        kind => unreachable!(
            "no builder makes {} with a type or a clause before step 5",
            super::spell::kind(kind)
        ),
    }
}

/// `let k = v [@rank]`: the contribution `let("k", v, "rank")` to the
/// cell `k`, which `modules` scopes.
fn let_fact(program: &Program, name: &str, value: ExprId, rank: Option<Rank>, span: Span) -> Atom {
    let (value, reads) = lower_expr(program, value);
    debug_assert!(reads.is_empty(), "a let of a literal reads nothing");
    let rank = rank.unwrap_or(Rank::Normal);
    ast::atom(
        crate::modules::LET,
        vec![str_term(name), value, str_term(rank.name())],
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::check;

    /// A program of opaque items lowers to the statements it was built
    /// from, spans, origins and module bodies included, and one built from
    /// diagnostics lowers to them.
    #[test]
    fn an_opaque_program_lowers_to_what_it_was_built_from() {
        let src = "\nlet x = 1\np(a) where a = x\ndeny \"no\" where p(2)\n";
        let lowered = crate::parser::parse_program(src).unwrap();
        let program = Program::of_statements(lowered.statements.clone());
        assert_eq!(program.roots.len(), lowered.statements.len());
        let back = lower(&program);
        assert_eq!(back.origins.len(), lowered.statements.len());
        assert_eq!(check::dump(&back.rules), check::dump(&Ok(lowered.clone())));

        let module = ast::Program {
            statements: vec![Stmt::Module(ast::Module {
                name: "m".into(),
                component: false,
                body: lowered.statements.clone(),
                span: Default::default(),
            })],
            stack: None,
        };
        let program = Program::of_statements(module.statements.clone());
        let ItemKind::Module { body, items, .. } = &program.items[program.roots[0]].kind else {
            panic!("not a module item");
        };
        assert_eq!(items.len(), lowered.statements.len());
        assert_eq!(program.scopes[*body].parent, Some(program.scope));
        assert_eq!(
            check::dump(&lower(&program).rules),
            check::dump(&Ok(module))
        );

        let err = vec![Diagnostic::error(Default::default(), "bad")];
        let program = Program {
            diags: err.clone(),
            ..Program::new()
        };
        assert_eq!(check::dump(&lower(&program).rules), check::dump(&Err(err)));
    }
}
