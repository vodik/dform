use crate::ast::{Atom, Constraint, Extern, Lit, Program, Rank, Resource, RuleStmt, Settings, Stmt, Term, When};
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Lowered {
    pub program: Program,
    /// `extern p/N.` declarations.
    pub externs: BTreeSet<Extern>,
}

pub fn lower(program: &Program) -> Result<Lowered> {
    let program = apply_decls(program)?;
    // In the future, imports should be handled in a loader before parsing.
    // For now, keep Import statements in the AST but drop them before eval.
    let expanded = expand_component_defs_and_uses(&program)?;
    let expanded = expand_policy_packs(&expanded)?;
    let expanded = expand_components(&expanded)?;
    let expanded = expand_when(&expanded)?;
    let expanded = desugar_settings(&expanded)?;
    let (expanded, externs) = drop_metadata(&expanded);
    let expanded = desugar_resources(&expanded)?;
    let expanded = desugar_comprehensions(&expanded)?;
    let expanded = lower_contributions(&expanded)?;
    Ok(Lowered { program: expanded, externs })
}

/// Rank of a contribution in the core form `arg(T, A, P, V, Rank)`.
pub const NORMAL: &str = "normal";

/// Pseudo-types of the attribute aggregate (E §2.5): settings are addressed
/// by environment, outputs by component scope ("" for the root program).
pub const SETTINGS: &str = "settings";
pub const OUTPUT: &str = "output";

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
    let value = rest.iter().rev().fold(value, |v, k| Term::Obj(BTreeMap::from([(k.to_string(), v)])));
    (first, value)
}

/// The core form of a contribution head: `arg(T, A, P, V, Rank)` with a
/// constant path normalized.
fn contribution_head(a: Atom) -> Result<Atom> {
    if a.pred == "merge_rule" {
        bail!("merge_rule is gone: a path's lattice is declared with type_lattice(Type, Path, flat|map|set)");
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
    Ok(Atom { pred: "arg".into(), args: vec![typ, addr, path, value, rank], record: None })
}

/// A body read of a contribution predicate is a read of the aggregate:
/// `attr(T, A, P, V)`, the collapsed value (E §2.5).
fn attr_read(a: Atom) -> Result<Atom> {
    if a.pred == "arg" && a.args.len() == 5 {
        bail!("arg/5 in a rule body reads raw contributions; read the collapsed attr(T, A, P, V) instead");
    }
    let Some((typ, addr, path, value)) = contribution_parts(&a) else {
        return Ok(a);
    };
    if let (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) = (&typ, &path) {
        if t != SETTINGS && t != OUTPUT && p.contains('.') {
            bail!("{}(..., {p:?}, ...) in a rule body: read the top-level attribute and destructure it", a.pred);
        }
    }
    Ok(Atom { pred: "attr".into(), args: vec![typ, addr, path, value], record: None })
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
            Stmt::Rule(r) => Stmt::Rule(RuleStmt { head: contribution_head(r.head)?, body: attr_lits(r.body)? }),
            Stmt::Constraint(c) => Stmt::Constraint(Constraint { message: c.message, body: attr_lits(c.body)? }),
            other => other,
        });
    }
    Ok(Program { statements: out })
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
    schemas.insert(
        "component_scope".to_string(),
        vec!["comp".into(), "inst".into(), "scope".into()],
    );
    schemas.insert("input".to_string(), vec!["key".into(), "value".into()]);
    schemas.insert("data".to_string(), vec!["key".into(), "value".into()]);
    schemas.insert("merge_rule".to_string(), vec!["type".into(), "path".into(), "op".into()]);
    schemas.insert("param".to_string(), vec!["scope".into(), "key".into(), "value".into()]);
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
        out.push(rewrite_stmt_records(s.clone(), &schemas, Ctx::Body)?);
    }
    Ok(Program { statements: out })
}

#[derive(Copy, Clone)]
enum Ctx {
    Fact,
    Head,
    Body,
}

