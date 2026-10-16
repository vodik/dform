//! Items to the statements the resolver has always written for them (R-211
//! step 5): a module's `Stmt::Module` around its items', and each
//! statement's, one function per kind:
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
//! | `input p`, `input p from src [where B]` | itself; the document's externs, `p(cols) :- B', reads`, `mixed p/n` (a stack's own) |
//! | `use m { k = v, p(..) } [where B]`, `resource C n { .. }` | `use`/the copy: its inputs, its rows' own statements, its clause and body |
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

use super::Env;
use super::clause::folded;
use super::expr::Lowering;
use crate::ast::{self, Atom, Decl, Rank, RuleStmt, Span, Stmt, TypeExpr, str_term};
use crate::program::node::{
    CheckKind, ClauseId, CopyKind, ExprId, Head, Header, ItemId, ItemKind, Param, RelRef,
    ResourceBody, Setting, Source, Target, TypeAttr, TypeRef, VarId, Write,
};
use crate::program::{NodeId, Program};
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The statements of the item `id`, onto `out`.
pub(super) fn item(env: Env, id: ItemId, out: &mut Vec<Stmt>) {
    if let ItemKind::Module { .. } = &env.program.items[id].kind {
        return module(env, id, out);
    }
    out.extend(statements(env, id));
}

/// Each rule of `s` (a copy's rows' too) lowered out of the statement
/// written at `span`, in the instance its head is lowered in, `through`
/// the rule a `set` is unfolded with; a rule that has its origin keeps
/// it.
fn stamp(s: &mut Stmt, span: Span, through: Option<Span>) {
    match s {
        Stmt::Rule(r) if r.origin.is_none() => {
            r.origin = Some(ast::Origin {
                stmt: span.within(r.head.span.origin),
                call: None,
                through,
            })
        }
        Stmt::Instance(i) | Stmt::Use(i) => {
            i.rows.iter_mut().for_each(|r| stamp(r, span, through));
        }
        _ => {}
    }
}

/// A module's file or a component: `Stmt::Module` around its items'
/// statements; its own `decl`s name its relations' fields inside it.
fn module(env: Env, id: ItemId, out: &mut Vec<Stmt>) {
    let program = env.program;
    let it = &program.items[id];
    let ItemKind::Module {
        path,
        component,
        items,
        ..
    } = &it.kind
    else {
        unreachable!("a module")
    };
    let records = env.records.map(|r| r.with_items(program, items));
    let env = Env {
        records: records.as_ref(),
        ..env
    };
    let mut body = Vec::new();
    for &i in items {
        item(env, i, &mut body);
    }
    out.push(Stmt::Module(ast::Module {
        name: path.clone(),
        component: *component,
        body,
        span: it.span,
    }));
}

/// The statements of the item `id`, which is not a module, each rule's
/// origin the item.
fn statements(env: Env, id: ItemId) -> Vec<Stmt> {
    let mut out = statements_of(env, id);
    let span = env.program.items[id].span;
    out.iter_mut().for_each(|s| stamp(s, span, None));
    out
}

