//! Items to the statements the resolver has always written for them (R-211
//! step 5): an opaque item's own, a module's `Stmt::Module` around its
//! items', and each ported statement's, one function per kind:
//!
//! | item                           | statements                                       |
//! |--------------------------------|--------------------------------------------------|
//! | `let k[: T] = v [@r] [where B]`| `let("k", v', "r") [:- B', reads]`, folded over B's aggregates; `__secret_let("k", "p")` per secret path; `decl k(k: T)` at the first typed row |
//! | `let f(a, b) = v [where B]`    | `f(A, B, v') :- demand(A, B), B', reads`, folded over B's aggregates; `extern demand/n`; `mode f/n+1` |
//! | `p(a, b) [@r] [where B]`       | `p(a', b'[, "r"]) [:- B', reads]`, folded over B's aggregates |
//! | `deny "m" [{..}] [where B]`    | `deny(m'[, d']) [:- B', reads]`, folded over B's aggregates |
//! | `input k: T [= d] [check B] [where G]` | itself, its fields', refinement's and clause's literals; under several clauses (R-104) where each holds and the deny where two do |
//! | `output k[: T] = v [where B]`  | its declaration at its first row; the value, or `output("k", v') :- B', reads` |
//! | `output p`                     | itself, its reference columns marked             |
//! | `input p`, `input p from src [where B]` | itself; the document's externs, `p(cols) :- B', reads`, `mixed p/n` |
//! | `resource T n [@r] { p = v .. } [where B]`, `= v` | itself: its entries (a value's the object's keys), its body B's literals then the entries' reads (`reads`), then a name from the clause's binding |
//! | `set t = v [@r] [where B]`, `set { .. }` | per line `arg(T, A, "p", v'[, "r"])` (`arg_add` for `+=`), an element `arg(T, A, "l", [k, {..}], "r")`, an input `arg("input", "", "k", v', "r")`, a fact or a rule over B's literals and the line's reads, folded over B's aggregates |
//! | `set from doc [@r] [where B]`  | the document's externs, then `arg("input", "", P, V, "r") :- B', reads` |
//! | `use p [as n] { k = v .. } [where B]` | the provider, its `source` (the first of its name); `provider_config("n", {k: v'}) [:- B', reads]`; `provider_expect_account("n", v') [:- B', reads]` each; under several clauses (R-104) `__declared` and the first's denies |
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
use crate::program::node::{
    CheckKind, ClauseId, ExprId, Head, Header, ItemId, ItemKind, Param, RelRef, ResourceBody,
    Setting, Source, Target, TypeRef, VarId, Write,
};
use crate::program::{NodeId, Origin, Program};
use crate::value::Value;
use std::collections::BTreeMap;

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
        ItemKind::LetFn {
            name,
            params,
            value,
            clause,
        } => let_fn(program, name, params, *value, *clause, it.span),
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
        ItemKind::RelationInput {
            rel,
            arity,
            source,
            columns,
            clause,
        } => relation_input(program, (rel, *arity), source.as_ref(), columns, *clause),
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
        ItemKind::Provider { .. } => provider(program, id),
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
        ItemKind::Resource {
            typ,
            name,
            rank,
            body,
            clause,
        } => resource(program, (typ, name, *rank), body, *clause, it.span),
        ItemKind::Set { writes, clause } => set(program, writes, *clause, it.span),
        ItemKind::SetFrom {
            source,
            path,
            value,
            rank,
            clause,
        } => set_from(program, source, (*path, *value), *rank, *clause, it.span),
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

