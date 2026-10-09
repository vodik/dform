//! Items to the statements the resolver has always written for them (R-211
//! step 5): an opaque item's own, a module's `Stmt::Module` around its
//! items', and each ported statement's, one function per kind:
//!
//! | item                           | statements                                       |
//! |--------------------------------|--------------------------------------------------|
//! | `let k[: T] = v [@r] [where B]`| `let("k", v', "r") [:- B', reads]`, folded over B's aggregates; `__secret_let("k", "p")` per secret path; `decl k(k: T)` at the first typed row |
//! | `p(a, b) [@r] [where B]`       | `p(a', b'[, "r"]) [:- B', reads]`, folded over B's aggregates |
//! | `decl p(a: T) [mixed]`         | `mixed p/1` or (fed from outside) `extern p/1`, then `decl p(a: T)` |
//! | `extern f(+a: T, -b)`          | itself                                           |
//! | `type T { .. }`                | itself, pending (`PendingKind::TypeDecl`)        |
//! | a doc comment                  | `doc(kind, name, key, value)` per pair            |
//!
//! Then the helper statements its clause and terms made, in the order
//! they made them.

use super::clause::folded;
use super::expr::Lowering;
use crate::ast::{self, Atom, Decl, Rank, RuleStmt, Span, Stmt, TypeExpr, str_term};
use crate::program::node::{ClauseId, ExprId, Head, ItemId, ItemKind, RelRef};
use crate::program::{NodeId, Origin, Program};

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
        ItemKind::Rule { head, clause, rank } => rule(program, id, (head, *rank), *clause, it.span),
        ItemKind::Decl {
            rel,
            columns,
            mixed,
            fed,
        } => decl(rel, columns, *mixed, *fed, it.span),
        ItemKind::Extern { name, args } => vec![Stmt::ExternFn(ast::ExternFn {
            name: name.clone(),
            args: args.clone(),
            span: it.span,
        })],
        ItemKind::TypeBlock { name, attrs } => vec![Stmt::Pending(ast::Pending {
            kind: ast::PendingKind::TypeDecl {
                name: name.clone(),
                attrs: attrs.clone(),
            },
            span: it.span,
        })],
        ItemKind::Doc { kind, name, pairs } => pairs
            .iter()
            .map(|(k, v)| {
                let args = [*kind, name, k, v].map(str_term).to_vec();
                Stmt::Fact(Atom {
                    pred: "doc".into(),
                    args,
                    record: None,
                    span: it.span,
                })
            })
            .collect(),
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

/// `p(a, b) [@rank] [where B]`: a fact, or a rule over B's literals and
/// the reads the head hoisted, its aggregates folded; a rank the head's
/// last argument (`arg(T, A, p, v, rank)`).
fn rule(
    program: &Program,
    id: ItemId,
    (head, rank): (&Head, Option<Rank>),
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let mut atom = l.atom(&head.rel, &head.args);
    atom.args.extend(rank.map(|r| str_term(r.name())));
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    for &g in &head.reads {
        l.goal(g, &mut body);
    }
    l.terms_made(NodeId::Item(id));
    let mut out = match (folds.is_empty(), clause) {
        (false, _) => folded(atom, body, folds, span),
        (true, None) if body.is_empty() => vec![Stmt::Fact(atom)],
        (true, _) => vec![Stmt::Rule(RuleStmt::new(atom, body))],
    };
    out.append(&mut l.helpers);
    out
}

/// `decl p(a: T, ..) [mixed]` (H-11): the relation's columns, after
/// `mixed` (facts and rules both) or, fed from outside, `extern`.
fn decl(
    rel: &RelRef,
    columns: &[(String, Option<TypeExpr>)],
    mixed: bool,
    fed: bool,
    span: Span,
) -> Vec<Stmt> {
    let e = ast::Extern {
        pred: rel.name.clone(),
        arity: columns.len(),
        span,
    };
    let mut out = Vec::new();
    match (mixed, fed) {
        (true, _) => out.push(Stmt::Mixed(e)),
        (false, true) => out.push(Stmt::Extern(e)),
        (false, false) => {}
    }
    let (fields, types) = columns.iter().cloned().unzip();
    out.push(Stmt::Decl(Decl {
        pred: rel.name.clone(),
        fields,
        types,
        span,
    }));
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