fn statements_of(env: Env, id: ItemId) -> Vec<Stmt> {
    let it = &env.program.items[id];
    match &it.kind {
        ItemKind::Module { .. } => unreachable!("a module is its items' statements"),
        ItemKind::Let { .. } => Let::of(&it.kind).statements(env, it.span),
        ItemKind::LetFn {
            name,
            params,
            value,
            clause,
        } => let_fn(env, name, params, *value, *clause, it.span),
        ItemKind::Rule { .. } | ItemKind::RelationInput { .. } => {
            let mut l = Lowering::new(env);
            let mut out = own(&mut l, id);
            out.append(&mut l.helpers);
            out
        }
        ItemKind::Copy { .. } => copy(env, id),
        ItemKind::Check {
            kind,
            message,
            detail,
            clause,
        } => check(env, *kind, (*message, *detail), *clause, it.span),
        ItemKind::Input { name, rows, .. } => input_item(env, id, name, rows.as_ref()),
        ItemKind::Output {
            name,
            ty,
            value,
            clause,
        } => output(env, (name, ty.as_ref()), *value, *clause, it.span),
        ItemKind::OutputRelation { rel, ref_columns } => vec![Stmt::Output(ast::OutputDecl {
            name: rel.pred(),
            ty: None,
            value: None,
            relation: Some(ref_columns.clone()),
            span: it.span,
        })],
        ItemKind::Provider { .. } => provider(env, id),
        ItemKind::Decl {
            rel,
            columns,
            mixed,
            fed,
        } => {
            let l = Lowering::new(env);
            let out = decl(rel, columns, *mixed, *fed, it.span);
            out.into_iter().map(|s| l.put_stmt(s)).collect()
        }
        ItemKind::Extern { name, args } => vec![Stmt::ExternFn(ast::ExternFn {
            name: name.clone(),
            args: args.clone(),
            span: it.span,
        })],
        ItemKind::TypeBlock { name, attrs } => type_block(env, id, name, attrs),
        ItemKind::Resource {
            typ,
            name,
            rank,
            body,
            clause,
        } => resource(env, (typ, name, *rank), body, *clause, it.span),
        ItemKind::Set { writes, clause } => set(env, writes, *clause, it.span),
        ItemKind::SetFrom {
            source,
            path,
            value,
            rank,
            clause,
        } => set_from(env, source, (*path, *value), *rank, *clause, it.span),
        ItemKind::Doc { kind, name, pairs } => {
            let l = Lowering::new(env);
            doc(kind, name, pairs, it.span)
                .into_iter()
                .map(|s| l.put_stmt(s))
                .collect()
        }
    }
}

/// `input k: T ..` (the item `id`): itself; declared under clauses
/// (R-104), where each holds and, the first, a deny where two do; then
/// the helpers its parts made.
fn input_item(env: Env, id: ItemId, name: &str, rows: Option<&(usize, Vec<Span>)>) -> Vec<Stmt> {
    let span = env.program.items[id].span;
    let (decl, helpers) = input(env, id);
    let l = Lowering::new(env);
    let finish = |a: Atom| l.put(a);
    let mut out = vec![Stmt::Input(decl.clone())];
    if let Some((i, sites)) = rows {
        let group = format!("input {name}");
        out.push(crate::modules::declared_with(
            &group, *i, decl.guard, span, &finish,
        ));
        if *i == 0 {
            let sites: Vec<(String, Span)> = sites.iter().map(|s| (group.clone(), *s)).collect();
            out.extend(crate::modules::denies_with(&group, &sites, &finish));
        }
    }
    out.extend(helpers);
    out
}

/// `type T { .. }` (the item `id`): pending, then the helpers its
/// refinements made.
fn type_block(env: Env, id: ItemId, name: &str, attrs: &[TypeAttr]) -> Vec<Stmt> {
    let mut helpers = Vec::new();
    let attrs = attrs
        .iter()
        .map(|a| attr_decl(env, a, &mut helpers))
        .collect();
    let pending = Stmt::Pending(ast::Pending {
        kind: ast::PendingKind::TypeDecl {
            name: name.to_string(),
            attrs,
        },
        span: env.program.items[id].span,
    });
    let made = env.program.terms_made.get(&NodeId::Item(id));
    std::iter::once(pending)
        .chain(helpers)
        .chain(made.into_iter().flatten().cloned())
        .collect()
}

/// A type block's attribute declared, its refinement's literals, the
/// helper statements they made into `helpers`.
fn attr_decl(env: Env, a: &TypeAttr, helpers: &mut Vec<Stmt>) -> ast::AttrDecl {
    let refinement = match a.refinement {
        Some(c) => {
            let (lits, made) = super::clause::lower_clause_in(env, c, &[]);
            helpers.extend(made);
            lits
        }
        None => Vec::new(),
    };
    let children = a
        .children
        .iter()
        .map(|c| attr_decl(env, c, helpers))
        .collect();
    ast::AttrDecl {
        path: a.path.clone(),
        ty: a.ty.clone(),
        flags: a.flags.clone(),
        refinement,
        children,
        span: a.span,
    }
}

