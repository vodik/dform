//! Items to the statements the resolver has always written for them (R-211
//! step 5): an opaque item's own, a module's `Stmt::Module` around its
//! items', and each ported statement's, one function per kind:
//!
//! | item                           | statements                                       |
//! |--------------------------------|--------------------------------------------------|
//! | `let k[: T] = v [@r] [where B]`| `let("k", v', "r") [:- B', reads]`, folded over B's aggregates; `__secret_let("k", "p")` per secret path; `decl k(k: T)` at the first typed row |
//! | `p(a, b) [@r] [where B]`       | `p(a', b'[, "r"]) [:- B', reads]`, folded over B's aggregates |
//! | `deny "m" [{..}] [where B]`    | `deny(m'[, d']) [:- B', reads]`, folded over B's aggregates |
//! | `input k: T [= d] [check B] [where G]` | itself, its fields', refinement's and clause's literals; under several clauses (R-104) where each holds and the deny where two do |
//! | `output k[: T] = v [where B]`  | its declaration at its first row; the value, or `output("k", v') :- B', reads` |
//! | `output p`                     | itself, its reference columns marked             |
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
use crate::program::node::{CheckKind, ClauseId, ExprId, Head, ItemId, ItemKind, RelRef};
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
        ItemKind::Check {
            kind,
            message,
            detail,
            clause,
        } => check(program, *kind, (*message, *detail), *clause, it.span),
        ItemKind::Input { name, rows, .. } => {
            let (decl, helpers) = input(program, id);
            let mut out = vec![Stmt::Input(decl.clone())];
            if let Some((i, sites)) = rows {
                // Declared under clauses (R-104): where each holds, and a
                // deny where two do.
                let group = format!("input {name}");
                out.push(crate::modules::declared(&group, *i, decl.guard, it.span));
                if *i == 0 {
                    let sites: Vec<(String, Span)> =
                        sites.iter().map(|s| (group.clone(), *s)).collect();
                    out.extend(crate::modules::denies(&group, &sites));
                }
            }
            out.extend(helpers);
            out
        }
        ItemKind::Output {
            name,
            ty,
            value,
            clause,
        } => output(program, (name, ty.as_ref()), *value, *clause, it.span),
        ItemKind::OutputRelation { rel, ref_columns } => vec![Stmt::Output(ast::OutputDecl {
            name: rel.name.clone(),
            ty: None,
            value: None,
            relation: Some(ref_columns.clone()),
            span: it.span,
        })],
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
        ItemKind::TypeBlock { name, attrs } => {
            let pending = Stmt::Pending(ast::Pending {
                kind: ast::PendingKind::TypeDecl {
                    name: name.clone(),
                    attrs: attrs.clone(),
                },
                span: it.span,
            });
            let made = program.terms_made.get(&NodeId::Item(id));
            std::iter::once(pending)
                .chain(made.into_iter().flatten().cloned())
                .collect()
        }
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
    let mut out = clause_rule(atom, body, folds, clause.is_some(), span);
    out.append(&mut l.helpers);
    out
}

/// `deny "m" [{ .. }] [where B]`, `warn ..`: `deny(m[, detail])`, a fact
/// or a rule over B's literals and the reads the message and the detail
/// hoisted, its aggregates folded.
fn check(
    program: &Program,
    kind: CheckKind,
    (message, detail): (ExprId, Option<ExprId>),
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    let mut args = vec![l.expr(message)];
    body.append(&mut l.reads);
    if let Some(d) = detail {
        args.push(l.expr(d));
        body.append(&mut l.reads);
    }
    let pred = match kind {
        CheckKind::Deny => "deny",
        CheckKind::Warn => "warn",
    };
    let head = Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span,
    };
    let mut out = clause_rule(head, body, folds, clause.is_some(), span);
    out.append(&mut l.helpers);
    out
}

/// `output k[: T] = v [where B]`: the output declared (at its first
/// row), then its value: as it is when nothing is read, else the rule
/// `output("k", v) :- B', reads`, folded over B's aggregates.
fn output(
    program: &Program,
    (name, ty): (&str, Option<&TypeExpr>),
    value: Option<ExprId>,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let decl = |ty: Option<TypeExpr>, value| {
        Stmt::Output(ast::OutputDecl {
            name: name.to_string(),
            ty,
            value,
            relation: None,
            span,
        })
    };
    let mut out: Vec<Stmt> = ty
        .map(|t| decl(Some(t.clone()), None))
        .into_iter()
        .collect();
    let Some(value) = value else {
        return out;
    };
    let mut l = Lowering::new(program);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    let v = l.expr(value);
    body.append(&mut l.reads);
    if body.is_empty() && clause.is_none() {
        out.push(decl(None, Some(v)));
    } else {
        let head = Atom {
            pred: "output".into(),
            args: vec![str_term(name), v],
            record: None,
            span,
        };
        out.extend(clause_rule(head, body, folds, true, span));
    }
    out.append(&mut l.helpers);
    out
}

/// The input `id` declared, and the helper statements its fields', its
/// refinement's and its clause's bodies made, in that order.
fn input(program: &Program, id: ItemId) -> (ast::InputDecl, Vec<Stmt>) {
    let it = &program.items[id];
    let ItemKind::Input {
        name,
        key,
        ty,
        default,
        refinement,
        guard,
        fields,
        ..
    } = &it.kind
    else {
        unreachable!("an input")
    };
    let mut helpers = Vec::new();
    let fields = fields
        .iter()
        .map(|&f| {
            let (decl, made) = input(program, f);
            helpers.extend(made);
            decl
        })
        .collect();
    let default = default.map(|d| super::lower_expr(program, d).0);
    let mut body = |c: &Option<ClauseId>| match c {
        Some(c) => {
            let (lits, made) = super::lower_clause(program, *c, &[]);
            helpers.extend(made);
            lits
        }
        None => Vec::new(),
    };
    let refinement = body(refinement);
    let guard = body(guard);
    let decl = ast::InputDecl {
        name: name.clone(),
        ty: ty.clone(),
        default,
        refinement,
        key: *key,
        guard,
        fields,
        span: it.span,
    };
    (decl, helpers)
}

/// `head` over `body`: a fact when nothing is written after `where` and
/// nothing read, else a rule, folded over `folds`.
fn clause_rule(
    head: Atom,
    body: Vec<ast::Lit>,
    folds: Vec<super::clause::Fold>,
    has_clause: bool,
    span: Span,
) -> Vec<Stmt> {
    match (folds.is_empty(), has_clause) {
        (false, _) => folded(head, body, folds, span),
        (true, false) if body.is_empty() => vec![Stmt::Fact(head)],
        (true, _) => vec![Stmt::Rule(RuleStmt::new(head, body))],
    }
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
        let mut out = clause_rule(head, body, folds, self.clause.is_some(), span);
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
