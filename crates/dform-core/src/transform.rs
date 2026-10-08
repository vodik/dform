use crate::ast::{
    Atom, Extern, Lit, Program, Rank, Resource, RuleStmt, Span, Stmt, Term, str_term, var,
};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Lowered {
    pub program: Program,
    /// `decl p/N` declarations.
    pub externs: BTreeSet<Extern>,
    /// The stack's and every module instance's typed inputs.
    pub inputs: Vec<crate::inputs::Declared>,
    /// `extern p(+a, -b)` declarations: what the provider answers on demand.
    pub extern_fns: Vec<crate::ast::ExternFn>,
    /// Outputs declared `secret(T)`: (scope, key), scope `""` for the
    /// stack's own; the key `conn.password` for a field an object type
    /// declares `secret(T)`.
    pub secret_outputs: Vec<(String, String)>,
    /// Every output declared with a type, by (scope, name).
    pub output_types: std::collections::BTreeMap<(String, String), crate::ast::TypeExpr>,
    /// Every relation's columns, declared or inferred (R-34).
    pub signatures: crate::infer::Signatures,
    /// The source program's `decl`s, for the pass with a schema.
    pub declared: crate::infer::Declared,
}

/// `secret_cell(Type, Scope, Key)`: an input or output declared
/// `secret(T)`. Every value of the cell prints as its label
/// (`query::Redactor`).
pub const SECRET_CELL: &str = "secret_cell";

/// An element write's path ends in this (R-35): `arg(T, A, "L[]", [K, V],
/// R)` writes `V` into the element of the keyed list `L` whose key is `K`
/// (the key field's value, or an object holding every key field: an
/// element itself). Its partition is `(arg, T, "P[]")`, `P` the top
/// attribute: it feeds `P`'s aggregate, and its rule reads `P`'s base.
pub const ELEM: &str = "[]";
/// The resolver's spelling of an element write, before `lower` (so that
/// `types::read` reads the content at its schema path): at the list's
/// path, an object with the key under `ELEM_KEY` beside the content, or
/// the content under `ELEM_VALUE` when it is not an object literal.
pub const ELEM_KEY: &str = "[key]";
pub const ELEM_VALUE: &str = "[value]";
/// A keyed list's aggregate without its element writes: what a rule that
/// writes elements of it reads of the top attribute (`attr_base(T, A, P,
/// V)`), so `set c.p = v where c in w.L` is not a cycle through the
/// aggregate it writes. It sees the lists as blocks and whole-list `set`s
/// give them, not another rule's element writes.
pub const ATTR_BASE: &str = "attr_base";

pub fn lower(program: &Program) -> Result<Lowered> {
    // `type` blocks: their refinements (`crate::refine`).
    let program = &crate::refine::lower_types(program)?;
    reject_pending(&program.statements)?;
    let declared = crate::infer::Declared::of(program);
    let program = apply_decls(program)?;
    // In the future, imports should be handled in a loader before parsing.
    // For now, keep Import statements in the AST but drop them before eval.
    let crate::modules::Expanded {
        program: expanded,
        mut inputs,
        secret_outputs,
        output_types,
    } = crate::modules::expand(&program)?;
    check_mixed(&expanded)?;
    let (expanded, externs, extern_fns) = drop_metadata(&expanded);
    let expanded = desugar_resources(&expanded)?;
    let expanded = desugar_comprehensions(&expanded)?;
    let mut expanded = declassified(lower_contributions(&expanded)?);
    // `set from DOC` contributes per input path (R-38).
    expanded = crate::tables::expand_set_from(expanded, &mut inputs);
    crate::externs::check(&expanded, &extern_fns)?;
    // The cells of secret inputs and outputs, for the Redactor.
    let secret_inputs = inputs
        .iter()
        .filter(|d| matches!(&d.decl.ty, crate::ast::TypeExpr::Apply(n, _) if n == "secret"))
        .map(|d| {
            (
                crate::modules::INPUT,
                d.scope.as_str(),
                d.decl.name.as_str(),
            )
        });
    let secret_outs = secret_outputs
        .iter()
        .map(|(scope, k)| (OUTPUT, scope.as_str(), k.as_str()));
    for (typ, scope, key) in secret_inputs.chain(secret_outs) {
        expanded.statements.push(Stmt::Fact(atom(
            SECRET_CELL,
            vec![str_term(typ), str_term(scope), str_term(key)],
        )));
    }
    // Column types (R-34): checked, and the literals read as them.
    let inferred = crate::infer::infer(&expanded, &extern_fns, &inputs, &declared, None)?;
    inferred.read(&mut expanded);
    let mut extern_fns = extern_fns;
    inferred.type_tables(&mut extern_fns);
    // A file of given secrets' values are secrets from the first byte
    // (R-108): never recorded in the clear.
    crate::tables::seal_columns(&mut extern_fns);
    Ok(Lowered {
        program: expanded,
        externs,
        inputs,
        extern_fns,
        secret_outputs,
        output_types,
        signatures: inferred.signatures,
        declared,
    })
}

/// A lowering error at a statement.
fn spanned(span: Span, msg: impl Into<String>) -> anyhow::Error {
    Diagnostics(vec![Diagnostic::error(span, msg)]).into()
}

