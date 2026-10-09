//! Items to the statements the resolver has always written for them (R-211
//! step 5): an opaque item's own, a module's `Stmt::Module` around its
//! items', and each ported statement's, one function per kind:
//!
//! | item                           | statements                                       |
//! |--------------------------------|--------------------------------------------------|
//! | `let k[: T] = v [@r] [where B]`| `let("k", v', "r") [:- B', reads]`, folded over B's aggregates; `__secret_let("k", "p")` per secret path; `decl k(k: T)` at the first typed row |
//!
//! Then the helper statements its clause and terms made, in the order
//! they made them.

use super::clause::folded;
use super::expr::Lowering;
use crate::ast::{self, Atom, Decl, Rank, RuleStmt, Span, Stmt, TypeExpr, str_term};
use crate::program::node::{ClauseId, ExprId, ItemId, ItemKind};
use crate::program::{Origin, Program};

/// The statements of the item `id`, onto `out`, one origin each.
pub(super) fn item(program: &Program, id: ItemId, out: &mut Vec<Stmt>, origins: &mut Vec<Origin>) {
    let it = &program.items[id];
    let stmts = match &it.kind {
        ItemKind::Opaque(stmts) => stmts.clone(),
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
            return;
        }
        ItemKind::Let {
            name,
            ty,
            value,
            clause,
            rank,
            declares,
        } => {
            let l = Let {
                name,
                ty: ty.as_ref(),
                value: *value,
                clause: *clause,
                rank: rank.unwrap_or(Rank::Normal),
                declares: *declares,
            };
            l.statements(program, it.span)
        }
        kind => unreachable!(
            "no builder makes {} before its step",
            super::super::spell::kind(kind)
        ),
    };
    origins.extend(stmts.iter().map(|_| Origin::of(id)));
    out.extend(stmts);
}

/// The statements of the item `id` alone: what it lowers to.
pub fn lower_item(program: &Program, id: ItemId) -> Vec<Stmt> {
    let mut out = Vec::new();
    item(program, id, &mut out, &mut Vec::new());
    out
}

/// A `let` item's parts.
struct Let<'p> {
    name: &'p str,
    ty: Option<&'p TypeExpr>,
    value: ExprId,
    clause: Option<ClauseId>,
    rank: Rank,
    declares: bool,
}

impl Let<'_> {
    /// `let k = v [@rank] [where B]`: the contribution `let("k", v,
    /// "rank")` to the cell `k`, which `modules` scopes, a rule over B and
    /// the value's reads, its aggregates folded; the cell's secret paths
    /// (R-153); its type `decl k(k: T)`, as R-34 types a relation, at the
    /// first typed row.
    fn statements(&self, program: &Program, span: Span) -> Vec<Stmt> {
        let mut l = Lowering::new(program);
        let (mut body, folds) = match self.clause {
            Some(c) => l.unfolded(c),
            None => Default::default(),
        };
        let value = l.expr(self.value);
        body.append(&mut l.reads);
        let head = Atom {
            pred: crate::modules::LET.to_string(),
            args: vec![str_term(self.name), value, str_term(self.rank.name())],
            record: None,
            span,
        };
        let mut out = match (folds.is_empty(), self.clause) {
            (false, _) => folded(head, body, folds, span),
            (true, None) if body.is_empty() => vec![Stmt::Fact(head)],
            (true, _) => vec![Stmt::Rule(RuleStmt::new(head, body))],
        };
        for (q, _) in self.ty.iter().flat_map(|d| crate::types::secret_fields(d)) {
            out.push(Stmt::Fact(Atom {
                pred: crate::modules::SECRET_LET.into(),
                args: vec![str_term(self.name), str_term(&q)],
                record: None,
                span,
            }));
        }
        if let (true, Some(d)) = (self.declares, self.ty) {
            out.push(Stmt::Decl(Decl {
                pred: self.name.to_string(),
                fields: vec![self.name.to_string()],
                types: vec![Some(d.clone())],
                span,
            }));
        }
        out.append(&mut l.helpers);
        out
    }
}