/// A doc comment's pairs: `doc(kind, name, key, value)` each.
fn doc(kind: &str, name: &str, pairs: &[(String, String)], span: Span) -> Vec<Stmt> {
    pairs
        .iter()
        .map(|(k, v)| {
            let args = [kind, name, k, v].map(str_term).to_vec();
            Stmt::Fact(Atom {
                pred: "doc".into(),
                args,
                record: None,
                span,
            })
        })
        .collect()
}

/// The statements of the item `id` alone: what it lowers to.
pub fn lower_item(program: &Program, id: ItemId) -> Vec<Stmt> {
    let mut out = Vec::new();
    item(Env::plain(program), id, &mut out);
    out
}

/// `p(a, b) [@rank] [where B]`: a fact, or a rule over B's literals and
/// the reads the head hoisted, its aggregates folded; a rank the head's
/// last argument (`arg(T, A, p, v, rank)`).
fn rule(
    l: &mut Lowering,
    id: ItemId,
    (head, rank): (&Head, Option<Rank>),
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut atom = l.head(&head.rel, &head.args);
    atom.args.extend(rank.map(|r| str_term(r.name())));
    // The head's reads, after the clause.
    let reads = std::mem::take(&mut l.reads);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    body.extend(reads);
    l.terms_made(NodeId::Item(id));
    clause_rule(l, atom, body, folds, clause.is_some(), span)
}

/// A rule's or a relation input's own statements, the helpers they make
/// left in `l`: a copy's rows are written inside it, their helpers after
/// it.
fn own(l: &mut Lowering, id: ItemId) -> Vec<Stmt> {
    let program = l.program;
    let it = &program.items[id];
    match &it.kind {
        ItemKind::Rule { head, clause, rank } => rule(l, id, (head, *rank), *clause, it.span),
        ItemKind::RelationInput {
            rel,
            arity,
            mixed,
            source,
            columns,
            clause,
        } => relation_input(l, (rel, *arity, *mixed), source.as_ref(), columns, *clause),
        k => unreachable!("{} is no row", super::super::spell::kind(k)),
    }
}

/// `use m { k = v, p(..) .. } [where B]`, `resource C n { .. } [where
/// B]`: the copy, its clause B's literals and a name from the clause's
/// binding (`clause`), those and the inputs' reads (`body`), its rows'
/// own statements; then the helpers, in the order its parts made them.
fn copy(env: Env, id: ItemId) -> Vec<Stmt> {
    let (copy, rows, helpers) = copy_parts(env, id, None);
    let copy = ast::Instance {
        rows: rows.into_iter().map(|(s, _)| s).collect(),
        ..copy
    };
    let used = matches!(
        &env.program.items[id].kind,
        ItemKind::Copy {
            kind: CopyKind::Use,
            ..
        }
    );
    let mut out = vec![match used {
        true => Stmt::Use(copy),
        false => Stmt::Instance(copy),
    }];
    out.extend(helpers);
    out
}