/// E §6 statements with no lowering yet are errors naming their ticket,
/// in a module's body too. A declaration stands anywhere: a provider's
/// `use` and a loader's own declaration in a module are the deployment's
/// (R-129).
fn reject_pending(stmts: &[Stmt]) -> Result<()> {
    fn walk(stmts: &[Stmt], diags: &mut Vec<Diagnostic>) {
        for s in stmts {
            match s {
                Stmt::Pending(p) => {
                    let (what, ticket) = p.kind.describe();
                    diags.push(
                        Diagnostic::error(p.span, format!("{what} is not yet supported"))
                            .with_note(format!("it parses; its semantics land with {ticket}")),
                    );
                }
                Stmt::Module(d) => walk(&d.body, diags),
                _ => {}
            }
        }
    }
    let mut diags = Vec::new();
    walk(stmts, &mut diags);
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// Rank of a contribution in the core form `arg(T, A, P, V, Rank)`.
pub const NORMAL: &str = "normal";

/// A pseudo-type of the attribute aggregate (E §2.5): outputs are
/// addressed by component scope ("" for the root program).
pub const OUTPUT: &str = "output";

/// Pseudo-types of the attribute aggregate that are not resources:
/// outputs, inputs and lets (`modules::INPUT`, `modules::LET`).
pub fn is_pseudo_type(typ: &str) -> bool {
    matches!(typ, OUTPUT | crate::modules::INPUT | crate::modules::LET)
}

/// Contribution heads in the source forms, as `(type, addr, path, value)`.
fn contribution_parts(a: &Atom) -> Option<(Term, Term, Term, Term)> {
    let g = |i: usize| a.args[i].clone();
    match (a.pred.as_str(), a.args.len()) {
        ("arg", 4) | ("arg_add", 4) => Some((g(0), g(1), g(2), g(3))),
        ("output", 3) => Some((str_term(OUTPUT), g(0), g(1), g(2))),
        ("output", 2) => Some((str_term(OUTPUT), str_term(""), g(0), g(1))),
        _ => None,
    }
}

/// E §2.5 path normalization at compile time, when the path is a constant: a
/// resource attribute path `a.b.c` contributes `{b: {c: V}}` to `a` (the fake
/// provider's attributes are all top-level keys). Outputs keep their full
/// key: each is its own declared leaf.
pub fn normalize_contribution(typ: &str, path: &str, value: Term) -> (String, Term) {
    if typ == OUTPUT {
        return (path.to_string(), value);
    }
    let segs = crate::ir::path_segments(path);
    let first = segs[0].to_string();
    let value = segs[1..].iter().rev().fold(value, |v, k| {
        Term::Obj(BTreeMap::from([(
            crate::ir::segment_key(k).into_owned(),
            v,
        )]))
    });
    (first, value)
}

/// The core form of a contribution head: `arg(T, A, P, V, Rank)` with a
/// constant path normalized.
fn contribution_head(a: Atom) -> Result<Atom> {
    if a.pred == "merge_rule" {
        return Err(spanned(
            a.span,
            "merge_rule is gone: a path's lattice is declared with type_lattice(Type, Path, flat|map|set)",
        ));
    }
    let (typ, addr, path, value, rank) = match contribution_parts(&a) {
        Some((t, n, p, v)) => (t, n, p, v, str_term(NORMAL)),
        None if a.pred == "arg" && a.args.len() == 5 => {
            let g = |i: usize| a.args[i].clone();
            (g(0), g(1), g(2), g(3), g(4))
        }
        None => return Ok(a),
    };
    let (path, value) = match (&typ, &path, element_write(&value)) {
        (_, Term::Val(Value::Str(p)), Some((k, v))) => {
            (str_term(&format!("{p}{ELEM}")), Term::List(vec![k, v]))
        }
        (Term::Val(Value::Str(t)), Term::Val(Value::Str(p)), None) => {
            let (p, v) = normalize_contribution(t, p, value);
            (str_term(&p), v)
        }
        _ => (path, value),
    };
    Ok(Atom {
        pred: "arg".into(),
        args: vec![typ, addr, path, value, rank],
        record: None,
        span: a.span,
    })
}

/// The resolver's element write (`ELEM_KEY`): its key and content.
fn element_write(value: &Term) -> Option<(Term, Term)> {
    let m: BTreeMap<String, Term> = match value {
        Term::Obj(m) => m.clone(),
        Term::Val(Value::Obj(m)) => m
            .iter()
            .map(|(k, v)| (k.clone(), Term::Val(v.clone())))
            .collect(),
        _ => return None,
    };
    let mut m = m;
    let key = m.remove(ELEM_KEY)?;
    let content = m.remove(ELEM_VALUE).unwrap_or(Term::Obj(m));
    Some((key, content))
}

/// The `(T, P)` an element write's rule writes: its type and top attribute.
fn element_target(r: &RuleStmt) -> Option<(Term, String)> {
    match (r.head.pred.as_str(), r.head.args.as_slice()) {
        ("arg", [t, _, Term::Val(Value::Str(p)), _, _]) if p.ends_with(ELEM) => Some((
            t.clone(),
            p.split(['.', '[']).next().unwrap_or(p).to_string(),
        )),
        _ => None,
    }
}

/// A rule that writes elements of a keyed list reads its type's top
/// attribute there as the base, `ATTR_BASE`: the aggregate without element
/// writes. Its own read of the list is then below its write. So do the
/// helpers the compiler wrote for its body (`__neg_0` for a `not`), which
/// belong to it.
fn base_reads(statements: Vec<Stmt>) -> Vec<Stmt> {
    let mut targets: BTreeMap<String, (Term, String)> = BTreeMap::new();
    let helpers = |r: &RuleStmt| -> Vec<String> {
        r.body
            .iter()
            .filter_map(|l| match l {
                Lit::Pos(a) | Lit::Not(a) if a.pred.starts_with("__") => Some(a.pred.clone()),
                _ => None,
            })
            .collect()
    };
    let mut work: Vec<(String, (Term, String))> = Vec::new();
    for st in &statements {
        if let Stmt::Rule(r) = st
            && let Some(t) = element_target(r)
        {
            work.extend(helpers(r).into_iter().map(|h| (h, t.clone())));
        }
    }
    while let Some((h, t)) = work.pop() {
        if targets.insert(h.clone(), t.clone()).is_some() {
            continue;
        }
        for st in &statements {
            if let Stmt::Rule(r) = st
                && r.head.pred == h
            {
                work.extend(helpers(r).into_iter().map(|h| (h, t.clone())));
            }
        }
    }
    statements
        .into_iter()
        .map(|st| match st {
            Stmt::Rule(r) => {
                let t = element_target(&r).or_else(|| targets.get(&r.head.pred).cloned());
                match t {
                    Some((typ, top)) => Stmt::Rule(base_body(r, &typ, &top)),
                    None => Stmt::Rule(r),
                }
            }
            other => other,
        })
        .collect()
}

fn base_body(r: RuleStmt, typ: &Term, top: &str) -> RuleStmt {
    let base = |a: Atom| match a.args.as_slice() {
        [t, _, Term::Val(Value::Str(p)), _] if a.pred == "attr" && t == typ && p == top => Atom {
            pred: ATTR_BASE.into(),
            ..a
        },
        _ => a,
    };
    RuleStmt {
        body: r
            .body
            .into_iter()
            .map(|l| match l {
                Lit::Pos(a) => Lit::Pos(base(a)),
                Lit::Not(a) => Lit::Not(base(a)),
                other => other,
            })
            .collect(),
        ..r
    }
}

/// A body read of a contribution predicate is a read of the aggregate:
/// `attr(T, A, P, V)`, the collapsed value (E §2.5).
fn attr_read(a: Atom) -> Result<Atom> {
    if a.pred == "arg" && a.args.len() == 5 {
        return Err(spanned(
            a.span,
            "arg/5 after `where` reads raw contributions; read the collapsed attr(T, A, P, V) instead",
        ));
    }
    let Some((typ, addr, path, value)) = contribution_parts(&a) else {
        return Ok(a);
    };
    if let (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) = (&typ, &path)
        && t != OUTPUT
        && p.contains('.')
    {
        return Err(spanned(
            a.span,
            format!(
                "{}(..., {p:?}, ...) after `where`: read the top-level attribute and destructure it",
                a.pred
            ),
        ));
    }
    Ok(Atom {
        pred: "attr".into(),
        args: vec![typ, addr, path, value],
        record: None,
        span: a.span,
    })
}

fn attr_lits(body: Vec<Lit>) -> Result<Vec<Lit>> {
    body.into_iter()
        .map(|l| {
            Ok(match l {
                Lit::Pos(a) => Lit::Pos(attr_read(a)?),
                Lit::Not(a) => Lit::Not(attr_read(a)?),
                other => other,
            })
        })
        .collect()
}

/// Last lowering pass: every contribution head (`arg`, `arg_add`,
/// `output`) becomes the core form `arg/5`, and every body
/// read of one becomes a read of `attr/4`.
fn lower_contributions(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        out.push(match stmt.clone() {
            Stmt::Fact(a) => Stmt::Fact(contribution_head(a)?),
            Stmt::Rule(r) => Stmt::Rule(RuleStmt {
                head: contribution_head(r.head)?,
                body: attr_lits(r.body)?,
            }),
            other => other,
        });
    }
    Ok(Program {
        stack: program.stack.clone(),
        statements: base_reads(out),
    })
}

/// E §2.6: a predicate is extensional (ground facts) or intensional
/// (rules), not both, unless declared `decl p(..) mixed`. Checked after
/// modules and `when` are expanded (a guarded fact is a rule), over the
/// program's own predicates: the compiler's (`want`, `arg`, ...) are
/// written both ways by design.
fn check_mixed(program: &Program) -> Result<()> {
    let mut mixed = BTreeSet::new();
    let mut facts: BTreeMap<(&str, usize), Span> = BTreeMap::new();
    let mut rules: BTreeMap<(&str, usize), Span> = BTreeMap::new();
    for st in &program.statements {
        match st {
            Stmt::Mixed(e) => {
                mixed.insert((e.pred.as_str(), e.arity));
            }
            Stmt::Fact(a) => {
                facts.entry((&a.pred, a.args.len())).or_insert(a.span);
            }
            Stmt::Rule(r) => {
                rules
                    .entry((&r.head.pred, r.head.args.len()))
                    .or_insert(r.head.span);
            }
            _ => {}
        }
    }
    let diags: Vec<Diagnostic> = rules
        .iter()
        .filter(|(k, _)| !mixed.contains(*k))
        .filter(|((p, _), _)| !crate::loader::is_core_pred(p) && !p.contains("__"))
        .filter_map(|(k @ (p, n), rule)| {
            let fact = facts.get(k)?;
            let decl = format!("decl {p}({}) mixed", columns(*n));
            let d = Diagnostic::error(
                *rule,
                format!(
                    "{p}/{n} has both ground facts and rules: it is extensional and intensional"
                ),
            )
            .with_label(*fact, format!("a ground fact of {p}/{n}"))
            .with_help(format!(
                "derive the facts with rules too, or declare it: `{decl}`"
            ));
            // The declaration goes before the first statement of `p/N`,
            // when both are the program's own (not a module's or a pack's,
            // whose names are renamed).
            if fact.origin != 0 || rule.origin != 0 || fact.file != rule.file {
                return Some(d);
            }
            let first = if fact.start < rule.start { fact } else { rule };
            let at = Span {
                end: first.start,
                ..*first
            };
            Some(d.with_fix(
                format!("declare it: `{decl}`"),
                vec![(at, format!("{decl}\n"))],
            ))
        })
        .collect();
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// E DR-19: a rule that uses `secret.declassify(V, Reason)` also derives
/// `declassified(Site, Reason)` from its body, `Site` where the rule is
/// written (`file:line:col`), so a policy can read and deny it.
fn declassified(mut program: Program) -> Program {
    fn reasons(t: &Term, out: &mut Vec<Term>) {
        match t {
            Term::Func { name, args } => {
                if name == crate::secrets::DECLASSIFY && args.len() == 2 {
                    out.push(args[1].clone());
                }
                args.iter().for_each(|a| reasons(a, out));
            }
            Term::List(xs) => xs.iter().for_each(|a| reasons(a, out)),
            Term::Obj(m) => m.values().for_each(|a| reasons(a, out)),
            _ => {}
        }
    }
    let mut more = Vec::new();
    for st in &program.statements {
        let Stmt::Rule(r) = st else { continue };
        let mut found = Vec::new();
        r.head.args.iter().for_each(|t| reasons(t, &mut found));
        for l in &r.body {
            match l {
                Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| reasons(t, &mut found)),
                Lit::Eq(x, y)
                | Lit::Neq(x, y)
                | Lit::Gt(x, y)
                | Lit::Ge(x, y)
                | Lit::Lt(x, y)
                | Lit::Le(x, y) => [x, y].into_iter().for_each(|t| reasons(t, &mut found)),
            }
        }
        let site = diag::at(r.head.span).unwrap_or_else(|| "compiler".into());
        for reason in found {
            let mut head = atom("declassified", vec![str_term(&site), reason]);
            head.span = r.head.span;
            more.push(Stmt::Rule(RuleStmt {
                head,
                body: r.body.clone(),
            }));
        }
    }
    program.statements.extend(more);
    program
}