fn rewrite_stmt_records(stmt: Stmt, schemas: &BTreeMap<String, Vec<String>>, ctx: Ctx) -> Result<Stmt> {
    Ok(match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom_records(a, schemas, Ctx::Fact)?),
        Stmt::Rule(r) => {
            let head = rewrite_atom_records(r.head, schemas, Ctx::Head)?;
            let body = rewrite_lits_records(r.body, schemas)?;
            Stmt::Rule(RuleStmt { head, body })
        }
        Stmt::Constraint(c) => {
            let body = rewrite_lits_records(c.body, schemas)?;
            Stmt::Constraint(Constraint {
                message: c.message,
                body,
            })
        }
        Stmt::When(w) => {
            let guard = rewrite_lit_records(w.guard, schemas)?;
            let mut body = Vec::new();
            for s in w.body {
                body.push(rewrite_stmt_records(s, schemas, ctx)?);
            }
            Stmt::When(When { guard, body })
        }
        Stmt::Component(mut c) => {
            c.body = c
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, schemas, ctx))
                .collect::<Result<Vec<_>>>()?;
            Stmt::Component(c)
        }
        Stmt::ComponentDef(mut c) => {
            c.body = c
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, schemas, ctx))
                .collect::<Result<Vec<_>>>()?;
            Stmt::ComponentDef(c)
        }
        Stmt::Use(mut u) => {
            if let Some(b) = u.body {
                u.body = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Use(u)
        }
        Stmt::PolicyPack(mut p) => {
            p.body = p
                .body
                .into_iter()
                .map(|s| rewrite_stmt_records(s, schemas, ctx))
                .collect::<Result<Vec<_>>>()?;
            Stmt::PolicyPack(p)
        }
        Stmt::Settings(mut s) => {
            if let Some(b) = s.body {
                s.body = Some(rewrite_lits_records(b, schemas)?);
            }
            Stmt::Settings(s)
        }
        other => other,
    })
}

fn rewrite_lits_records(lits: Vec<Lit>, schemas: &BTreeMap<String, Vec<String>>) -> Result<Vec<Lit>> {
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

fn rewrite_atom_records(mut atom: Atom, schemas: &BTreeMap<String, Vec<String>>, ctx: Ctx) -> Result<Atom> {
    let Some(fields) = atom.record.take() else {
        return Ok(atom);
    };
    let Some(order) = schemas.get(&atom.pred) else {
        bail!("no schema for predicate '{}' (add decl {} {{ ... }})", atom.pred, atom.pred);
    };

    // No extra fields.
    for k in fields.keys() {
        if !order.iter().any(|x| x == k) {
            bail!("unknown field '{k}' for predicate '{}'", atom.pred);
        }
    }

    let require_complete = matches!(ctx, Ctx::Fact | Ctx::Head);
    let mut args = Vec::with_capacity(order.len());
    for f in order {
        match fields.get(f) {
            Some(t) => args.push(t.clone()),
            None => {
                if require_complete {
                    bail!("missing field '{f}' for predicate '{}'", atom.pred);
                }
                args.push(Term::Wildcard);
            }
        }
    }
    if require_complete && args.iter().any(|t| matches!(t, Term::Wildcard)) {
        bail!("wildcards not allowed in fact/head for predicate '{}'", atom.pred);
    }

    atom.args = args;
    atom.record = None;
    Ok(atom)
}

fn expand_component_defs_and_uses(program: &Program) -> Result<Program> {
    let mut defs: BTreeMap<String, Vec<Stmt>> = BTreeMap::new();
    for s in &program.statements {
        if let Stmt::ComponentDef(d) = s {
            defs.insert(d.name.clone(), d.body.clone());
        }
    }

    let mut out = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::ComponentDef(_) => {}
            Stmt::Use(u) => {
                let Some(body) = defs.get(&u.name) else {
                    bail!("use references unknown component_def '{}'", u.name);
                };

                let mut comp_body: Vec<Stmt> = Vec::new();
                // Params become `param(Key, Value)` statements inside the component.
                for (k, v) in &u.params {
                    let atom = Atom {
                        pred: "param".to_string(),
                        args: vec![Term::Val(Value::Str(k.clone())), v.clone()],
                        record: None,
                    };
                    if let Some(b) = &u.body {
                        comp_body.push(Stmt::Rule(RuleStmt {
                            head: atom,
                            body: b.clone(),
                        }));
                    } else {
                        comp_body.push(Stmt::Fact(atom));
                    }
                }

                comp_body.extend(body.clone());
                out.push(Stmt::Component(crate::ast::Component {
                    comp: u.name.clone(),
                    inst: u.inst.clone(),
                    body: comp_body,
                }));
            }
            Stmt::Component(_) => {
                // Allow legacy direct component usage.
                out.push(s.clone());
            }
            other => out.push(other.clone()),
        }
    }

    Ok(Program { statements: out })
}

