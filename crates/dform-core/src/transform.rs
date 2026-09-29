use crate::ast::{
    Atom, Constraint, Extern, Lit, Program, Rank, Resource, RuleStmt, Span, Stmt, Term, When,
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
    /// stack's own.
    pub secret_outputs: Vec<(String, String)>,
}

/// `unknowns = strict` (DESIGN.org "Strict mode is first-class"): a plan
/// that needs a phase boundary is denied. A stuck instance (E §2.7 Rules 2
/// and 3; a stuck resource rule is a pending group) and a resource rule
/// that may derive after a boundary (F DR-2 revised; a pending group too,
/// given to the plan's policy pass) each derive the deny, with the rule,
/// its head pattern and the nulls as provenance. `allow_stuck(HeadPattern).`
/// facts relax it per key.
pub const STRICT_RULES: &str = r#"
deny "strict: unresolved value at plan time" { rule: r, head: h, nulls: ns } if
  stuck(r, h, _, ns), not allow_stuck(h)
deny "strict: a pending group at plan time" { rule: r, head: h, nulls: ns } if
  may_derive(r, h, ns), not allow_stuck(h)
"#;

/// `secret_cell(Type, Scope, Key)`: an input or output declared
/// `secret(T)`. Every value of the cell prints as its label
/// (`query::Redactor`).
pub const SECRET_CELL: &str = "secret_cell";

pub fn lower(program: &Program) -> Result<Lowered> {
    // `type` blocks: their refinements (`crate::refine`).
    let program = &crate::refine::lower_types(program)?;
    reject_pending(&program.statements)?;
    let strict =
        crate::stack::config(program).is_ok_and(|s| s.unknowns == crate::stack::Unknowns::Strict);
    let mut program = apply_decls(program)?;
    if strict {
        program
            .statements
            .extend(crate::parser::parse_program(STRICT_RULES)?.statements);
    }
    // In the future, imports should be handled in a loader before parsing.
    // For now, keep Import statements in the AST but drop them before eval.
    let crate::modules::Expanded {
        program: expanded,
        inputs,
        secret_outputs,
    } = crate::modules::expand(&program)?;
    let expanded = expand_when(&expanded)?;
    check_mixed(&expanded)?;
    let expanded = desugar_settings(&expanded)?;
    let (expanded, externs, extern_fns) = drop_metadata(&expanded);
    let expanded = desugar_resources(&expanded, strict)?;
    let expanded = desugar_comprehensions(&expanded)?;
    let mut expanded = declassified(lower_contributions(&expanded)?);
    // A stack's config contributes per settings path the program knows.
    expanded = crate::tables::expand_config(expanded);
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
    Ok(Lowered {
        program: expanded,
        externs,
        inputs,
        extern_fns,
        secret_outputs,
    })
}

/// A lowering error at a statement.
fn spanned(span: Span, msg: impl Into<String>) -> anyhow::Error {
    Diagnostics(vec![Diagnostic::error(span, msg)]).into()
}