fn apply_decls(program: &Program) -> Result<Program> {
    // Built-in schemas for record atoms.
    let mut schemas: BTreeMap<String, Vec<String>> = BTreeMap::new();
    schemas.insert("want".to_string(), vec!["type".into(), "name".into()]);
    schemas.insert(
        "arg".to_string(),
        vec!["type".into(), "name".into(), "path".into(), "value".into()],
    );
    schemas.insert(
        "arg_add".to_string(),
        vec!["type".into(), "name".into(), "path".into(), "value".into()],
    );
    schemas.insert(
        "output".to_string(),
        vec!["scope".into(), "key".into(), "value".into()],
    );
    schemas.insert("input".to_string(), vec!["key".into(), "value".into()]);
    schemas.insert("data".to_string(), vec!["key".into(), "value".into()]);
    schemas.insert(
        "merge_rule".to_string(),
        vec!["type".into(), "path".into(), "op".into()],
    );
    schemas.insert("warn".to_string(), vec!["msg".into(), "ctx".into()]);
    schemas.insert("deny".to_string(), vec!["msg".into(), "ctx".into()]);

    for s in &program.statements {
        if let Stmt::Decl(d) = s {
            schemas.insert(d.pred.clone(), d.fields.clone());
        }
    }

    let mut out = Vec::new();
    for s in &program.statements {
        if matches!(s, Stmt::Decl(_)) {
            continue;
        }
        out.push(rewrite_stmt_records(s.clone(), &schemas)?);
    }
    Ok(Program {
        statements: out,
        stack: program.stack.clone(),
    })
}

#[derive(Copy, Clone)]
enum Ctx {
    Fact,
    Head,
    Body,
}

fn rewrite_stmt_records(stmt: Stmt, schemas: &BTreeMap<String, Vec<String>>) -> Result<Stmt> {
    Ok(match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom_records(a, schemas, Ctx::Fact)?),
        Stmt::Rule(r) => {
            let head = rewrite_atom_records(r.head, schemas, Ctx::Head)?;
            let body = rewrite_lits_records(r.body, schemas)?;
            Stmt::Rule(RuleStmt { head, body })
        }
        // A module's or component's own `decl`s name its relations'
        // fields inside it, over any outer one of the same name.
        Stmt::Module(mut c) => {
            let mut inner = schemas.clone();
            for s in &c.body {
                if let Stmt::Decl(d) = s {
                    inner.insert(d.pred.clone(), d.fields.clone());
                }
            }
            c.body = c
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, &inner))
                .collect::<Result<Vec<_>>>()?;
            Stmt::Module(c)
        }
        Stmt::Instance(mut u) => {
            if let Some(b) = u.body {
                u.body = Some(rewrite_lits_records(b, schemas)?);
            }
            if let Some(b) = u.clause {
                u.clause = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Instance(u)
        }
        Stmt::Use(mut u) => {
            if let Some(b) = u.body {
                u.body = Some(rewrite_lits_records(b, schemas)?);
            }
            if let Some(b) = u.clause {
                u.clause = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Use(u)
        }
        Stmt::Resource(mut r) => {
            if let Some(b) = r.body {
                r.body = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Resource(r)
        }
        other => other,
    })
}

fn rewrite_lits_records(
    lits: Vec<Lit>,
    schemas: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<Lit>> {
    lits.into_iter()
        .map(|l| rewrite_lit_records(l, schemas))
        .collect()
}

fn rewrite_lit_records(lit: Lit, schemas: &BTreeMap<String, Vec<String>>) -> Result<Lit> {
    Ok(match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom_records(a, schemas, Ctx::Body)?),
        Lit::Not(a) => Lit::Not(rewrite_atom_records(a, schemas, Ctx::Body)?),
        other => other,
    })
}

fn rewrite_atom_records(
    mut atom: Atom,
    schemas: &BTreeMap<String, Vec<String>>,
    ctx: Ctx,
) -> Result<Atom> {
    let Some(fields) = atom.record.take() else {
        return Ok(atom);
    };
    let Some(order) = schemas.get(&atom.pred) else {
        return Err(spanned(
            atom.span,
            format!(
                "no record fields declared for predicate '{}' (declare them with decl {}(Field: type, ...))",
                atom.pred, atom.pred
            ),
        ));
    };

    // No extra fields.
    for k in fields.keys() {
        if !order.iter().any(|x| x == k) {
            return Err(spanned(
                atom.span,
                format!("unknown field '{k}' for predicate '{}'", atom.pred),
            ));
        }
    }

    let require_complete = matches!(ctx, Ctx::Fact | Ctx::Head);
    let mut args = Vec::with_capacity(order.len());
    for f in order {
        match fields.get(f) {
            Some(t) => args.push(t.clone()),
            None => {
                if require_complete {
                    return Err(spanned(
                        atom.span,
                        format!("missing field '{f}' for predicate '{}'", atom.pred),
                    ));
                }
                args.push(Term::Wildcard);
            }
        }
    }
    if require_complete && args.iter().any(|t| matches!(t, Term::Wildcard)) {
        return Err(spanned(
            atom.span,
            format!(
                "`_` is no value: not in a fact, nor left of `where`, of '{}'",
                atom.pred
            ),
        ));
    }

    atom.args = args;
    atom.record = None;
    Ok(atom)
}

/// A head with no body and no variables is a fact; anything else a rule.
fn fact_or_rule(head: Atom, body: &[Lit]) -> Stmt {
    if body.is_empty() && head.args.iter().all(is_ground_term) {
        Stmt::Fact(head)
    } else {
        Stmt::Rule(RuleStmt {
            head,
            body: body.to_vec(),
        })
    }
}

fn desugar_comprehensions(program: &Program) -> Result<Program> {
    // Keep rewriting until no list comprehensions remain. This makes nested
    // comprehensions work without complex single-pass analysis.
    let mut current = program.clone();
    let mut counter = 0usize;
    loop {
        let mut changed = false;
        let mut next_stmts: Vec<Stmt> = Vec::new();
        let mut helpers: Vec<Stmt> = Vec::new();

        for stmt in &current.statements {
            match stmt {
                Stmt::Rule(r) => {
                    if has_listcomp_rule(r) {
                        changed = true;
                    }
                    let (h, rr) = rewrite_rule_listcomps(r.clone(), &mut counter)?;
                    helpers.extend(h);
                    next_stmts.push(Stmt::Rule(rr));
                }
                other => next_stmts.push(other.clone()),
            }
        }

        // Helper rules must be part of the same program.
        next_stmts.extend(helpers);
        current = Program {
            statements: next_stmts,
            stack: current.stack.clone(),
        };

        if !changed {
            break;
        }
        if counter > 50_000 {
            bail!("comprehension lowering did not converge");
        }
    }
    Ok(current)
}

fn rewrite_rule_listcomps(mut r: RuleStmt, counter: &mut usize) -> Result<(Vec<Stmt>, RuleStmt)> {
    let counts_all = {
        let mut m = count_vars_in_term(&Term::Func {
            name: "__head".to_string(),
            args: r.head.args.clone(),
        });
        merge_counts(&mut m, &count_vars_in_lits(&r.body));
        m
    };

    let mut helpers = Vec::new();

    // Rewrite body first (so binding order stays intuitive).
    r.body = rewrite_lits_listcomps(&r.body, &counts_all, counter, &mut helpers)?;

    // Rewrite head terms; any joins needed for head terms are appended to the rule body.
    let mut head_prefix = Vec::new();
    let mut new_args = Vec::new();
    for a in r.head.args {
        let (t2, prefix) = rewrite_term_listcomps(a, &counts_all, counter, &mut helpers)?;
        head_prefix.extend(prefix);
        new_args.push(t2);
    }
    r.head.args = new_args;
    r.body.extend(head_prefix);
    locate(&mut helpers, r.head.span);

    Ok((helpers, r))
}