/// The copy `id` as its user writes it: the copy (its rows apart), each
/// row's statements, `true` for a relation `taken` names the copy's own
/// (its scope, the relations its definition takes, R-55), and the
/// helpers its parts made.
pub(super) fn copy_parts(
    env: Env,
    id: ItemId,
    taken: Option<(&str, &BTreeSet<String>)>,
) -> (ast::Instance, Vec<(Stmt, bool)>, Vec<Stmt>) {
    let program = env.program;
    let it = &program.items[id];
    let ItemKind::Copy {
        module,
        name,
        named,
        inputs,
        rows,
        clause,
        via,
        ..
    } = &it.kind
    else {
        unreachable!("a copy")
    };
    let mut l = Lowering::new(env);
    let mut lits = Vec::new();
    if let Some(c) = clause {
        l.clause(*c, &mut lits);
    }
    let named = named.map(|e| {
        let t = l.expr(e);
        lits.append(&mut l.reads);
        t
    });
    let clause = (!lits.is_empty()).then(|| lits.clone());
    let mut body = lits;
    let inputs = inputs
        .iter()
        .map(|(k, span, e)| {
            let v = l.expr(*e);
            body.append(&mut l.reads);
            (k.clone(), v, *span)
        })
        .collect();
    l.taken = taken;
    let mut out = Vec::new();
    for &r in rows {
        let takes = taken.is_some_and(|(_, t)| t.contains(&row_relation(program, r)));
        let stmts = own(&mut l, r);
        out.extend(stmts.into_iter().map(|s| {
            let taken = takes && matches!(s, Stmt::Fact(_) | Stmt::Rule(_));
            (s, taken)
        }));
    }
    l.taken = None;
    let copy = ast::Instance {
        module: module.clone(),
        name: name.clone(),
        named,
        inputs,
        rows: Vec::new(),
        body: (!body.is_empty()).then_some(body),
        clause,
        via: via.clone(),
        span: it.span,
    };
    (copy, out, std::mem::take(&mut l.helpers))
}

/// The relation a copy block's row `r` gives rows of.
fn row_relation(program: &Program, r: ItemId) -> String {
    match &program.items[r].kind {
        ItemKind::Rule { head, .. } => head.rel.pred(),
        ItemKind::RelationInput { rel, .. } => rel.pred(),
        k => unreachable!("{} is no row", super::super::spell::kind(k)),
    }
}

/// The name the copy `id` binds and its clause, as a check of a name
/// bound more than once (R-104) names it.
pub(super) fn alternative(env: Env, id: ItemId) -> crate::modules::Alternative {
    let it = &env.program.items[id];
    let ItemKind::Copy {
        kind,
        module,
        name,
        clause,
        ..
    } = &it.kind
    else {
        unreachable!("a copy")
    };
    let what = match kind {
        CopyKind::Use if module.rsplit('.').next() == Some(name.as_str()) => {
            format!("use {module}")
        }
        CopyKind::Use => format!("use {module} as {name}"),
        CopyKind::Component => format!("resource {module} {name}"),
    };
    let mut lits = Vec::new();
    if let Some(c) = clause {
        Lowering::new(env).clause(*c, &mut lits);
    }
    crate::modules::Alternative {
        what,
        clause: lits,
        span: it.span,
    }
}

/// The statements of the item `id` under `env`, which is not a module.
pub(super) fn statements_in(env: Env, id: ItemId) -> Vec<Stmt> {
    statements(env, id)
}

/// `deny "m" [{ .. }] [where B]`, `warn ..`: `deny(m[, detail])`, a fact
/// or a rule over B's literals and the reads the message and the detail
/// hoisted, its aggregates folded.
fn check(
    env: Env,
    kind: CheckKind,
    (message, detail): (ExprId, Option<ExprId>),
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(env);
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
    let head = l.put(Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span,
    });
    let mut out = clause_rule(&l, head, body, folds, clause.is_some(), span);
    out.append(&mut l.helpers);
    out
}

/// `output k[: T] = v [where B]`: the output declared (at its first
/// row), then its value: as it is when nothing is read, else the rule
/// `output("k", v) :- B', reads`, folded over B's aggregates.
fn output(
    env: Env,
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
    let mut l = Lowering::new(env);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    let v = l.expr(value);
    body.append(&mut l.reads);
    if body.is_empty() && clause.is_none() {
        out.push(decl(None, Some(v)));
    } else {
        let head = l.put(Atom {
            pred: "output".into(),
            args: vec![str_term(name), v],
            record: None,
            span,
        });
        out.extend(clause_rule(&l, head, body, folds, true, span));
    }
    out.append(&mut l.helpers);
    out
}

