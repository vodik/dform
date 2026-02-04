use crate::ast::{Atom, Constraint, Lit, Program, Resource, RuleStmt, Stmt, Term, Unique, When};
use crate::value::Value;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Lowered {
    pub program: Program,
    pub uniques: Vec<Unique>,
}

pub fn lower(program: &Program) -> Result<Lowered> {
    // In the future, imports should be handled in a loader before parsing.
    // For now, keep Import statements in the AST but drop them before eval.
    let expanded = desugar_settings(program)?;
    let expanded = expand_components(&expanded)?;
    let expanded = expand_when(&expanded)?;
    let (expanded, uniques) = extract_uniques(&expanded);
    let expanded = desugar_resources(&expanded)?;
    let expanded = desugar_comprehensions(&expanded)?;
    Ok(Lowered {
        program: expanded,
        uniques,
    })
}

fn desugar_settings(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Settings(s) => {
                let mut facts = Vec::new();
                for (k, v) in &s.fields {
                    flatten_settings(&mut facts, s.env.clone(), k, v.clone())?;
                }
                out.extend(facts);
            }
            _ => out.push(stmt.clone()),
        }
    }
    Ok(Program { statements: out })
}

fn flatten_settings(out: &mut Vec<Stmt>, env: Term, key: &str, val: Term) -> Result<()> {
    match val {
        Term::Obj(m) => {
            for (k, v) in m {
                let next = if key.is_empty() {
                    k
                } else {
                    format!("{key}.{k}")
                };
                flatten_settings(out, env.clone(), &next, v)?;
            }
        }
        other => {
            out.push(Stmt::Fact(Atom {
                pred: "setting".to_string(),
                args: vec![env, Term::Val(Value::Str(key.to_string())), other],
            }));
        }
    }
    Ok(())
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

fn extract_uniques(program: &Program) -> (Program, Vec<Unique>) {
    let mut uniques = Vec::new();
    let mut statements = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::Unique(u) => uniques.push(u.clone()),
            Stmt::Import(_) => {
                // Loader-level feature, ignored in evaluator for now.
            }
            Stmt::Settings(_) => {
                // lowered away by desugar_settings
            }
            _ => statements.push(s.clone()),
        }
    }
    (Program { statements }, uniques)
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
            fields: r
                .fields
                .into_iter()
                .map(|(k, v)| (k, rewrite_term(v, scope)))
                .collect(),
            body: r
                .body
                .map(|xs| xs.into_iter().map(|l| rewrite_lit(l, scope)).collect()),
        }),
        // These are metadata statements; leave them as-is.
        Stmt::Import(i) => Stmt::Import(i),
        Stmt::Unique(u) => Stmt::Unique(u),
        Stmt::Settings(s) => Stmt::Settings(s),
        Stmt::Component(c) => Stmt::Component(c),
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
        "arg" if atom.args.len() == 4 => {
            atom.args[1] = scoped_term(scope, rewrite_term(atom.args[1].clone(), scope));
            atom.args[3] = rewrite_term(atom.args[3].clone(), scope);
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

fn resource_to_stmts(r: Resource) -> Result<Vec<Stmt>> {
    let mut out = Vec::new();
    let body = r.body.unwrap_or_default();

    let want_atom = Atom {
        pred: "want".to_string(),
        args: vec![r.typ.clone(), r.name.clone()],
    };

    if body.is_empty() && is_ground_term(&r.typ) && is_ground_term(&r.name) {
        out.push(Stmt::Fact(want_atom.clone()));
    } else {
        out.push(Stmt::Rule(RuleStmt {
            head: want_atom.clone(),
            body: body.clone(),
        }));
    }

    for (k, v) in r.fields {
        let arg_atom = Atom {
            pred: "arg".to_string(),
            args: vec![
                r.typ.clone(),
                r.name.clone(),
                Term::Val(Value::Str(k)),
                v,
            ],
        };
        if body.is_empty() && is_ground_term(&arg_atom.args[0]) && is_ground_term(&arg_atom.args[1]) && is_ground_term(&arg_atom.args[2]) && is_ground_term(&arg_atom.args[3]) {
            out.push(Stmt::Fact(arg_atom));
        } else {
            out.push(Stmt::Rule(RuleStmt {
                head: arg_atom,
                body: body.clone(),
            }));
        }
    }

    Ok(out)
}

fn is_ground_term(t: &Term) -> bool {
    match t {
        Term::Var(_) => false,
        Term::Val(_) => true,
        Term::Func { args, .. } => args.iter().all(is_ground_term),
        Term::List(xs) => xs.iter().all(is_ground_term),
        Term::Obj(m) => m.values().all(is_ground_term),
        Term::ListComp { .. } => false,
    }
}