/// A comprehension's helper rules are where the comprehension is.
fn locate(helpers: &mut [Stmt], span: Span) {
    for h in helpers {
        if let Stmt::Rule(r) = h {
            r.head.span = span;
        }
    }
}

fn rewrite_lits_listcomps(
    lits: &[Lit],
    counts_all: &BTreeMap<String, usize>,
    counter: &mut usize,
    helpers: &mut Vec<Stmt>,
) -> Result<Vec<Lit>> {
    let mut out = Vec::new();
    for lit in lits {
        let parts = rewrite_lit_listcomps(lit.clone(), counts_all, counter, helpers)?;
        out.extend(parts);
    }
    Ok(out)
}

fn rewrite_lit_listcomps(
    lit: Lit,
    counts_all: &BTreeMap<String, usize>,
    counter: &mut usize,
    helpers: &mut Vec<Stmt>,
) -> Result<Vec<Lit>> {
    match lit {
        Lit::Pos(mut a) => {
            let mut prefix = Vec::new();
            let mut args2 = Vec::new();
            for t in a.args {
                let (t2, p) = rewrite_term_listcomps(t, counts_all, counter, helpers)?;
                prefix.extend(p);
                args2.push(t2);
            }
            a.args = args2;
            prefix.push(Lit::Pos(a));
            Ok(prefix)
        }
        Lit::Not(mut a) => {
            let mut prefix = Vec::new();
            let mut args2 = Vec::new();
            for t in a.args {
                let (t2, p) = rewrite_term_listcomps(t, counts_all, counter, helpers)?;
                prefix.extend(p);
                args2.push(t2);
            }
            a.args = args2;
            prefix.push(Lit::Not(a));
            Ok(prefix)
        }
        Lit::Eq(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Eq(a2, b2));
            Ok(pa)
        }
        Lit::Neq(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Neq(a2, b2));
            Ok(pa)
        }
        Lit::Gt(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Gt(a2, b2));
            Ok(pa)
        }
        Lit::Ge(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Ge(a2, b2));
            Ok(pa)
        }
        Lit::Lt(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Lt(a2, b2));
            Ok(pa)
        }
        Lit::Le(a, b) => {
            let (a2, mut pa) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
            let (b2, pb) = rewrite_term_listcomps(b, counts_all, counter, helpers)?;
            pa.extend(pb);
            pa.push(Lit::Le(a2, b2));
            Ok(pa)
        }
    }
}

fn rewrite_term_listcomps(
    term: Term,
    counts_all: &BTreeMap<String, usize>,
    counter: &mut usize,
    helpers: &mut Vec<Stmt>,
) -> Result<(Term, Vec<Lit>)> {
    Ok(match term {
        Term::ListComp { item, body } => {
            *counter += 1;
            let lc_pred = format!("__lc_{counter}");
            let list_var = Term::Var(format!("__lc_val_{counter}"));

            let comp_counts = count_vars_in_listcomp(&item, &body);
            let body_vars = count_vars_in_lits(&body);

            let mut key_vars = Vec::new();
            for (v, c) in &comp_counts {
                let total = counts_all.get(v).copied().unwrap_or(0);
                if total > *c {
                    // Var appears outside this comprehension.
                    if !body_vars.contains_key(v) {
                        bail!(
                            "`{v}` in a comprehension is given its values inside it, after its `|`"
                        );
                    }
                    key_vars.push(v.clone());
                }
            }
            key_vars.sort();
            key_vars.dedup();

            let helper_head_args: Vec<Term> = key_vars
                .iter()
                .cloned()
                .map(Term::Var)
                .chain(std::iter::once(Term::Func {
                    name: "collect_list".to_string(),
                    args: vec![*item],
                }))
                .collect();

            helpers.push(Stmt::Rule(RuleStmt {
                head: Atom {
                    pred: lc_pred.clone(),
                    args: helper_head_args,
                    record: None,
                    span: Default::default(),
                },
                body,
            }));

            let mut join_args: Vec<Term> = key_vars.iter().cloned().map(Term::Var).collect();
            join_args.push(list_var.clone());

            (
                list_var,
                vec![Lit::Pos(Atom {
                    pred: lc_pred,
                    args: join_args,
                    record: None,
                    span: Default::default(),
                })],
            )
        }
        Term::Func { name, args } => {
            let mut prefix = Vec::new();
            let mut args2 = Vec::new();
            for a in args {
                let (a2, p) = rewrite_term_listcomps(a, counts_all, counter, helpers)?;
                prefix.extend(p);
                args2.push(a2);
            }
            (Term::Func { name, args: args2 }, prefix)
        }
        Term::List(xs) => {
            let mut prefix = Vec::new();
            let mut xs2 = Vec::new();
            for x in xs {
                let (x2, p) = rewrite_term_listcomps(x, counts_all, counter, helpers)?;
                prefix.extend(p);
                xs2.push(x2);
            }
            (Term::List(xs2), prefix)
        }
        Term::Obj(m) => {
            let mut prefix = Vec::new();
            let mut m2 = BTreeMap::new();
            for (k, v) in m {
                let (v2, p) = rewrite_term_listcomps(v, counts_all, counter, helpers)?;
                prefix.extend(p);
                m2.insert(k, v2);
            }
            (Term::Obj(m2), prefix)
        }
        other => (other, Vec::new()),
    })
}

fn has_listcomp_rule(r: &RuleStmt) -> bool {
    r.head.args.iter().any(has_listcomp_term) || has_listcomp_lits(&r.body)
}

fn has_listcomp_lits(lits: &[Lit]) -> bool {
    lits.iter().any(|l| match l {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().any(has_listcomp_term),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => has_listcomp_term(a) || has_listcomp_term(b),
    })
}

fn has_listcomp_term(t: &Term) -> bool {
    match t {
        Term::ListComp { .. } => true,
        Term::Func { args, .. } => args.iter().any(has_listcomp_term),
        Term::List(xs) => xs.iter().any(has_listcomp_term),
        Term::Obj(m) => m.values().any(has_listcomp_term),
        _ => false,
    }
}

fn count_vars_in_listcomp(item: &Term, body: &[Lit]) -> BTreeMap<String, usize> {
    let mut out = count_vars_in_term(item);
    merge_counts(&mut out, &count_vars_in_lits(body));
    out
}

fn count_vars_in_lits(lits: &[Lit]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for l in lits {
        merge_counts(&mut out, &count_vars_in_lit(l));
    }
    out
}

fn count_vars_in_lit(l: &Lit) -> BTreeMap<String, usize> {
    match l {
        Lit::Pos(a) | Lit::Not(a) => {
            let mut out = BTreeMap::new();
            for t in &a.args {
                merge_counts(&mut out, &count_vars_in_term(t));
            }
            out
        }
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            let mut out = count_vars_in_term(a);
            merge_counts(&mut out, &count_vars_in_term(b));
            out
        }
    }
}

fn count_vars_in_term(t: &Term) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    count_vars_in_term_into(t, &mut out);
    out
}

fn count_vars_in_term_into(t: &Term, out: &mut BTreeMap<String, usize>) {
    match t {
        Term::Var(v) => {
            *out.entry(v.clone()).or_insert(0) += 1;
        }
        Term::Wildcard => {}
        Term::Func { args, .. } => {
            for a in args {
                count_vars_in_term_into(a, out);
            }
        }
        Term::List(xs) => {
            for x in xs {
                count_vars_in_term_into(x, out);
            }
        }
        Term::Obj(m) => {
            for v in m.values() {
                count_vars_in_term_into(v, out);
            }
        }
        Term::ListComp { item, body } => {
            count_vars_in_term_into(item, out);
            merge_counts(out, &count_vars_in_lits(body));
        }
        Term::Val(_) => {}
    }
}

fn merge_counts(dst: &mut BTreeMap<String, usize>, src: &BTreeMap<String, usize>) {
    for (k, v) in src {
        *dst.entry(k.clone()).or_insert(0) += v;
    }
}