/// `resource T n [@rank] { .. } [where B]`: the statement, its body B's
/// literals, then the entries' reads (its `reads`), then the binding of a
/// name from the clause (none when nothing is read or written after
/// `where`); then the helpers.
fn resource(
    env: Env,
    (typ, name, rank): (&TypeRef, &Header, Option<Rank>),
    body: &ResourceBody,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(env);
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
            value_entries(value, env.program.exprs[*v].span).expect("a value body is an object")
        }
    };
    let reads = start..lits.len();
    let (name, name_at) = match name {
        Header::Bare(n) => (str_term(n), env.from),
        Header::Literal(s) => (str_term(&crate::address::name_segment(s)), env.from),
        Header::Interp(e) => {
            let at = l.start_of(*e);
            let t = l.expr(*e);
            lits.append(&mut l.reads);
            (t, at)
        }
    };
    let r = ast::Resource {
        typ: str_term(&typ.name),
        name,
        rank,
        fields,
        body: (!lits.is_empty()).then_some(lits),
        reads,
        span,
    };
    let r = match env.chain {
        Some(c) => c.resource(r, name_at),
        None => r,
    };
    let mut out = vec![Stmt::Resource(r)];
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
        key: crate::address::path_join("", key),
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
/// `p(columns) :- B', reads` folded over B's aggregates, and, a stack's
/// own, `mixed p/n` (rows from the document and from rules).
fn relation_input(
    l: &mut Lowering,
    (rel, arity, mixed): (&RelRef, usize, bool),
    source: Option<&Source>,
    columns: &[VarId],
    clause: Option<ClauseId>,
) -> Vec<Stmt> {
    let e = ast::Extern {
        pred: rel.pred(),
        arity,
        span: rel.span,
    };
    let Some(source) = source else {
        return vec![Stmt::RelationInput(e)];
    };
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    l.goal(source.rows, &mut body);
    let mut head = Atom {
        pred: rel.pred(),
        args: columns.iter().map(|v| l.var(*v)).collect(),
        record: None,
        span: rel.span,
    };
    if let Some((scope, takes)) = l.taken
        && takes.contains(&head.pred)
    {
        head.pred = super::instances::private_name(scope, &head.pred);
    }
    let head = l.put(head);
    let mut out: Vec<Stmt> = source
        .externs
        .iter()
        .map(|s| l.put_stmt(s.clone()))
        .collect();
    out.extend(clause_rule(l, head, body, folds, true, rel.span));
    if mixed {
        out.push(l.put_stmt(Stmt::Mixed(e)));
    }
    out
}

/// `let f(a, b) = v [where B]` (R-187): the rule `f(A, B, v') :-
/// demand(A, B), B', reads`, folded over B's aggregates, its parameters
/// bound by the demand its readers feed (`crate::demand`); the demand
/// declared `extern`, so a module's copy names it its own with the let;
/// the relation's mode; then the helpers.
fn let_fn(
    env: Env,
    name: &str,
    params: &[Param],
    value: ExprId,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(env);
    let mut args: Vec<ast::Term> = params.iter().map(|p| param(&mut l, p)).collect();
    let demand = crate::demand::of(name);
    let seed = vec![ast::Lit::Pos(l.put(Atom {
        pred: demand.clone(),
        args: args.clone(),
        record: None,
        span,
    }))];
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded_after(c, seed),
        None => (seed, Vec::new()),
    };
    args.push(l.expr(value));
    body.append(&mut l.reads);
    let arity = args.len();
    let head = l.put(Atom {
        pred: name.to_string(),
        args,
        record: None,
        span,
    });
    let mut out = clause_rule(&l, head, body, folds, true, span);
    let relation = |pred: String, arity| ast::Extern { pred, arity, span };
    out.push(l.put_stmt(Stmt::Extern(relation(demand, arity - 1))));
    out.push(l.put_stmt(Stmt::Mode(relation(name.to_string(), arity))));
    out.append(&mut l.helpers);
    out
}