/// E §6 statements with no lowering yet are errors naming their ticket,
/// and an interface statement (`input`, `output`, `export`, `contributes`)
/// where it has no meaning is an error naming where it belongs.
fn reject_pending(stmts: &[Stmt]) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum At {
        Top,
        Module,
        Nested,
    }
    fn walk(stmts: &[Stmt], at: At, diags: &mut Vec<Diagnostic>) {
        for s in stmts {
            let misplaced = |span, what: &str| Diagnostic::error(span, what.to_string());
            match s {
                Stmt::Pending(p) => {
                    let (what, ticket) = p.kind.describe();
                    diags.push(
                        Diagnostic::error(p.span, format!("{what} is not yet supported"))
                            .with_note(format!("it parses; its semantics land with {ticket}")),
                    );
                }
                Stmt::Module(d) => walk(&d.body, At::Module, diags),
                Stmt::PolicyPack(p) => walk(&p.body, At::Module, diags),
                Stmt::When(w) => walk(&w.body, At::Nested, diags),
                Stmt::Input(i) if at == At::Nested => diags.push(misplaced(
                    i.span,
                    "an input is declared at the top of a module",
                )),
                Stmt::Output(o) if at == At::Nested => diags.push(misplaced(
                    o.span,
                    "an output is declared at the top of a module or the program",
                )),
                Stmt::Export(e) if at != At::Module => {
                    diags.push(misplaced(e.span, "`export` belongs at the top of a module"))
                }
                Stmt::Scenario(sc) if at != At::Top => diags.push(misplaced(
                    sc.span,
                    "a scenario belongs at the top of the program",
                )),
                Stmt::Scenario(sc) => walk(&sc.body, At::Nested, diags),
                Stmt::ExternFn(e) if at != At::Top => diags.push(misplaced(
                    e.span,
                    "`extern` belongs at the top of the program",
                )),
                Stmt::Stack(c) | Stmt::Provider(c) if at != At::Top => diags.push(misplaced(
                    c.span,
                    "`stack` and `provider` belong at the top of the program",
                )),
                Stmt::Contributes(c) if at != At::Module => diags.push(misplaced(
                    c.span,
                    "`contributes` belongs at the top of a module or policy pack",
                )),
                _ => {}
            }
        }
    }
    let mut diags = Vec::new();
    walk(stmts, At::Top, &mut diags);
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// Rank of a contribution in the core form `arg(T, A, P, V, Rank)`.
pub const NORMAL: &str = "normal";

/// Pseudo-types of the attribute aggregate (E §2.5): settings are addressed
/// by environment, outputs by component scope ("" for the root program).
pub const SETTINGS: &str = "settings";
pub const OUTPUT: &str = "output";

/// Pseudo-types of the attribute aggregate that are not resources:
/// settings, outputs and inputs (`modules::INPUT`).
pub fn is_pseudo_type(typ: &str) -> bool {
    matches!(typ, SETTINGS | OUTPUT | crate::modules::INPUT)
}