fn expand_policy_packs(program: &Program) -> Result<Program> {
    let mut packs: BTreeMap<String, Vec<Stmt>> = BTreeMap::new();
    let mut applied = Vec::new();

    for s in &program.statements {
        match s {
            Stmt::PolicyPack(p) => {
                packs.insert(p.name.clone(), p.body.clone());
            }
            Stmt::ApplyPolicy(a) => applied.push(a.name.clone()),
            _ => {}
        }
    }

    let mut out = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::PolicyPack(_) => {}
            Stmt::ApplyPolicy(_) => {}
            other => out.push(other.clone()),
        }
    }

    for name in applied {
        let Some(body) = packs.get(&name) else {
            bail!("apply_policy references unknown policy_pack '{name}'");
        };
        out.extend(body.clone());
    }

    Ok(Program { statements: out })
}

/// `settings E [@rank] { k = v, ... } [:- body].` is one contribution per
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
                            args: vec![str_term(SETTINGS), s.env.clone(), str_term(&key), value, str_term(rank.name())],
                            record: None,
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
                let next = if key.is_empty() { k } else { format!("{key}.{k}") };
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
        Stmt::Rule(RuleStmt { head, body: body.to_vec() })
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
    Ok((helpers, c))
}

fn rewrite_rule_listcomps(
    mut r: RuleStmt,
    counter: &mut usize,
) -> Result<(Vec<Stmt>, RuleStmt)> {
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

    Ok((helpers, r))
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
                        bail!("comprehension free var '{v}' must be bound in the comprehension body");
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
                },
                body,
            }));

            let mut join_args: Vec<Term> = key_vars
                .iter()
                .cloned()
                .map(Term::Var)
                .collect();
            join_args.push(list_var.clone());

            (
                list_var,
                vec![Lit::Pos(Atom {
                    pred: lc_pred,
                    args: join_args,
                    record: None,
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
fn drop_metadata(program: &Program) -> (Program, BTreeSet<Extern>) {
    let mut statements = Vec::new();
    let mut externs = BTreeSet::new();
    for s in &program.statements {
        match s {
            Stmt::Unique(_) => {}
            Stmt::Extern(e) => {
                externs.insert(e.clone());
            }
            Stmt::Import(_) => {
                // Loader-level feature, ignored in evaluator for now.
            }
            Stmt::Settings(_) => {
                // lowered away by desugar_settings
            }
            Stmt::Use(_) | Stmt::ComponentDef(_) | Stmt::PolicyPack(_) | Stmt::ApplyPolicy(_) => {
                // lowered away earlier
            }
            Stmt::Decl(_) => {
                // lowered away by apply_decls
            }
            _ => statements.push(s.clone()),
        }
    }
    (Program { statements }, externs)
}

fn expand_components(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        expand_component_stmt(stmt, &mut out)?;
    }
    Ok(Program { statements: out })
}

fn expand_component_stmt(stmt: &Stmt, out: &mut Vec<Stmt>) -> Result<()> {
    match stmt {
        Stmt::Component(c) => {
            let scope = format!("{}.{}", c.comp, c.inst);
            out.push(Stmt::Fact(Atom {
                pred: "component_scope".to_string(),
                args: vec![
                    Term::Val(Value::Str(c.comp.clone())),
                    Term::Val(Value::Str(c.inst.clone())),
                    Term::Val(Value::Str(scope.clone())),
                ],
                record: None,
            }));

            for inner in &c.body {
                if matches!(inner, Stmt::Component(_)) {
                    bail!("nested components are not supported yet");
                }
                out.push(rewrite_stmt(inner.clone(), &scope));
            }
        }
        _ => out.push(stmt.clone()),
    }
    Ok(())
}

fn rewrite_stmt(stmt: Stmt, scope: &str) -> Stmt {
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom(a, scope)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rewrite_atom(r.head, scope),
            body: r.body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        }),
        Stmt::Constraint(c) => Stmt::Constraint(Constraint {
            message: c.message,
            body: c.body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        }),
        Stmt::When(w) => Stmt::When(When {
            guard: rewrite_lit(w.guard, scope),
            body: w.body.into_iter().map(|s| rewrite_stmt(s, scope)).collect(),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: rewrite_term(r.typ, scope),
            name: scoped_term(scope, rewrite_term(r.name, scope)),
            rank: r.rank,
            fields: r
                .fields
                .into_iter()
                .map(|f| crate::ast::FieldAssign { value: rewrite_term(f.value, scope), ..f })
                .collect(),
            body: r
                .body
                .map(|xs| xs.into_iter().map(|l| rewrite_lit(l, scope)).collect()),
        }),
        // Settings are addressed by environment, not by scope: only the
        // values and the body are rewritten.
        Stmt::Settings(s) => Stmt::Settings(Settings {
            fields: s
                .fields
                .into_iter()
                .map(|f| crate::ast::FieldAssign { value: rewrite_term(f.value, scope), ..f })
                .collect(),
            body: s.body.map(|xs| xs.into_iter().map(|l| rewrite_lit(l, scope)).collect()),
            ..s
        }),
        // These are metadata statements; leave them as-is.
        Stmt::Import(i) => Stmt::Import(i),
        Stmt::Unique(u) => Stmt::Unique(u),
        Stmt::ComponentDef(d) => Stmt::ComponentDef(d),
        Stmt::Use(u) => Stmt::Use(u),
        Stmt::PolicyPack(p) => Stmt::PolicyPack(p),
        Stmt::ApplyPolicy(a) => Stmt::ApplyPolicy(a),
        Stmt::Component(c) => Stmt::Component(c),
        Stmt::Decl(d) => Stmt::Decl(d),
        Stmt::Extern(e) => Stmt::Extern(e),
    }
}