/// A let's parameter as its relation's column: its variable, or one of
/// a resource type the reference whose address it is, `__ref(T, A, "")`,
/// as a reference column holds it.
fn param(l: &mut Lowering, p: &Param) -> ast::Term {
    let var = l.var(p.var);
    match p.ty.as_ref().map(crate::types::of_expr) {
        Some(crate::types::Ty::Ref(t)) => ast::Term::Func {
            name: crate::address::REF.into(),
            args: vec![str_term(&t), var, str_term("")],
        },
        _ => var,
    }
}

/// A provider's `use p [as n] { .. } [where B]`: when it starts the
/// provider, the provider with its `source`; its configuration
/// `provider_config("n", {k: v'}) [:- B', reads]` (a guarded provider's
/// with no settings too); each `expect_account`'s
/// `provider_expect_account("n", v') [:- B', reads]`; declared under
/// several clauses (R-104), where this one holds and, the first, the
/// denies where two do; then the helpers, in the order its parts made
/// them.
fn provider(env: Env, id: ItemId) -> Vec<Stmt> {
    let program = env.program;
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
    let mut l = Lowering::new(env);
    let mut lits = Vec::new();
    if let Some(c) = clause {
        l.clause(*c, &mut lits);
    }
    let mut body = lits.clone();
    let Settings {
        source,
        config,
        accounts,
    } = Settings::of(&mut l, name, settings, &mut body);
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
        let head = l.put(Atom {
            pred: "provider_config".into(),
            args: vec![str_term(name), ast::Term::Obj(config)],
            record: None,
            span,
        });
        out.extend(clause_rule(&l, head, body, Vec::new(), false, span));
    }
    for (head, b) in accounts {
        out.extend(clause_rule(&l, head, b, Vec::new(), false, span));
    }
    let group = format!("use {name}");
    let finish = |a: Atom| l.put(a);
    if let Some(i) = declared {
        out.push(crate::modules::declared_with(
            &group, *i, lits, span, &finish,
        ));
    }
    if !denies.is_empty() {
        let sites: Vec<(String, Span)> = denies.iter().map(|s| (group.clone(), *s)).collect();
        out.extend(crate::modules::denies_with(&group, &sites, &finish));
    }
    out.append(&mut l.helpers);
    out
}

/// A provider's settings as its `use` block lowers them.
struct Settings {
    /// `source = v`, how the provider is started.
    source: Vec<(String, ast::Term, Span)>,
    /// Every other setting, its configuration.
    config: BTreeMap<String, ast::Term>,
    /// Each `expect_account`'s head and clause.
    accounts: Vec<(Atom, Vec<ast::Lit>)>,
}

impl Settings {
    /// The settings of the provider `name`, the reads a value hoists onto
    /// `body`.
    fn of(
        l: &mut Lowering,
        name: &str,
        settings: &[Setting],
        body: &mut Vec<ast::Lit>,
    ) -> Settings {
        let mut out = Settings {
            source: Vec::new(),
            config: BTreeMap::new(),
            accounts: Vec::new(),
        };
        for s in settings {
            match s {
                Setting::Source { key, span, value } => {
                    out.source.push((key.clone(), l.expr(*value), *span));
                }
                Setting::Value { key, value, .. } => {
                    let v = l.expr(*value);
                    body.append(&mut l.reads);
                    out.config.insert(key.clone(), v);
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
                    let args = vec![str_term(name), l.expr(*value)];
                    let head = l.put(Atom {
                        pred: crate::plugin::providers::EXPECT_ACCOUNT.into(),
                        args,
                        record: None,
                        span: *span,
                    });
                    b.append(&mut l.reads);
                    out.accounts.push((head, b));
                }
            }
        }
        out
    }
}