/// Drop statements that carry no rules, keeping the `extern` declarations.
/// `unique` lowers to nothing: one value per key is what the attribute
/// aggregate already enforces.
fn drop_metadata(program: &Program) -> (Program, BTreeSet<Extern>, Vec<crate::ast::ExternFn>) {
    let mut statements = Vec::new();
    let mut externs = BTreeSet::new();
    let mut fns = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::Extern(e) => {
                externs.insert(e.clone());
            }
            // A loader's declaration stands at each call (R-129): one is
            // the table's.
            Stmt::ExternFn(f) if fns.iter().any(|g: &crate::ast::ExternFn| g.name == f.name) => {}
            Stmt::ExternFn(f) => {
                externs.insert(Extern {
                    pred: f.name.clone(),
                    arity: f.args.len(),
                    span: f.span,
                });
                fns.push(f.clone());
            }
            Stmt::Mixed(_) => {
                // checked by check_mixed
            }
            Stmt::Instance(_) | Stmt::Module(_) | Stmt::Use(_) => {
                // lowered away earlier
            }
            Stmt::Decl(_) => {
                // lowered away by apply_decls
            }
            Stmt::Output(_) | Stmt::Provider(_) => {
                // declarations: the interface and the stack, not rules
            }
            _ => statements.push(s.clone()),
        }
    }
    (
        Program {
            statements,
            stack: program.stack.clone(),
        },
        externs,
        fns,
    )
}

fn desugar_resources(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    let mut n = 0;
    // After the program's own statements, so its rules keep their indices
    // (their ids in `why`).
    let mut reports = Vec::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Resource(r) => {
                reports.extend(unread_field_reports(r, &mut n));
                reports.extend(not_planned_report(r));
                out.extend(resource_to_stmts(r.clone())?);
            }
            _ => out.push(stmt.clone()),
        }
    }
    out.extend(reports);
    Ok(Program {
        statements: out,
        stack: program.stack.clone(),
    })
}

/// `resource T N [@rank] { [for body] k = v [@rank] ... }` is `want(T, N)`
/// plus one contribution `arg(T, N, k, v, Rank)` per field, each with the
/// whole body. `+=` is a plain contribution: the lattice decides the merge.
fn resource_to_stmts(r: Resource) -> Result<Vec<Stmt>> {
    let body = r.body.unwrap_or_default();
    let mut out = vec![fact_or_rule(
        Atom {
            pred: "want".to_string(),
            args: vec![r.typ.clone(), r.name.clone()],
            record: None,
            span: r.span,
        },
        &body,
    )];
    for f in r.fields {
        let rank = f.rank.or(r.rank).unwrap_or(Rank::Normal);
        // `resource T N = VALUE` with a value known only when the rule runs
        // (R-126): a contribution per key of the object it is, `arg(T, N,
        // P, V, Rank) :- body, X = __body(VALUE), member(X, K, V), P =
        // __segment(K)` (`__body` is the value when it is an object).
        let (path, value, body) = match f.key.is_empty() {
            true => {
                let used: BTreeSet<String> = count_vars_in_term(&Term::List(vec![
                    r.typ.clone(),
                    r.name.clone(),
                    f.value.clone(),
                ]))
                .into_keys()
                .chain(body.iter().flat_map(|l| count_vars_in_lit(l).into_keys()))
                .collect();
                let fresh = |base: &str| {
                    (0..)
                        .map(|i| format!("{base}{i}"))
                        .find(|v| !used.contains(v))
                        .unwrap_or_default()
                };
                let (x, k, v, p) = (
                    fresh("Body"),
                    fresh("BodyKey"),
                    fresh("BodyValue"),
                    fresh("BodyPath"),
                );
                let mut body = body.clone();
                body.push(Lit::Eq(
                    Term::Var(x.clone()),
                    Term::Func {
                        name: crate::ir::RESOURCE_BODY.into(),
                        args: vec![f.value],
                    },
                ));
                body.push(Lit::Pos(atom(
                    "member",
                    vec![Term::Var(x), Term::Var(k.clone()), Term::Var(v.clone())],
                )));
                body.push(Lit::Eq(
                    Term::Var(p.clone()),
                    Term::Func {
                        name: crate::ir::NAME_SEGMENT.into(),
                        args: vec![Term::Var(k)],
                    },
                ));
                (Term::Var(p), Term::Var(v), body)
            }
            false => (str_term(&f.key), f.value, body.clone()),
        };
        let head = Atom {
            pred: "arg".to_string(),
            args: vec![
                r.typ.clone(),
                r.name.clone(),
                path,
                value,
                str_term(rank.name()),
            ],
            record: None,
            span: f.span,
        };
        out.push(fact_or_rule(head, &body));
    }
    Ok(out)
}

/// The message of [`unread_field_reports`].
pub const UNREAD_FIELD: &str = "a field read found no value: the resource is not derived";

/// A field's reads are literals of the block's one body, so a read with no
/// row (a misspelled path, an attribute the provider never returns) holds
/// the whole resource back. Per read of an object's attribute, the report
/// that it did, when the object is there but the attribute is not:
///
/// ```text
/// __field_read_N(Xs) :- Rest, attr(T, A, P, V).
/// warn(UNREAD_FIELD, {resource, read, at}) :-
///     Rest, want(T, A), not __field_read_N(Xs).
/// ```
///
/// (`cloud_attr(T, A, _, _)` for a live object's.) `Rest` is the body
/// without this read, the field reads after it, and what only they bind:
/// the block's clauses and guards, the reads before it (so the first read
/// that fails is the one named). `Xs` are the read's variables `Rest`
/// binds.
///
/// A missing object, an instance input or output with no value, an input
/// no `set` gives in this deployment: those hold a block back on purpose
/// (a module's resource exists where its inputs are given), and are quiet.
fn unread_field_reports(r: &Resource, n: &mut usize) -> Vec<Stmt> {
    let Some(body) = &r.body else {
        return Vec::new();
    };
    let reads: Vec<usize> = r
        .reads
        .clone()
        .filter(|&i| matches!(body.get(i), Some(Lit::Pos(_))))
        .collect();
    let mut out = Vec::new();
    for (k, &i) in reads.iter().enumerate() {
        let Lit::Pos(read) = &body[i] else {
            continue;
        };
        let exists = match (read.pred.as_str(), read.args.as_slice()) {
            ("attr", [t, a, _, _]) => atom("want", vec![t.clone(), a.clone()]),
            ("cloud_attr", [t, a, _, _]) => atom(
                "cloud_attr",
                vec![t.clone(), a.clone(), Term::Wildcard, Term::Wildcard],
            ),
            _ => continue,
        };
        let mut dropped: BTreeSet<usize> = reads[k..].iter().copied().collect();
        let bound = loop {
            let rest: Vec<&Lit> = (0..body.len())
                .filter(|j| !dropped.contains(j))
                .map(|j| &body[j])
                .collect();
            let bound = bound_by(&rest);
            let unbound: Vec<usize> = (0..body.len())
                .filter(|j| !dropped.contains(j))
                .filter(|&j| {
                    count_vars_in_lit(&body[j])
                        .keys()
                        .any(|v| !bound.contains(v))
                })
                .collect();
            if unbound.is_empty() {
                break bound;
            }
            dropped.extend(unbound);
        };
        let rest: Vec<Lit> = (0..body.len())
            .filter(|j| !dropped.contains(j))
            .map(|j| body[j].clone())
            .collect();
        let xs: Vec<Term> = count_vars_in_term(&Term::List(read.args.clone()))
            .into_keys()
            .filter(|v| bound.contains(v))
            .map(|v| var(&v))
            .collect();
        let helper = atom(&format!("__field_read_{n}"), xs);
        *n += 1;
        let names_bound = count_vars_in_term(&Term::List(vec![r.typ.clone(), r.name.clone()]))
            .keys()
            .all(|v| bound.contains(v));
        let resource = if names_bound {
            Term::Func {
                name: crate::ir::FORMAT.into(),
                args: vec![str_term("%s.%s"), r.typ.clone(), r.name.clone()],
            }
        } else {
            str_term(&format!(
                "{}.{}",
                crate::partition::fmt_term(&r.typ).trim_matches('"'),
                crate::partition::fmt_term(&r.name).trim_matches('"')
            ))
        };
        let mut ctx = BTreeMap::from([
            ("resource".to_string(), resource),
            (
                "read".to_string(),
                str_term(&crate::partition::fmt_atom(read)),
            ),
        ]);
        if let Some(at) = diag::place(read.span) {
            ctx.insert("at".to_string(), str_term(&at));
        }
        let mut with_read = rest.clone();
        with_read.push(Lit::Pos(read.clone()));
        out.push(Stmt::Rule(RuleStmt {
            head: helper.clone(),
            body: with_read,
        }));
        let mut body = rest;
        body.push(Lit::Pos(exists));
        body.push(Lit::Not(helper));
        out.push(Stmt::Rule(RuleStmt {
            head: Atom {
                span: read.span,
                ..atom("warn", vec![str_term(UNREAD_FIELD), Term::Obj(ctx)])
            },
            body,
        }));
    }
    out
}