fn str_term(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

/// Contribution heads in the source forms, as `(type, addr, path, value)`.
fn contribution_parts(a: &Atom) -> Option<(Term, Term, Term, Term)> {
    let g = |i: usize| a.args[i].clone();
    match (a.pred.as_str(), a.args.len()) {
        ("arg", 4) | ("arg_add", 4) => Some((g(0), g(1), g(2), g(3))),
        ("setting", 3) | ("setting_add", 3) => Some((str_term(SETTINGS), g(0), g(1), g(2))),
        ("output", 3) => Some((str_term(OUTPUT), g(0), g(1), g(2))),
        ("output", 2) => Some((str_term(OUTPUT), str_term(""), g(0), g(1))),
        _ => None,
    }
}

/// E §2.5 path normalization at compile time, when the path is a constant: a
/// resource attribute path `a.b.c` contributes `{b: {c: V}}` to `a` (the fake
/// provider's attributes are all top-level keys). Settings and outputs keep
/// their full key: each is its own declared leaf.
pub fn normalize_contribution(typ: &str, path: &str, value: Term) -> (String, Term) {
    if typ == SETTINGS || typ == OUTPUT {
        return (path.to_string(), value);
    }
    let mut segs = path.split('.');
    let first = segs.next().unwrap_or(path).to_string();
    let rest: Vec<&str> = segs.collect();
    let value = rest.iter().rev().fold(value, |v, k| {
        Term::Obj(BTreeMap::from([(k.to_string(), v)]))
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
    let (path, value) = match (&typ, &path) {
        (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) => {
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

/// A body read of a contribution predicate is a read of the aggregate:
/// `attr(T, A, P, V)`, the collapsed value (E §2.5).
fn attr_read(a: Atom) -> Result<Atom> {
    if a.pred == "arg" && a.args.len() == 5 {
        return Err(spanned(
            a.span,
            "arg/5 in a rule body reads raw contributions; read the collapsed attr(T, A, P, V) instead",
        ));
    }
    let Some((typ, addr, path, value)) = contribution_parts(&a) else {
        return Ok(a);
    };
    if let (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) = (&typ, &path)
        && t != SETTINGS
        && t != OUTPUT
        && p.contains('.')
    {
        return Err(spanned(
            a.span,
            format!(
                "{}(..., {p:?}, ...) in a rule body: read the top-level attribute and destructure it",
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

/// Last lowering pass: every contribution head (`arg`, `arg_add`, `setting`,
/// `setting_add`, `output`) becomes the core form `arg/5`, and every body
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
            Stmt::Constraint(c) => Stmt::Constraint(Constraint {
                body: attr_lits(c.body)?,
                ..c
            }),
            other => other,
        });
    }
    Ok(Program { statements: out })
}

/// E §2.6: a predicate is extensional (ground facts) or intensional
/// (rules), not both, unless declared `decl p/N mixed`. Checked after
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
            let d = Diagnostic::error(
                *rule,
                format!(
                    "{p}/{n} has both ground facts and rules: it is extensional and intensional"
                ),
            )
            .with_label(*fact, format!("a ground fact of {p}/{n}"))
            .with_help(format!(
                "derive the facts with rules too, or declare it: `decl {p}/{n} mixed`"
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
                format!("declare it: `decl {p}/{n} mixed`"),
                vec![(at, format!("decl {p}/{n} mixed\n"))],
            ))
        })
        .collect();
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// E DR-19: a rule that uses `declassify(V, Reason)` also derives
/// `declassified(Site, Reason)` from its body, `Site` where the rule is
/// written (`file:line:col`), so a policy can read and deny it.
fn declassified(mut program: Program) -> Program {
    fn reasons(t: &Term, out: &mut Vec<Term>) {
        match t {
            Term::Func { name, args } => {
                if name == "declassify" && args.len() == 2 {
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
        "setting".to_string(),
        vec!["env".into(), "key".into(), "value".into()],
    );
    schemas.insert(
        "setting_add".to_string(),
        vec!["env".into(), "key".into(), "value".into()],
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
    Ok(Program { statements: out })
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
        Stmt::Constraint(c) => {
            let body = rewrite_lits_records(c.body, schemas)?;
            Stmt::Constraint(Constraint { body, ..c })
        }
        Stmt::When(w) => {
            let guard = rewrite_lit_records(w.guard, schemas)?;
            let mut body = Vec::new();
            for s in w.body {
                body.push(rewrite_stmt_records(s, schemas)?);
            }
            Stmt::When(When {
                guard,
                body,
                span: w.span,
            })
        }
        Stmt::Module(mut c) => {
            c.body = c
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, schemas))
                .collect::<Result<Vec<_>>>()?;
            Stmt::Module(c)
        }
        Stmt::Instance(mut u) => {
            if let Some(b) = u.body {
                u.body = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Instance(u)
        }
        Stmt::PolicyPack(mut p) => {
            p.body = p
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, schemas))
                .collect::<Result<Vec<_>>>()?;
            Stmt::PolicyPack(p)
        }
        Stmt::Settings(mut s) => {
            if let Some(b) = s.body {
                s.body = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Settings(s)
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
                "wildcards not allowed in fact/head for predicate '{}'",
                atom.pred
            ),
        ));
    }

    atom.args = args;
    atom.record = None;
    Ok(atom)
}

/// `settings E [@rank] { [for body] k = v ... }` is one contribution per
/// leaf to the `settings` pseudo-type: `arg(settings, E, k, v, Rank)`. An
/// object value is flattened into dotted leaves, each a declared key.
fn desugar_settings(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Settings(s) => {
                let body = s.body.clone().unwrap_or_default();
                for f in &s.fields {
                    let rank = f.rank.or(s.rank).unwrap_or(Rank::Normal);
                    let mut leaves = Vec::new();
                    flatten_settings(&mut leaves, &f.key, f.value.clone());
                    for (key, value) in leaves {
                        let head = Atom {
                            pred: "arg".to_string(),
                            args: vec![
                                str_term(SETTINGS),
                                s.env.clone(),
                                str_term(&key),
                                value,
                                str_term(rank.name()),
                            ],
                            record: None,
                            span: f.span,
                        };
                        out.push(fact_or_rule(head, &body));
                    }
                }
            }
            _ => out.push(stmt.clone()),
        }
    }
    Ok(Program { statements: out })
}

fn flatten_settings(out: &mut Vec<(String, Term)>, key: &str, val: Term) {
    match val {
        Term::Obj(m) => {
            for (k, v) in m {
                let next = if key.is_empty() {
                    k
                } else {
                    format!("{key}.{k}")
                };
                flatten_settings(out, &next, v);
            }
        }
        other => out.push((key.to_string(), other)),
    }
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
                Stmt::Constraint(c) => {
                    if has_listcomp_lits(&c.body) {
                        changed = true;
                    }
                    let (h, cc) = rewrite_constraint_listcomps(c.clone(), &mut counter)?;
                    helpers.extend(h);
                    next_stmts.push(Stmt::Constraint(cc));
                }
                other => next_stmts.push(other.clone()),
            }
        }

        // Helper rules must be part of the same program.
        next_stmts.extend(helpers);
        current = Program {
            statements: next_stmts,
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

fn rewrite_constraint_listcomps(
    mut c: Constraint,
    counter: &mut usize,
) -> Result<(Vec<Stmt>, Constraint)> {
    let counts_all = count_vars_in_lits(&c.body);
    let mut helpers = Vec::new();
    c.body = rewrite_lits_listcomps(&c.body, &counts_all, counter, &mut helpers)?;
    locate(&mut helpers, c.span);
    Ok((helpers, c))
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
                            "comprehension free var '{v}' must be bound in the comprehension body"
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
            Stmt::ExternFn(f) => {
                externs.insert(Extern {
                    pred: f.name.clone(),
                    arity: f.args.len(),
                    span: f.span,
                });
                fns.push(f.clone());
            }
            Stmt::Import(_) => {
                // Loader-level feature, ignored in evaluator for now.
            }
            Stmt::Mixed(_) => {
                // checked by check_mixed
            }
            Stmt::Settings(_) => {
                // lowered away by desugar_settings
            }
            Stmt::Instance(_) | Stmt::Module(_) | Stmt::PolicyPack(_) | Stmt::ApplyPolicy(_) => {
                // lowered away earlier
            }
            Stmt::Decl(_) => {
                // lowered away by apply_decls
            }
            Stmt::Output(_) | Stmt::Stack(_) | Stmt::Provider(_) | Stmt::Scenario(_) => {
                // declarations: the interface and the stack, not rules
            }
            _ => statements.push(s.clone()),
        }
    }
    (Program { statements }, externs, fns)
}