/// `resource T n [@rank] { .. } [where B]`: the statement, its body B's
/// literals, then the entries' reads (its `reads`), then the binding of a
/// name from the clause (none when nothing is read or written after
/// `where`); then the helpers.
fn resource(
    program: &Program,
    (typ, name, rank): (&TypeRef, &Header, Option<Rank>),
    body: &ResourceBody,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let mut lits = Vec::new();
    if let Some(c) = clause {
        l.clause(c, &mut lits);
    }
    let start = lits.len();
    let fields = match body {
        ResourceBody::Block(entries) => entries
            .iter()
            .map(|e| {
                let value = l.expr(e.value);
                lits.append(&mut l.reads);
                ast::FieldAssign {
                    key: e.path.clone(),
                    op: e.op,
                    value,
                    rank: e.rank,
                    span: e.span,
                }
            })
            .collect(),
        ResourceBody::Value(v) => {
            let value = l.expr(*v);
            lits.append(&mut l.reads);
            value_entries(value, program.exprs[*v].span).expect("a value body is an object")
        }
    };
    let reads = start..lits.len();
    let name = match name {
        Header::Bare(n) => str_term(n),
        Header::Literal(s) => str_term(&crate::ir::name_segment(s)),
        Header::Interp(e) => {
            let t = l.expr(*e);
            lits.append(&mut l.reads);
            t
        }
    };
    let mut out = vec![Stmt::Resource(ast::Resource {
        typ: str_term(&typ.name),
        name,
        rank,
        fields,
        body: (!lits.is_empty()).then_some(lits),
        reads,
        span,
    })];
    out.append(&mut l.helpers);
    out
}

/// The entries of `resource T N = value` (R-126), each at `span`: an
/// object's keys (written out, or a literal), or any other value whole at
/// the root, an entry per key of the object it is when the rule runs
/// (`transform::resource_to_stmts`); none for a scalar or a list, which
/// is no value of a type.
pub fn value_entries(value: ast::Term, span: Span) -> Option<Vec<ast::FieldAssign>> {
    use ast::Term;
    let entry = |key: &str, value: Term| ast::FieldAssign {
        key: crate::ir::path_join("", key),
        op: ast::FieldOp::Assign,
        value,
        rank: None,
        span,
    };
    Some(match value {
        Term::Obj(m) => m.into_iter().map(|(k, v)| entry(&k, v)).collect(),
        Term::Val(Value::Obj(m)) => m
            .into_iter()
            .map(|(k, v)| entry(&k, Term::Val(v)))
            .collect(),
        Term::Val(_) | Term::List(_) => return None,
        value => vec![ast::FieldAssign {
            key: String::new(),
            op: ast::FieldOp::Assign,
            value,
            rank: None,
            span,
        }],
    })
}

/// `input p` (a module's relation, its user gives the rows): itself; `input
/// p from src [where B]`: the externs reading the document, the rule
/// `p(columns) :- B', reads` folded over B's aggregates, and `mixed p/n`
/// (rows from the document and from rules); then the helpers.
fn relation_input(
    program: &Program,
    (rel, arity): (&RelRef, usize),
    source: Option<&Source>,
    columns: &[VarId],
    clause: Option<ClauseId>,
) -> Vec<Stmt> {
    let e = ast::Extern {
        pred: rel.name.clone(),
        arity,
        span: rel.span,
    };
    let Some(source) = source else {
        return vec![Stmt::RelationInput(e)];
    };
    let mut l = Lowering::new(program);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    for &g in &source.reads {
        l.goal(g, &mut body);
    }
    let head = Atom {
        pred: rel.name.clone(),
        args: columns.iter().map(|v| l.var(*v)).collect(),
        record: None,
        span: rel.span,
    };
    let mut out = source.externs.clone();
    out.extend(clause_rule(head, body, folds, true, rel.span));
    out.push(Stmt::Mixed(e));
    out.append(&mut l.helpers);
    out
}