fn rewrite_lit(lit: Lit, scope: &str) -> Lit {
    match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom(a, scope)),
        Lit::Not(a) => Lit::Not(rewrite_atom(a, scope)),
        Lit::Eq(a, b) => Lit::Eq(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Neq(a, b) => Lit::Neq(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Gt(a, b) => Lit::Gt(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Ge(a, b) => Lit::Ge(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Lt(a, b) => Lit::Lt(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Le(a, b) => Lit::Le(rewrite_term(a, scope), rewrite_term(b, scope)),
    }
}

fn rewrite_atom(mut atom: Atom, scope: &str) -> Atom {
    match atom.pred.as_str() {
        "want" if atom.args.len() == 2 => {
            atom.args[1] = scoped_term(scope, rewrite_term(atom.args[1].clone(), scope));
        }
        "arg" if atom.args.len() == 4 || atom.args.len() == 5 => {
            atom.args[1] = scoped_term(scope, rewrite_term(atom.args[1].clone(), scope));
            atom.args[3] = rewrite_term(atom.args[3].clone(), scope);
        }
        "arg_add" if atom.args.len() == 4 => {
            atom.args[1] = scoped_term(scope, rewrite_term(atom.args[1].clone(), scope));
            atom.args[3] = rewrite_term(atom.args[3].clone(), scope);
        }
        "adopt" if atom.args.len() == 3 => {
            atom.args[1] = scoped_term(scope, rewrite_term(atom.args[1].clone(), scope));
            atom.args[2] = rewrite_term(atom.args[2].clone(), scope);
        }
        "param" if atom.args.len() == 2 => {
            // Scope parameters so multiple component instances don't collide.
            let key = rewrite_term(atom.args[0].clone(), scope);
            let val = rewrite_term(atom.args[1].clone(), scope);
            atom.args = vec![Term::Val(Value::Str(scope.to_string())), key, val];
        }
        "param" if atom.args.len() == 3 => {
            // Fully-qualified param/3; rewrite nested terms.
            atom.args = atom
                .args
                .into_iter()
                .map(|t| rewrite_term(t, scope))
                .collect();
        }
        // Sugar: inside a component, allow output(Key, Value)
        // which becomes output(Scope, Key, Value).
        "output" if atom.args.len() == 2 => {
            let key = rewrite_term(atom.args[0].clone(), scope);
            let val = rewrite_term(atom.args[1].clone(), scope);
            atom.args = vec![Term::Val(Value::Str(scope.to_string())), key, val];
        }
        // output/3 is the fully-qualified form.
        _ => {
            atom.args = atom
                .args
                .into_iter()
                .map(|t| rewrite_term(t, scope))
                .collect();
        }
    }
    atom
}

fn rewrite_term(term: Term, scope: &str) -> Term {
    match term {
        Term::Val(v) => Term::Val(v),
        Term::Var(v) => Term::Var(v),
        Term::Wildcard => Term::Wildcard,
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rewrite_term(t, scope)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rewrite_term(v, scope)))
                .collect::<BTreeMap<_, _>>(),
        ),
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rewrite_term(*item, scope)),
            body: body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        },
        Term::Func { name, args } => {
            if name == "ref" && args.len() == 3 {
                let mut out = args;
                out[0] = rewrite_term(out[0].clone(), scope);
                out[1] = scoped_term(scope, rewrite_term(out[1].clone(), scope));
                out[2] = rewrite_term(out[2].clone(), scope);
                return Term::Func { name, args: out };
            }
            Term::Func {
                name,
                args: args.into_iter().map(|t| rewrite_term(t, scope)).collect(),
            }
        }
    }
}