/// `__not_planned(T, A)`: a resource statement whose own clause holds
/// derives no `want` row (R-120), so a read in its block found nothing:
/// an input of its copy with no value, a `let` with no row, another
/// resource's attribute no rule sets. The plan lists it with why
/// (`zset::not_planned`) instead of leaving it out silently.
///
/// ```text
/// __not_planned(T, A) :- Clause, not want(T, A).
/// ```
///
/// `Clause` is the block's body without its field reads: its `where`,
/// its guards, a copy's gate and the name's holes. A clause that does
/// not hold holds the block back on purpose, quietly. A statement whose
/// clause needs a read to bind it, or whose name only a read binds, has
/// no report: what it would name is not known.
fn not_planned_report(r: &Resource) -> Option<Stmt> {
    let body = r.body.clone().unwrap_or_default();
    let clause: Vec<Lit> = body
        .iter()
        .enumerate()
        .filter(|(i, _)| !r.reads.contains(i))
        .map(|(_, l)| l.clone())
        .collect();
    let bound = bound_by(&clause.iter().collect::<Vec<_>>());
    let free = |vars: BTreeMap<String, usize>| vars.keys().any(|v| !bound.contains(v));
    if clause.iter().any(|l| free(count_vars_in_lit(l)))
        || free(count_vars_in_term(&Term::List(vec![
            r.typ.clone(),
            r.name.clone(),
        ])))
    {
        return None;
    }
    let mut body = clause;
    body.push(Lit::Not(atom("want", vec![r.typ.clone(), r.name.clone()])));
    Some(Stmt::Rule(RuleStmt {
        head: Atom {
            span: r.span,
            ..atom(NOT_PLANNED, vec![r.typ.clone(), r.name.clone()])
        },
        body,
    }))
}

/// The relation of [`not_planned_report`].
pub const NOT_PLANNED: &str = "__not_planned";