/// `let f(a, b) = v [where B]` (R-187): the rule `f(A, B, v') :-
/// demand(A, B), B', reads`, folded over B's aggregates, its parameters
/// bound by the demand its readers feed (`crate::demand`); the demand
/// declared `extern`, so a module's copy names it its own with the let;
/// the relation's mode; then the helpers.
fn let_fn(
    program: &Program,
    name: &str,
    params: &[Param],
    value: ExprId,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let mut args: Vec<ast::Term> = params.iter().map(|p| l.var(p.var)).collect();
    let demand = crate::demand::of(name);
    let seed = vec![ast::Lit::Pos(Atom {
        pred: demand.clone(),
        args: args.clone(),
        record: None,
        span,
    })];
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded_after(c, seed),
        None => (seed, Vec::new()),
    };
    args.push(l.expr(value));
    body.append(&mut l.reads);
    let arity = args.len();
    let head = Atom {
        pred: name.to_string(),
        args,
        record: None,
        span,
    };
    let mut out = clause_rule(head, body, folds, true, span);
    let relation = |pred: String, arity| ast::Extern { pred, arity, span };
    out.push(Stmt::Extern(relation(demand, arity - 1)));
    out.push(Stmt::Mode(relation(name.to_string(), arity)));
    out.append(&mut l.helpers);
    out
}

/// A provider's `use p [as n] { .. } [where B]`: when it starts the
/// provider, the provider with its `source`; its configuration
/// `provider_config("n", {k: v'}) [:- B', reads]` (a guarded provider's
/// with no settings too); each `expect_account`'s
/// `provider_expect_account("n", v') [:- B', reads]`; declared under
/// several clauses (R-104), where this one holds and, the first, the
/// denies where two do; then the helpers, in the order its parts made
/// them.
fn provider(program: &Program, id: ItemId) -> Vec<Stmt> {
    let it = &program.items[id];
    let ItemKind::Provider {
        name,
        of,
        starts,
        settings,
        clause,
        declared,
        denies,
    } = &it.kind
    else {
        unreachable!("a provider")
    };
    let span = it.span;
    let mut l = Lowering::new(program);
    let mut lits = Vec::new();
    if let Some(c) = clause {
        l.clause(*c, &mut lits);
    }
    let mut body = lits.clone();
    let mut source = Vec::new();
    let mut config = BTreeMap::new();
    let mut accounts = Vec::new();
    for s in settings {
        match s {
            Setting::Source { key, span, value } => {
                source.push((key.clone(), l.expr(*value), *span));
            }
            Setting::Value { key, value, .. } => {
                let v = l.expr(*value);
                body.append(&mut l.reads);
                config.insert(key.clone(), v);
            }
            Setting::Account {
                value,
                clause,
                span,
            } => {
                let mut b = Vec::new();
                if let Some(c) = clause {
                    l.clause(*c, &mut b);
                }
                let head = Atom {
                    pred: crate::plugin::providers::EXPECT_ACCOUNT.into(),
                    args: vec![str_term(name), l.expr(*value)],
                    record: None,
                    span: *span,
                };
                b.append(&mut l.reads);
                accounts.push((head, b));
            }
        }
    }
    let mut out = Vec::new();
    if *starts {
        out.push(Stmt::Provider(ast::Config {
            name: name.clone(),
            of: of.clone(),
            config: source,
            span,
        }));
    }
    if !config.is_empty() || clause.is_some() {
        let head = Atom {
            pred: "provider_config".into(),
            args: vec![str_term(name), ast::Term::Obj(config)],
            record: None,
            span,
        };
        out.extend(clause_rule(head, body, Vec::new(), false, span));
    }
    for (head, b) in accounts {
        out.extend(clause_rule(head, b, Vec::new(), false, span));
    }
    let group = format!("use {name}");
    if let Some(i) = declared {
        out.push(crate::modules::declared(&group, *i, lits, span));
    }
    if !denies.is_empty() {
        let sites: Vec<(String, Span)> = denies.iter().map(|s| (group.clone(), *s)).collect();
        out.extend(crate::modules::denies(&group, &sites));
    }
    out.append(&mut l.helpers);
    out
}