fn expand_when(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        expand_when_stmt(stmt.clone(), &mut out)?;
    }
    Ok(Program { statements: out })
}

fn expand_when_stmt(stmt: Stmt, out: &mut Vec<Stmt>) -> Result<()> {
    match stmt {
        Stmt::When(w) => {
            for s in w.body {
                for s2 in apply_guard(s, &w.guard)? {
                    expand_when_stmt(s2, out)?;
                }
            }
        }
        _ => out.push(stmt),
    }
    Ok(())
}

fn apply_guard(stmt: Stmt, guard: &Lit) -> Result<Vec<Stmt>> {
    Ok(match stmt {
        Stmt::Fact(a) => vec![Stmt::Rule(RuleStmt {
            head: a,
            body: vec![guard.clone()],
        })],
        Stmt::Rule(r) => {
            let mut body = r.body;
            body.push(guard.clone());
            vec![Stmt::Rule(RuleStmt { head: r.head, body })]
        }
        Stmt::Constraint(c) => {
            let mut body = c.body;
            body.push(guard.clone());
            vec![Stmt::Constraint(Constraint { body, ..c })]
        }
        Stmt::Resource(mut r) => {
            let mut body = r.body.unwrap_or_default();
            body.push(guard.clone());
            r.body = Some(body);
            vec![Stmt::Resource(r)]
        }
        Stmt::Settings(mut s) => {
            let mut body = s.body.unwrap_or_default();
            body.push(guard.clone());
            s.body = Some(body);
            vec![Stmt::Settings(s)]
        }
        Stmt::When(w) => {
            let mut body = Vec::new();
            for s in w.body {
                body.extend(apply_guard(s, guard)?);
            }
            vec![Stmt::When(When {
                guard: w.guard,
                body,
                span: w.span,
            })]
        }
        other => vec![other],
    })
}