/// The variables `lits` bind: every variable of a positive literal outside
/// a function's arguments, and through `=`, a side whose variables are
/// all bound binds the other's.
fn bound_by(lits: &[&Lit]) -> BTreeSet<String> {
    fn pattern(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(v) => {
                out.insert(v.clone());
            }
            Term::List(xs) => xs.iter().for_each(|x| pattern(x, out)),
            Term::Obj(m) => m.values().for_each(|x| pattern(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for l in lits {
        if let Lit::Pos(a) = l {
            a.args.iter().for_each(|t| pattern(t, &mut out));
        }
    }
    loop {
        let before = out.len();
        for l in lits {
            if let Lit::Eq(a, b) = l {
                for (x, y) in [(a, b), (b, a)] {
                    if count_vars_in_term(y).keys().all(|v| out.contains(v)) {
                        pattern(x, &mut out);
                    }
                }
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

fn is_ground_term(t: &Term) -> bool {
    match t {
        Term::Var(_) => false,
        Term::Wildcard => false,
        Term::Val(_) => true,
        Term::Func { args, .. } => args.iter().all(is_ground_term),
        Term::List(xs) => xs.iter().all(is_ground_term),
        Term::Obj(m) => m.values().all(is_ground_term),
        Term::ListComp { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Computed attributes (E §2.5, F14): the prelude that mints one null per
// (want(T, A), computed P), and `ref` to a computed path as an attr read.
// These run after lowering, against the provider schema.
// ---------------------------------------------------------------------------

fn atom(pred: &str, args: Vec<Term>) -> Atom {
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    }
}

/// The schema paths the prelude mints a null for: every `computed` and every
/// `optional_computed` path outside a list element, with its class and the
/// rank the null contributes at (`@default` for Optional+Computed, so a
/// program's value wins). Minted paths do not nest (R-116): a path with
/// another below it (a claim's `status.capacity`, a map whose usual keys
/// the schema types, `status.capacity.storage`) is those leaves, so its
/// value is not one null beside its own leaves', which disagree with it.
pub fn minted_paths(schema: &Schema) -> Vec<(String, String, NullClass, &'static str)> {
    let mut out = Vec::new();
    for ((t, p), c) in &schema.computed {
        if !schema.in_list(t, p) {
            out.push((t.clone(), p.clone(), *c, NORMAL));
        }
    }
    for ((t, p), c) in &schema.optional_computed {
        if !schema.in_list(t, p) {
            out.push((t.clone(), p.clone(), *c, "default"));
        }
    }
    let below = |t: &str, p: &str| {
        out.iter().any(|(u, q, _, _)| {
            u == t
                && q.strip_prefix(p)
                    .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
        })
    };
    let nested: Vec<bool> = out.iter().map(|(t, p, _, _)| below(t, p)).collect();
    let mut nested = nested.into_iter();
    out.retain(|_| !nested.next().unwrap_or(false));
    out
}

/// The compiler-generated prelude, expanded per schema row (E §2.5, §4.3):
///
/// ```text
/// resolve(__label(T, A, P), V) :- identity(T, A, Rid), world_attr(T, Rid, P, V).
/// resolved(L) :- resolve(L, _).
/// % per computed (T, P), at rank normal (computed) or default (optional_computed):
/// arg(T, A, P, V, Rank)    :- want(T, A), resolve(__label(T, A, P), V).       % round 0 (not secret)
/// arg(T, A, P, ?T/A#P, Rank) :- want(T, A), not resolved(__label(T, A, P)).
/// ```
///
/// A secret is never resolved in the store (E Rule 4): its null stays.
pub fn computed_prelude(schema: &Schema) -> Vec<RuleStmt> {
    let rows = minted_paths(schema);
    if rows.is_empty() {
        return vec![];
    }
    let label = |t: &Term, p: &Term| Term::Func {
        name: "__label".into(),
        args: vec![t.clone(), var("__A"), p.clone()],
    };
    let mut out = vec![
        RuleStmt {
            head: atom(
                "resolve",
                vec![
                    Term::Func {
                        name: "__label".into(),
                        args: vec![var("__T"), var("__A"), var("__P")],
                    },
                    var("__V"),
                ],
            ),
            body: vec![
                Lit::Pos(atom("identity", vec![var("__T"), var("__A"), var("__Rid")])),
                Lit::Pos(atom(
                    "world_attr",
                    vec![var("__T"), var("__Rid"), var("__P"), var("__V")],
                )),
            ],
        },
        RuleStmt {
            head: atom("resolved", vec![var("__L")]),
            body: vec![Lit::Pos(atom("resolve", vec![var("__L"), Term::Wildcard]))],
        },
    ];
    for (t, p, class, rank) in rows {
        let (tt, pt) = (str_term(&t), str_term(&p));
        let want = Lit::Pos(atom("want", vec![tt.clone(), var("__A")]));
        let contribution = |value: Term| {
            let (path, value) = normalize_contribution(&t, &p, value);
            atom(
                "arg",
                vec![
                    tt.clone(),
                    var("__A"),
                    str_term(&path),
                    value,
                    str_term(rank),
                ],
            )
        };
        if class != NullClass::Secret {
            out.push(RuleStmt {
                head: contribution(var("__V")),
                body: vec![
                    want.clone(),
                    Lit::Pos(atom("resolve", vec![label(&tt, &pt), var("__V")])),
                ],
            });
        }
        let ty = schema
            .attr(&t, &p)
            .map(|a| a.ty.clone())
            .unwrap_or_default();
        let null = Term::Func {
            name: "__null".into(),
            args: vec![
                tt.clone(),
                var("__A"),
                pt.clone(),
                str_term(class.name()),
                str_term(&ty),
            ],
        };
        let mut body = vec![want];
        if class != NullClass::Secret {
            body.push(Lit::Not(atom("resolved", vec![label(&tt, &pt)])));
        }
        out.push(RuleStmt {
            head: contribution(null),
            body,
        });
    }
    out
}

/// A contribution to a plain `computed` path is a compile error: the provider
/// owns it (E §2.5). Optional+Computed paths may be set.
pub fn check_computed_writes(rules: &[RuleStmt], facts: &[Atom], schema: &Schema) -> Result<()> {
    let heads = rules
        .iter()
        .map(|r| (&r.head, crate::partition::fmt_rule(r)))
        .chain(facts.iter().map(|a| (a, crate::partition::fmt_atom(a))));
    let mut errors = Vec::new();
    for (h, text) in heads {
        if h.pred != "arg" || h.args.len() != 5 {
            continue;
        }
        let (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) = (&h.args[0], &h.args[2]) else {
            continue;
        };
        let mut paths = vec![p.clone()];
        leaf_paths(&h.args[3], p, &mut paths);
        for path in paths {
            if schema.class_of(t, &path).is_some() {
                let addr = match &h.args[1] {
                    Term::Val(Value::Str(a)) => crate::ir::Address {
                        typ: t.to_string(),
                        name: a.clone(),
                    }
                    .to_string(),
                    Term::Val(v) => format!("{t}[{}]", crate::partition::fmt_value(v)),
                    other => format!("{t}[{}]", crate::partition::fmt_term(other)),
                };
                errors.push(
                    Diagnostic::error(
                        h.span,
                        format!(
                            "resource {addr}: attribute {path} is computed by the provider and cannot be set"
                        ),
                    )
                    .with_note(format!("in: {text}")),
                );
            }
        }
    }
    if !errors.is_empty() {
        return Err(Diagnostics(errors).into());
    }
    Ok(())
}

/// Every path under `prefix` a literal object term sets, inner nodes included.
fn leaf_paths(t: &Term, prefix: &str, out: &mut Vec<String>) {
    let entries: Vec<(&String, Option<&Term>)> = match t {
        Term::Obj(m) => m.iter().map(|(k, v)| (k, Some(v))).collect(),
        Term::Val(Value::Obj(m)) => m.keys().map(|k| (k, None)).collect(),
        _ => return,
    };
    for (k, v) in entries {
        let p = crate::ir::path_join(prefix, k);
        if let Some(v) = v {
            leaf_paths(v, &p, out);
        } else if let Term::Val(Value::Obj(m)) = t {
            leaf_paths(&Term::Val(m[k].clone()), &p, out);
        }
        out.push(p);
    }
}

/// A content read of a computed path inside a block (DESIGN.org R-4): the
/// block's shared body reads `attr(T, A, P, V)` where the schema computes
/// `P` (or the path the body walks from it), so the whole block waits for
/// the tick that creates `A`, where a reference (`f = a.p`, a whole value)
/// would be an edge and apply with it. One note per block, at its first
/// such read (`file:line:col` first), in the order of the program. A read
/// the block's name needs (an interpolated header) is the point of the
/// wait, and has none.
pub fn computed_reads(statements: &[Stmt], schema: &Schema) -> Vec<(Span, String)> {
    // A block's rules (its `want` and each field's `arg`) share one body:
    // the block is known by its reads' places.
    let mut blocks = BTreeSet::new();
    let mut out = Vec::new();
    for s in statements {
        let Stmt::Rule(r) = s else { continue };
        let block = matches!(
            (r.head.pred.as_str(), r.head.args.len()),
            ("want", 2) | ("arg", 5)
        ) && matches!(&r.head.args[0], Term::Val(Value::Str(t)) if !is_pseudo_type(t));
        if !block {
            continue;
        }
        let named = name_vars(&r.head.args[1], &r.body);
        let reads: Vec<&Atom> = r
            .body
            .iter()
            .filter_map(|l| match l {
                Lit::Pos(a) if a.pred == "attr" && a.args.len() == 4 && !a.span.is_none() => {
                    Some(a)
                }
                _ => None,
            })
            .collect();
        let key: Vec<(u32, u32, u32)> = reads
            .iter()
            .map(|a| (a.span.file, a.span.start, a.span.end))
            .collect();
        if !blocks.insert(key) {
            continue;
        }
        let first = reads.iter().find_map(|a| {
            let [Term::Val(Value::Str(t)), addr, Term::Val(Value::Str(p)), v] = a.args.as_slice()
            else {
                return None;
            };
            if matches!(v, Term::Var(x) if named.contains(x)) {
                return None;
            }
            let mut paths = vec![p.clone()];
            walks(v, &r.body, p, &mut paths);
            let path = paths
                .into_iter()
                .find(|q| schema.class_of(t, q).is_some())?;
            let at = match addr {
                Term::Val(Value::Str(a)) => crate::ir::scope_split(a)
                    .map_or(a.as_str(), |(_, n)| n)
                    .to_string(),
                other => format!("{t}[{}]", crate::partition::fmt_term(other)),
            };
            Some((a.span, at, path))
        });
        if let Some((span, at, path)) = first {
            let place = diag::place(span)
                .map(|p| format!("{p}: "))
                .unwrap_or_default();
            out.push((
                span,
                format!(
                    "{place}reads `{at}.{path}` now, a computed value: this block waits for \
                     the tick that creates `{at}`; a field written `= {at}.{path}` would be an \
                     edge and apply with it"
                ),
            ));
        }
    }
    out
}

/// The variables a block's address is built from: its header's holes
/// (`Addr = format(..)`), followed back through every literal that joins
/// them (`member(Zones, _, Z)`), but an `attr` read, whose value its key
/// does not decide.
fn name_vars(addr: &Term, body: &[Lit]) -> BTreeSet<String> {
    let mut vars: BTreeSet<String> = count_vars_in_term(addr).into_keys().collect();
    loop {
        let before = vars.len();
        for l in body {
            if matches!(l, Lit::Pos(a) if a.pred == "attr") {
                continue;
            }
            let ls = count_vars_in_lit(l);
            if ls.keys().any(|v| vars.contains(v)) {
                vars.extend(ls.into_keys());
            }
        }
        if vars.len() == before {
            return vars;
        }
    }
}

/// Each path `p.q` the body walks from the read's value `v`
/// (`__path(v, "q")`, anywhere in a term).
fn walks(v: &Term, body: &[Lit], p: &str, out: &mut Vec<String>) {
    fn term(t: &Term, v: &Term, p: &str, out: &mut Vec<String>) {
        match t {
            Term::Func { name, args } => {
                if name == "__path"
                    && args.first() == Some(v)
                    && let Some(Term::Val(Value::Str(q))) = args.get(1)
                {
                    out.push(format!("{p}.{q}"));
                }
                args.iter().for_each(|a| term(a, v, p, out));
            }
            Term::List(xs) => xs.iter().for_each(|a| term(a, v, p, out)),
            Term::Obj(m) => m.values().for_each(|a| term(a, v, p, out)),
            _ => {}
        }
    }
    for l in body {
        match l {
            Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| term(t, v, p, out)),
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => {
                term(x, v, p, out);
                term(y, v, p, out);
            }
        }
    }
}

/// `__ref_dep(T, A, T2, A2)`: a contribution to `(T, A)` holds a ref to
/// `(T2, A2)`, so `(T2, A2)` is applied first.
pub const REF_DEP: &str = "__ref_dep";

/// `ref(T, A, P)` with a constant `T` and a computed or Optional+Computed
/// `P` reads the attribute aggregate (E DR-2): the term becomes a variable
/// bound by `attr(T, A, P0, V)` (`P0` the path's top-level attribute, the
/// rest walked with `__path`), so it is the minted null, the round-0 value,
/// or the program's value. A type with no provider is a data source with no
/// `want`: its ref is the null itself. Facts that hold such a ref become
/// rules.
pub fn rewrite_computed_refs(
    rules: Vec<RuleStmt>,
    facts: Vec<Atom>,
    schema: &Schema,
) -> (Vec<RuleStmt>, Vec<Atom>) {
    let mut n = 0usize;
    let mut out_rules = Vec::new();
    let mut out_facts = Vec::new();
    // After every rule, so a rule's index (its id in `why`) does not move.
    let mut dangling = Vec::new();
    let rules = facts
        .into_iter()
        .map(|f| RuleStmt {
            head: f,
            body: vec![],
        })
        .chain(rules);
    for r in rules {
        let mut head_reads = Vec::new();
        let head = rewrite_atom_refs(&r.head, schema, &mut n, &mut head_reads);
        // A contribution that holds a reference (`vpc = main`, R-43) joins
        // the resource's identity as a read of it would, so it holds once
        // the resource is wanted and is pending while that may derive; the
        // value stays the reference (`ir::compile_resources` gives the
        // provider the id). To an address no rule wants it is the same deny.
        let mut wants = Vec::new();
        if head.pred == "arg" && head.args.len() == 5 {
            let mut to = Vec::new();
            references(&head.args[3], &mut to);
            for (typ, addr) in to {
                wants.push(identity_read(typ, addr, schema));
            }
        }
        if r.body.is_empty() && head_reads.is_empty() && wants.is_empty() {
            out_facts.push(head);
            continue;
        }
        let body_only = rewrite_body_refs(r.body, schema, &mut n);
        for read in &wants {
            dangling.push(dangling_ref_deny(&head, read, "", body_only.clone()));
        }
        // A ref to an address no rule wants would make the attr join empty
        // and the field vanish: it is a deny instead.
        for (read, path) in &head_reads {
            dangling.push(dangling_ref_deny(&head, read, path, body_only.clone()));
        }
        let mut body = body_only;
        body.extend(head_reads.iter().map(|(read, _)| Lit::Pos(read.clone())));
        body.extend(wants.into_iter().map(Lit::Pos));
        // The value is read now, but the order of Apply still follows the
        // ref: a contribution that reads another resource's attribute
        // depends on it (`ir::compile_resources` reads `__ref_dep`).
        if head.pred == "arg" && head.args.len() == 5 {
            for (read, _) in &head_reads {
                out_rules.push(RuleStmt {
                    head: atom(
                        REF_DEP,
                        vec![
                            head.args[0].clone(),
                            head.args[1].clone(),
                            read.args[0].clone(),
                            read.args[1].clone(),
                        ],
                    ),
                    body: body.clone(),
                });
            }
        }
        out_rules.push(RuleStmt { head, body });
    }
    out_rules.extend(dangling);
    (out_rules, out_facts)
}

/// The read a reference to the resource `(typ, addr)` joins: its identity
/// `attr(T, A, id, _)`, a cell minted for every wanted resource, whose
/// aggregate is stuck while the resource may still derive. A type need not
/// declare an `id` (`k8s.namespace`, whose reference a copy's input takes,
/// R-120): then the top of its first computed attribute, minted the same
/// way (`metadata`); a type with none, its `want`.
pub(crate) fn identity_read(typ: Term, addr: Term, schema: &Schema) -> Atom {
    let cell = match &typ {
        Term::Val(Value::Str(t)) if schema.attr(t, crate::schema::IDENTITY).is_none() => schema
            .computed_of(t)
            .first()
            .map(|(p, _)| p.split('.').next().unwrap_or(p).to_string()),
        _ => Some(crate::schema::IDENTITY.to_string()),
    };
    match cell {
        Some(p) => atom("attr", vec![typ, addr, str_term(&p), Term::Wildcard]),
        None => atom("want", vec![typ, addr]),
    }
}

/// `deny("ref to an address no rule wants", {type, addr, path, from, at})
/// :- Body, not want(T, A).` for one ref `read` (`attr(T, A, P0, V)`) in
/// the head of a rule with `body`. `from` names what holds the ref: the
/// resource `T.A` for a contribution, else the head's predicate; `at` is
/// where that is written.
fn dangling_ref_deny(head: &Atom, read: &Atom, path: &str, mut body: Vec<Lit>) -> RuleStmt {
    let (typ, addr) = (read.args[0].clone(), read.args[1].clone());
    let from = if head.pred == "arg" && head.args.len() == 5 {
        Term::Func {
            name: crate::ir::FORMAT.into(),
            args: vec![
                str_term("%s.%s"),
                head.args[0].clone(),
                head.args[1].clone(),
            ],
        }
    } else {
        str_term(&head.pred)
    };
    // As early as the address is bound: the rest of the body may be stuck
    // on a null (member over a computed list) for an address that is
    // wanted, and that must not make this deny undetermined.
    let need: BTreeSet<String> = count_vars_in_term(&typ)
        .into_keys()
        .chain(count_vars_in_term(&addr).into_keys())
        .collect();
    let mut bound = BTreeSet::new();
    let mut at = body.len();
    for (i, l) in body.iter().enumerate() {
        if need.is_subset(&bound) {
            at = i;
            break;
        }
        match l {
            Lit::Pos(_) => bound.extend(count_vars_in_lit(l).into_keys()),
            Lit::Eq(a, b) => {
                for t in [a, b] {
                    if let Term::Var(v) = t {
                        bound.insert(v.clone());
                    }
                }
            }
            _ => {}
        }
    }
    body.insert(at, Lit::Not(atom("want", vec![typ.clone(), addr.clone()])));
    let mut ctx = BTreeMap::from([
        ("type".to_string(), typ),
        ("addr".to_string(), addr),
        ("path".to_string(), str_term(path)),
        ("from".to_string(), from),
    ]);
    if head.pred == "arg" && head.args.len() == 5 {
        ctx.insert("from_type".to_string(), head.args[0].clone());
        ctx.insert("from_name".to_string(), head.args[1].clone());
    }
    if let Some(at) = diag::place(head.span) {
        ctx.insert("at".to_string(), str_term(&at));
    }
    RuleStmt {
        head: Atom {
            span: head.span,
            ..atom("deny", vec![str_term(DANGLING_REF), Term::Obj(ctx)])
        },
        body,
    }
}

/// Each reference `ref(T, A, "")` of a constant type in `t`: its type and
/// address.
fn references(t: &Term, out: &mut Vec<(Term, Term)>) {
    match t {
        Term::Func { name, args } if name == crate::ir::REF && args.len() == 3 => {
            if let (Term::Val(Value::Str(_)), Term::Val(Value::Str(p))) = (&args[0], &args[2])
                && p.is_empty()
            {
                out.push((args[0].clone(), args[1].clone()));
            }
        }
        Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|x| references(x, out)),
        Term::Obj(m) => m.values().for_each(|x| references(x, out)),
        _ => {}
    }
}

/// The deny message for a ref to an address no rule wants.
pub const DANGLING_REF: &str = "ref to an address no rule wants";

fn rewrite_body_refs(body: Vec<Lit>, schema: &Schema, n: &mut usize) -> Vec<Lit> {
    let mut out = Vec::new();
    for l in body {
        let mut reads = Vec::new();
        let l = l
            .map(|a| Atom { record: None, ..a }, |t| t)
            .map_terms(|t| rewrite_term_refs(&t, schema, n, &mut reads));
        out.extend(reads.into_iter().map(|(read, _)| Lit::Pos(read)));
        out.push(l);
    }
    out
}

fn rewrite_atom_refs(
    a: &Atom,
    schema: &Schema,
    n: &mut usize,
    reads: &mut Vec<(Atom, String)>,
) -> Atom {
    Atom {
        pred: a.pred.clone(),
        args: a
            .args
            .iter()
            .map(|t| rewrite_term_refs(t, schema, n, reads))
            .collect(),
        record: a.record.clone(),
        span: a.span,
    }
}

fn rewrite_term_refs(
    t: &Term,
    schema: &Schema,
    n: &mut usize,
    reads: &mut Vec<(Atom, String)>,
) -> Term {
    match t {
        Term::Func { name, args } if name == crate::ir::REF && args.len() == 3 => {
            if let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) = (&args[0], &args[2])
            {
                let class = schema
                    .class_of(typ, path)
                    .or_else(|| schema.optional_computed_class(typ, path));
                if let Some(class) = class {
                    let addr = rewrite_term_refs(&args[1], schema, n, reads);
                    if !schema.provider_of.contains_key(typ) {
                        let ty = schema
                            .attr(typ, path)
                            .map(|a| a.ty.clone())
                            .unwrap_or_default();
                        return Term::Func {
                            name: "__null".into(),
                            args: vec![
                                args[0].clone(),
                                addr,
                                args[2].clone(),
                                str_term(class.name()),
                                str_term(&ty),
                            ],
                        };
                    }
                    *n += 1;
                    let v = var(&format!("__ref{n}"));
                    let (top, rest) = match crate::ir::path_split_first(path) {
                        Some((top, rest)) => (top, Some(rest)),
                        None => (path.as_str(), None),
                    };
                    reads.push((
                        atom(
                            "attr",
                            vec![args[0].clone(), addr, str_term(top), v.clone()],
                        ),
                        path.clone(),
                    ));
                    return match rest {
                        None => v,
                        Some(rest) => Term::Func {
                            name: "__path".into(),
                            args: vec![v, str_term(rest)],
                        },
                    };
                }
            }
            Term::Func {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|x| rewrite_term_refs(x, schema, n, reads))
                    .collect(),
            }
        }
        Term::Func { name, args } => Term::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|x| rewrite_term_refs(x, schema, n, reads))
                .collect(),
        },
        Term::List(xs) => Term::List(
            xs.iter()
                .map(|x| rewrite_term_refs(x, schema, n, reads))
                .collect(),
        ),
        Term::Obj(m) => Term::Obj(
            m.iter()
                .map(|(k, x)| (k.clone(), rewrite_term_refs(x, schema, n, reads)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Column names for a declaration the compiler writes: `a, b, c`.
pub fn columns(n: usize) -> String {
    (0..n)
        .map(|i| {
            let c = (b'a' + (i % 26) as u8) as char;
            if i < 26 {
                c.to_string()
            } else {
                format!("{c}{}", i / 26)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