/// `set ..`: each line's write, a fact or a rule over B's literals (B
/// lowered once, its helpers once) and the line's reads, folded over B's
/// aggregates by the line's own numbers (their helpers at the
/// statement's `span`); then the helpers.
fn set(program: &Program, writes: &[Write], clause: Option<ClauseId>, span: Span) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let (lits, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    let mut out = Vec::new();
    for w in writes {
        let mut body = lits.clone();
        for &g in &w.reads {
            l.goal(g, &mut body);
        }
        let value = l.expr(w.value);
        body.append(&mut l.reads);
        let head = write_head(&mut l, w, value);
        let mut folds = folds.clone();
        for (f, n) in folds.iter_mut().zip(&w.folds) {
            f.2 = Some(*n);
        }
        out.extend(clause_rule(head, body, folds, clause.is_some(), span));
    }
    out.append(&mut l.helpers);
    out
}

/// A line's head: the cell its target is, `value` written to it.
fn write_head(l: &mut Lowering, w: &Write, value: ast::Term) -> Atom {
    let rank = || str_term(w.rank.unwrap_or(Rank::Normal).name());
    let args = match &w.target {
        Target::Attr { typ, addr, path } => {
            let mut args = vec![l.expr(*typ), l.expr(*addr), str_term(path), value];
            args.extend(w.rank.map(|r| str_term(r.name())));
            args
        }
        Target::Element {
            typ,
            addr,
            list,
            key,
            rest,
        } => {
            let (typ, addr, key) = (l.expr(*typ), l.expr(*addr), l.expr(*key));
            let elem = element_write(key, rest, value);
            vec![typ, addr, str_term(list), elem, rank()]
        }
        Target::Input { path } => vec![
            str_term(crate::modules::INPUT),
            str_term(""),
            str_term(path),
            value,
            rank(),
        ],
    };
    let pred = match w.op {
        ast::FieldOp::Add => "arg_add",
        ast::FieldOp::Assign => "arg",
    };
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span: w.span,
    }
}

/// What `set L[k].p.q = v` writes at the list `L` (R-35, R-69), before
/// the transform lowers it to the core's element write
/// (`transform::ELEM`): an object with the key under
/// `transform::ELEM_KEY` beside the element's content, `{"[key]": k, p:
/// {q: v}}`, so that `types::read` reads a quantity in it at its schema
/// path (`L.p.q`). Content that is not an object literal is held whole
/// under `transform::ELEM_VALUE`.
pub fn element_write(key: ast::Term, rest: &[String], value: ast::Term) -> ast::Term {
    use ast::Term;
    let content = rest
        .iter()
        .rev()
        .fold(value, |v, k| Term::Obj(BTreeMap::from([(k.clone(), v)])));
    let mut m = match content {
        Term::Obj(m) => m,
        Term::Val(Value::Obj(m)) => m.into_iter().map(|(k, v)| (k, Term::Val(v))).collect(),
        v => BTreeMap::from([(crate::transform::ELEM_VALUE.to_string(), v)]),
    };
    m.insert(crate::transform::ELEM_KEY.to_string(), key);
    Term::Obj(m)
}

/// `set from doc [@rank] [where B]`: the externs reading the document,
/// then `arg("input", "", Path, Value, "rank") :- B', reads`, folded over
/// B's aggregates.
fn set_from(
    program: &Program,
    source: &Source,
    (path, value): (VarId, VarId),
    rank: Option<Rank>,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(program);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    for &g in &source.reads {
        l.goal(g, &mut body);
    }
    let args = vec![
        str_term(crate::modules::INPUT),
        str_term(""),
        l.var(path),
        l.var(value),
        str_term(rank.unwrap_or(Rank::Normal).name()),
    ];
    let head = Atom {
        pred: "arg".into(),
        args,
        record: None,
        span,
    };
    let mut out = source.externs.clone();
    out.extend(clause_rule(head, body, folds, true, span));
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