/// `set ..`: each line's write, a fact or a rule over B's literals (B
/// lowered once, its helpers once) and the line's reads, folded over B's
/// aggregates by the line's own numbers (their helpers at the
/// statement's `span`); then the helpers.
fn set(env: Env, writes: &[Write], clause: Option<ClauseId>, span: Span) -> Vec<Stmt> {
    let mut l = Lowering::new(env);
    let (lits, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    let mut out = Vec::new();
    for w in writes {
        let mut body = lits.clone();
        let at = cell_from(&l, &w.target);
        let cell = cell(&mut l, &w.target);
        body.append(&mut l.reads);
        let value = l.expr(w.value);
        body.append(&mut l.reads);
        let head = write_head(w, cell, value);
        let all = l.env.from;
        let mut froms = vec![all; head.args.len()];
        froms[1] = at;
        let head = l.put_at(head, all, &froms);
        let mut folds = folds.clone();
        for (f, n) in folds.iter_mut().zip(&w.folds) {
            f.2 = Some(*n);
        }
        let mut rules = clause_rule(&l, head, body, folds, clause.is_some(), span);
        rules.iter_mut().for_each(|r| stamp(r, span, w.through));
        out.extend(rules);
    }
    out.append(&mut l.helpers);
    out
}

/// The terms of the cell a line's target is, the reads they hold onto
/// `l`'s: its type, its address, an element's key.
fn cell(l: &mut Lowering, target: &Target) -> Vec<ast::Term> {
    match target {
        Target::Attr { typ, addr, .. } => vec![l.expr(*typ), l.expr(*addr)],
        Target::Element { typ, addr, key, .. } => vec![l.expr(*typ), l.expr(*addr), l.expr(*key)],
        Target::Input { .. } => Vec::new(),
    }
}

/// Where the copies' parts start for the address of the cell a line's
/// target is: its own, when the scope reaches it by its name.
fn cell_from(l: &Lowering, target: &Target) -> Option<super::instances::From> {
    match target {
        Target::Attr { addr, .. } | Target::Element { addr, .. } => l.start_of(*addr),
        Target::Input { .. } => l.env.from,
    }
}

/// A line's head: the cell its target is (its terms `cell`), `value`
/// written to it.
fn write_head(w: &Write, cell: Vec<ast::Term>, value: ast::Term) -> Atom {
    let value = match w.op {
        ast::FieldOp::Replace => crate::transform::whole(value),
        ast::FieldOp::Assign | ast::FieldOp::Add => value,
    };
    let rank = || str_term(w.rank.unwrap_or(Rank::Normal).name());
    let mut cell = cell.into_iter();
    let mut next = || cell.next().expect("a term of the cell");
    let args = match &w.target {
        Target::Attr { path, .. } => {
            let mut args = vec![next(), next(), str_term(path), value];
            args.extend(w.rank.map(|r| str_term(r.name())));
            args
        }
        Target::Element { list, rest, .. } => {
            let (typ, addr, key) = (next(), next(), next());
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
        ast::FieldOp::Assign | ast::FieldOp::Replace => "arg",
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
    env: Env,
    source: &Source,
    (path, value): (VarId, VarId),
    rank: Option<Rank>,
    clause: Option<ClauseId>,
    span: Span,
) -> Vec<Stmt> {
    let mut l = Lowering::new(env);
    let (mut body, folds) = match clause {
        Some(c) => l.unfolded(c),
        None => Default::default(),
    };
    l.goal(source.rows, &mut body);
    let args = vec![
        str_term(crate::modules::INPUT),
        str_term(""),
        l.var(path),
        l.var(value),
        str_term(rank.unwrap_or(Rank::Normal).name()),
    ];
    let head = l.put(Atom {
        pred: "arg".into(),
        args,
        record: None,
        span,
    });
    let mut out: Vec<Stmt> = source
        .externs
        .iter()
        .map(|s| l.put_stmt(s.clone()))
        .collect();
    out.extend(clause_rule(&l, head, body, folds, true, span));
    out.append(&mut l.helpers);
    out
}

/// The input `id` declared, and the helper statements its fields', its
/// refinement's and its clause's bodies made, in that order.
fn input(env: Env, id: ItemId) -> (ast::InputDecl, Vec<Stmt>) {
    input_in(id, Reading::of(env))
}

/// What an input's parts are lowered under: its refinement, its clause
/// and its default.
#[derive(Clone, Copy)]
struct Reading<'p> {
    refinement: Env<'p>,
    guard: Env<'p>,
    default: Env<'p>,
}

impl<'p> Reading<'p> {
    fn of(env: Env<'p>) -> Reading<'p> {
        Reading {
            refinement: env,
            guard: env,
            default: env,
        }
    }
}

/// The input `id` as a copy reads it: its refinement under `full`, its
/// clause under `guard`, its default under `default`.
pub(super) fn input_read(id: ItemId, full: Env, guard: Env, default: Env) -> ast::InputDecl {
    let reading = Reading {
        refinement: full,
        guard,
        default,
    };
    input_in(id, reading).0
}

/// [`input`], each part under its own environment.
fn input_in(id: ItemId, envs: Reading) -> (ast::InputDecl, Vec<Stmt>) {
    let program = envs.refinement.program;
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
            let (decl, made) = input_in(f, envs);
            helpers.extend(made);
            decl
        })
        .collect();
    let default = default.map(|d| Lowering::new(envs.default).expr(d));
    let mut body = |env: Env, c: &Option<ClauseId>| match c {
        Some(c) => {
            let (lits, made) = super::clause::lower_clause_in(env, *c, &[]);
            helpers.extend(made);
            lits
        }
        None => Vec::new(),
    };
    let refinement = body(envs.refinement, refinement);
    let guard = body(envs.guard, guard);
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
    l: &Lowering,
    head: Atom,
    body: Vec<ast::Lit>,
    folds: Vec<super::clause::Fold>,
    has_clause: bool,
    span: Span,
) -> Vec<Stmt> {
    match (folds.is_empty(), has_clause) {
        (false, _) => folded(head, body, folds, span, &|a| l.put(a)),
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
        pred: rel.pred(),
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
        pred: rel.pred(),
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

impl<'p> Let<'p> {
    /// The parts of a `let` item's kind.
    fn of(kind: &'p ItemKind) -> Let<'p> {
        let ItemKind::Let {
            name,
            ty,
            value,
            clause,
            rank,
            declares,
        } = kind
        else {
            unreachable!("a let")
        };
        Let {
            name,
            ty: ty.as_ref(),
            value: *value,
            clause: *clause,
            rank: rank.unwrap_or(Rank::Normal),
            declares: *declares,
        }
    }

    /// `let k = v [@rank] [where B]`: the contribution `let("k", v,
    /// "rank")` to the cell `k`, which `modules` scopes, a rule over B and
    /// the value's reads, its aggregates folded; the cell's secret paths
    /// (R-153); its type `decl k(k: T)`, as R-34 types a relation, at the
    /// first typed row.
    fn statements(&self, env: Env, span: Span) -> Vec<Stmt> {
        let mut l = Lowering::new(env);
        let (mut body, folds) = match self.clause {
            Some(c) => l.unfolded(c),
            None => Default::default(),
        };
        let value = l.expr(self.value);
        body.append(&mut l.reads);
        let head = l.put(Atom {
            pred: crate::modules::LET.to_string(),
            args: vec![str_term(self.name), value, str_term(self.rank.name())],
            record: None,
            span,
        });
        let mut out = clause_rule(&l, head, body, folds, self.clause.is_some(), span);
        for (q, _) in self.ty.iter().flat_map(|d| crate::types::secret_fields(d)) {
            out.push(Stmt::Fact(l.put(Atom {
                pred: crate::modules::SECRET_LET.into(),
                args: vec![str_term(self.name), str_term(&q)],
                record: None,
                span,
            })));
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