fn scoped_term(scope: &str, name_term: Term) -> Term {
    Term::Func {
        name: "scoped".to_string(),
        args: vec![Term::Val(Value::Str(scope.to_string())), name_term],
    }
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
            vec![Stmt::Constraint(Constraint {
                message: c.message,
                body,
            })]
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
            vec![Stmt::When(When { guard: w.guard, body })]
        }
        other => vec![other],
    })
}

fn desugar_resources(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Resource(r) => {
                out.extend(resource_to_stmts(r.clone())?);
            }
            _ => out.push(stmt.clone()),
        }
    }
    Ok(Program { statements: out })
}

/// `resource T N [@rank] { k = v [@rank], ... } [:- body].` is `want(T, N)`
/// plus one contribution `arg(T, N, k, v, Rank)` per field, each with the
/// whole body. `+=` is a plain contribution: the lattice decides the merge.
fn resource_to_stmts(r: Resource) -> Result<Vec<Stmt>> {
    let body = r.body.unwrap_or_default();
    let mut out = vec![fact_or_rule(
        Atom { pred: "want".to_string(), args: vec![r.typ.clone(), r.name.clone()], record: None },
        &body,
    )];
    for f in r.fields {
        let rank = f.rank.or(r.rank).unwrap_or(Rank::Normal);
        let head = Atom {
            pred: "arg".to_string(),
            args: vec![r.typ.clone(), r.name.clone(), str_term(&f.key), f.value, str_term(rank.name())],
            record: None,
        };
        out.push(fact_or_rule(head, &body));
    }
    Ok(out)
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