fn desugar_resources(program: &Program, strict: bool) -> Result<Program> {
    let mut out = Vec::new();
    let mut n = 0;
    // After the program's own statements, so its rules keep their indices
    // (their ids in `why`).
    let mut reports = Vec::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Resource(r) => {
                reports.extend(unread_field_reports(r, strict, &mut n));
                out.extend(resource_to_stmts(r.clone())?);
            }
            _ => out.push(stmt.clone()),
        }
    }
    out.extend(reports);
    Ok(Program { statements: out })
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
        let head = Atom {
            pred: "arg".to_string(),
            args: vec![
                r.typ.clone(),
                r.name.clone(),
                str_term(&f.key),
                f.value,
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
/// binds. Under strict mode the head is a `deny`.
///
/// A missing object, an instance input or output with no value, a settings
/// key an environment does not set: those hold a block back on purpose
/// (a module's resource exists where its inputs are given), and are quiet.
fn unread_field_reports(r: &Resource, strict: bool, n: &mut usize) -> Vec<Stmt> {
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
                name: "format".into(),
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
                ..atom(
                    if strict { "deny" } else { "warn" },
                    vec![str_term(UNREAD_FIELD), Term::Obj(ctx)],
                )
            },
            body,
        }));
    }
    out
}

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

fn var(s: &str) -> Term {
    Term::Var(s.to_string())
}

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
/// program's value wins).
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
        let p = format!("{prefix}.{k}");
        if let Some(v) = v {
            leaf_paths(v, &p, out);
        } else if let Term::Val(Value::Obj(m)) = t {
            leaf_paths(&Term::Val(m[k].clone()), &p, out);
        }
        out.push(p);
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
    constraints: Vec<Constraint>,
    schema: &Schema,
) -> (Vec<RuleStmt>, Vec<Atom>, Vec<Constraint>) {
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
        if r.body.is_empty() && head_reads.is_empty() {
            out_facts.push(head);
            continue;
        }
        let body_only = rewrite_body_refs(r.body, schema, &mut n);
        // A ref to an address no rule wants would make the attr join empty
        // and the field vanish: it is a deny instead.
        for (read, path) in &head_reads {
            dangling.push(dangling_ref_deny(&head, read, path, body_only.clone()));
        }
        let mut body = body_only;
        body.extend(head_reads.iter().map(|(read, _)| Lit::Pos(read.clone())));
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
    let constraints = constraints
        .into_iter()
        .map(|c| Constraint {
            body: rewrite_body_refs(c.body, schema, &mut n),
            ..c
        })
        .collect();
    (out_rules, out_facts, constraints)
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
            name: "format".into(),
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

/// The deny message for a ref to an address no rule wants.
pub const DANGLING_REF: &str = "ref to an address no rule wants";

fn rewrite_body_refs(body: Vec<Lit>, schema: &Schema, n: &mut usize) -> Vec<Lit> {
    let mut out = Vec::new();
    for l in body {
        let mut reads = Vec::new();
        let mut t = |x: &Term| rewrite_term_refs(x, schema, n, &mut reads);
        let l = match &l {
            Lit::Pos(a) => Lit::Pos(Atom {
                pred: a.pred.clone(),
                args: a.args.iter().map(&mut t).collect(),
                record: None,
                span: a.span,
            }),
            Lit::Not(a) => Lit::Not(Atom {
                pred: a.pred.clone(),
                args: a.args.iter().map(&mut t).collect(),
                record: None,
                span: a.span,
            }),
            Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
            Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
            Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
            Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
            Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
            Lit::Le(a, b) => Lit::Le(t(a), t(b)),
        };
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
        Term::Func { name, args } if name == "ref" && args.len() == 3 => {
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
                    let (top, rest) = match path.split_once('.') {
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
