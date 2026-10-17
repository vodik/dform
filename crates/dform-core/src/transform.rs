use crate::ast::{
    Atom, Extern, FieldOp, Helper, Lit, Name, Program, Rank, Resource, RuleStmt, Span, Stmt, Term,
    str_term, var,
};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::schema::Schema;
use crate::spell;
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
    /// Each let with parameters' declared parameter types, by its
    /// relation (`None` where one declares none): a call's copy reads it
    /// through `demand::copies`.
    pub let_params: BTreeMap<Name, Vec<crate::ast::LetParam>>,
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
/// The key a write of the whole subtree (`:=`) holds its value under, at
/// its path: the attribute aggregate replaces what lower ranks write at
/// or below it, and takes the key away (`engine::contributions`).
pub const WHOLE: &str = "[whole]";

/// `value` written whole at its path (`:=`): `{"[whole]": value}`.
pub fn whole(value: Term) -> Term {
    Term::Obj(BTreeMap::from([(WHOLE.to_string(), value)]))
}

/// The value a write of a whole subtree holds ([`whole`]); any other
/// term itself.
pub fn written_whole(t: &Term) -> &Term {
    match t {
        Term::Obj(m) if m.len() == 1 => m.get(WHOLE).unwrap_or(t),
        t => t,
    }
}

/// A contribution's value `v` at `at` without the marks of its writes of
/// a whole subtree ([`whole`]), the path of each pushed onto `out`.
pub fn unwhole(v: Value, at: &str, out: &mut Vec<String>) -> Value {
    match v {
        Value::Obj(mut m) if m.len() == 1 && m.contains_key(WHOLE) => {
            out.push(at.to_string());
            unwhole(m.remove(WHOLE).expect("its one key"), at, out)
        }
        Value::Obj(m) => Value::Obj(
            m.into_iter()
                .map(|(k, x)| {
                    let x = unwhole(x, &crate::address::path_join(at, &k), out);
                    (k, x)
                })
                .collect(),
        ),
        v => v,
    }
}
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
    let (expanded, externs, mut extern_fns) = drop_metadata(&expanded);
    let expanded = desugar_resources(&expanded)?;
    let expanded = desugar_comprehensions(&expanded)?;
    let mut expanded = declassified(lower_contributions(&expanded)?);
    // `set from DOC` contributes per input path (R-38).
    expanded = crate::tables::expand_set_from(expanded, &mut inputs);
    // A let with parameters is answered at each site that reads it (R-187).
    let sources = extern_fns.iter().map(|f| Name::user(&f.name)).collect();
    let let_params = expanded
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Mode(e, types) => Some((e.pred.clone(), types.clone())),
            _ => None,
        })
        .collect();
    expanded = crate::demand::answer(expanded, &sources)?;
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
        let_params,
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
    match (a.pred.name(), a.args.len()) {
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
    let segs = crate::address::path_segments(path);
    let first = segs[0].to_string();
    let value = segs[1..].iter().rev().fold(value, |v, k| {
        Term::Obj(BTreeMap::from([(
            crate::address::segment_key(k).into_owned(),
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
        pred: crate::ast::Name::user("arg"),
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
    match (r.head.pred.name(), r.head.args.as_slice()) {
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
    let mut targets: BTreeMap<Name, (Term, String)> = BTreeMap::new();
    let helpers = |r: &RuleStmt| -> Vec<Name> {
        r.body
            .iter()
            .filter_map(|l| match l {
                Lit::Pos(a) | Lit::Not(a) if a.pred.is_internal() => Some(a.pred.clone()),
                _ => None,
            })
            .collect()
    };
    let mut work: Vec<(Name, (Term, String))> = Vec::new();
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
            pred: crate::ast::Name::user(ATTR_BASE),
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
        pred: crate::ast::Name::user("attr"),
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
                ..r
            }),
            other => other,
        });
    }
    Ok(Program {
        stack: program.stack.clone(),
        statements: base_reads(out),
        nodes: program.nodes.clone(),
    })
}

/// E §2.6: a predicate is extensional (ground facts) or intensional
/// (rules), not both, unless declared `decl p(..) mixed`. Checked after
/// modules and `when` are expanded (a guarded fact is a rule), over the
/// program's own predicates: the compiler's (`want`, `arg`, ...) are
/// written both ways by design.
fn check_mixed(program: &Program) -> Result<()> {
    let mut mixed = BTreeSet::new();
    let mut facts: BTreeMap<(&Name, usize), Span> = BTreeMap::new();
    let mut rules: BTreeMap<(&Name, usize), Span> = BTreeMap::new();
    for st in &program.statements {
        match st {
            Stmt::Mixed(e) => {
                mixed.insert((&e.pred, e.arity));
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
        .filter(|((p, _), _)| !crate::loader::is_core_pred(p) && !p.is_internal())
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
            more.push(Stmt::Rule(RuleStmt::new(head, r.body.clone())));
        }
    }
    program.statements.extend(more);
    program
}

/// Each record atom's fields in their columns (H-11): the program's
/// own statements' by its `decl`s and the core's. A module's or a
/// component's records are placed as each copy is instanced, by the
/// `decl`s of the scope each is written in (`program::Records`); every
/// definition's are checked here, a copied one's or not, and the
/// definitions dropped: each copy is lowered from the program's nodes.
/// The program's `decl`s are dropped: their columns are placed.
fn apply_decls(program: &Program) -> Result<Program> {
    let records = crate::program::Records::core().with(&program.statements);
    let definitions = Definitions::of(program);
    let mut out = Vec::new();
    for s in &program.statements {
        match s {
            Stmt::Decl(_) => continue,
            Stmt::Module(m) => definitions.check(m, &records)?,
            s => out.push(records_placed(s.clone(), &records)?),
        }
    }
    Ok(Program {
        statements: out,
        stack: program.stack.clone(),
        nodes: program.nodes.clone(),
    })
}

/// The program's definitions as its nodes have them, by path.
struct Definitions<'p> {
    nodes: Option<&'p crate::program::Program>,
    by_path: BTreeMap<&'p str, crate::program::ItemId>,
}

impl<'p> Definitions<'p> {
    fn of(program: &'p Program) -> Definitions<'p> {
        let nodes = program.nodes.as_deref();
        let by_path = nodes
            .into_iter()
            .flat_map(|n| n.roots.iter().map(move |&i| (i, &n.items[i].kind)))
            .filter_map(|(i, k)| match k {
                crate::program::ItemKind::Module { path, .. } => Some((path.as_str(), i)),
                _ => None,
            })
            .collect();
        Definitions { nodes, by_path }
    }

    /// The first record of the definition `m` that its scope's `decl`s
    /// over `records` cannot place, as an error.
    fn check(&self, m: &crate::ast::Module, records: &crate::program::Records) -> Result<()> {
        let (Some(nodes), Some(&item)) = (self.nodes, self.by_path.get(m.name.as_str())) else {
            bail!("internal: no nodes of the definition {}", m.name);
        };
        crate::program::lower::records_checked(nodes, item, records)
            .map_err(|d| anyhow::Error::from(Diagnostics(d)))
    }
}

/// `stmt` with each record atom's fields in their columns by `records`:
/// a fact's, a rule's head and body, a copy's clause and body, a
/// resource's body.
fn records_placed(stmt: Stmt, records: &crate::program::Records) -> Result<Stmt> {
    let place = |a: Atom, complete: bool| {
        records
            .resolve(a, complete)
            .map_err(|d| anyhow::Error::from(Diagnostics(vec![*d])))
    };
    let lits = |lits: Vec<Lit>| -> Result<Vec<Lit>> {
        lits.into_iter()
            .map(|l| {
                Ok(match l {
                    Lit::Pos(a) => Lit::Pos(place(a, false)?),
                    Lit::Not(a) => Lit::Not(place(a, false)?),
                    other => other,
                })
            })
            .collect()
    };
    Ok(match stmt {
        Stmt::Fact(a) => Stmt::Fact(place(a, true)?),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: place(r.head, true)?,
            body: lits(r.body)?,
            ..r
        }),
        Stmt::Instance(mut u) => {
            u.body = u.body.map(lits).transpose()?;
            u.clause = u.clause.map(lits).transpose()?;
            Stmt::Instance(u)
        }
        Stmt::Use(mut u) => {
            u.body = u.body.map(lits).transpose()?;
            u.clause = u.clause.map(lits).transpose()?;
            Stmt::Use(u)
        }
        Stmt::Resource(mut r) => {
            r.body = r.body.map(lits).transpose()?;
            Stmt::Resource(r)
        }
        other => other,
    })
}

/// A head with no body and no variables is a fact; anything else a rule.
fn fact_or_rule(head: Atom, body: &[Lit]) -> Stmt {
    if body.is_empty() && head.args.iter().all(is_ground_term) {
        Stmt::Fact(head)
    } else {
        Stmt::Rule(RuleStmt::new(head, body.to_vec()))
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
            nodes: current.nodes.clone(),
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
    let mut counts_all = BTreeMap::new();
    for a in &r.head.args {
        count_vars_in_term_into(a, &mut counts_all);
    }
    merge_counts(&mut counts_all, &count_vars_in_lits(&r.body));
    let mut c = Comprehensions {
        counts_all,
        counter,
        helpers: Vec::new(),
        around: Vec::new(),
    };
    // Rewrite body first (so binding order stays intuitive).
    r.body = c.lits(&r.body)?;
    // Rewrite head terms; any joins needed for head terms are appended to the rule body.
    let (args, head_prefix) = c.terms(std::mem::take(&mut r.head.args))?;
    r.head.args = args;
    r.body.extend(head_prefix);
    let mut helpers = c.helpers;
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

/// A rule's comprehensions as they are lowered ([`rewrite_rule_listcomps`]):
/// how often each variable occurs in the rule, the run's count of helper
/// rules, the helper rules made, and the literals around the one being
/// lowered.
struct Comprehensions<'c> {
    counts_all: BTreeMap<String, usize>,
    counter: &'c mut usize,
    helpers: Vec<Stmt>,
    /// What binds a comprehension's keys outside it: the rule's literals
    /// before the one being lowered (lowered) and those after it that do
    /// not read what it binds; for the head's, the whole body.
    around: Vec<Lit>,
}

impl Comprehensions<'_> {
    fn lits(&mut self, lits: &[Lit]) -> Result<Vec<Lit>> {
        let mut out = Vec::new();
        for (i, lit) in lits.iter().enumerate() {
            if has_listcomp_lits(std::slice::from_ref(lit)) {
                self.around = out
                    .iter()
                    .cloned()
                    .chain(independent(lit, &lits[i + 1..]))
                    .collect();
            }
            out.extend(self.lit(lit.clone())?);
        }
        self.around = out.clone();
        Ok(out)
    }

    /// The literal `lit` without comprehensions: the joins its terms need,
    /// then it.
    fn lit(&mut self, lit: Lit) -> Result<Vec<Lit>> {
        match lit {
            Lit::Pos(mut a) => {
                let (args, mut prefix) = self.terms(a.args)?;
                a.args = args;
                prefix.push(Lit::Pos(a));
                Ok(prefix)
            }
            Lit::Not(mut a) => {
                let (args, mut prefix) = self.terms(a.args)?;
                a.args = args;
                prefix.push(Lit::Not(a));
                Ok(prefix)
            }
            Lit::Eq(a, b) => self.binary(a, b, Lit::Eq),
            Lit::Neq(a, b) => self.binary(a, b, Lit::Neq),
            Lit::Gt(a, b) => self.binary(a, b, Lit::Gt),
            Lit::Ge(a, b) => self.binary(a, b, Lit::Ge),
            Lit::Lt(a, b) => self.binary(a, b, Lit::Lt),
            Lit::Le(a, b) => self.binary(a, b, Lit::Le),
        }
    }

    /// The comparison `lit(a, b)` without comprehensions.
    fn binary(&mut self, a: Term, b: Term, lit: fn(Term, Term) -> Lit) -> Result<Vec<Lit>> {
        let (a2, mut prefix) = self.term(a)?;
        let (b2, pb) = self.term(b)?;
        prefix.extend(pb);
        prefix.push(lit(a2, b2));
        Ok(prefix)
    }

    /// The terms `ts` without comprehensions, and the joins they need.
    fn terms(&mut self, ts: Vec<Term>) -> Result<(Vec<Term>, Vec<Lit>)> {
        let mut prefix = Vec::new();
        let mut out = Vec::new();
        for t in ts {
            let (t2, p) = self.term(t)?;
            prefix.extend(p);
            out.push(t2);
        }
        Ok((out, prefix))
    }

    /// The term `term` without comprehensions, and the joins it needs.
    fn term(&mut self, term: Term) -> Result<(Term, Vec<Lit>)> {
        Ok(match term {
            Term::ListComp { item, body } => self.comprehension(*item, body)?,
            Term::Func { name, args } => {
                let (args, prefix) = self.terms(args)?;
                (Term::Func { name, args }, prefix)
            }
            Term::List(xs) => {
                let (xs, prefix) = self.terms(xs)?;
                (Term::List(xs), prefix)
            }
            Term::Obj(m) => {
                let mut prefix = Vec::new();
                let mut m2 = BTreeMap::new();
                for (k, v) in m {
                    let (v2, p) = self.term(v)?;
                    prefix.extend(p);
                    m2.insert(k, v2);
                }
                (Term::Obj(m2), prefix)
            }
            other => (other, Vec::new()),
        })
    }

    /// The comprehension `[item | body]`: a helper rule collecting
    /// `item` by the variables it shares with the rest of the rule (its
    /// keys), and the join that reads the list.
    fn comprehension(&mut self, item: Term, body: Vec<Lit>) -> Result<(Term, Vec<Lit>)> {
        *self.counter += 1;
        let counter = *self.counter;
        let lc_pred = Name::internal(format!("__lc_{counter}"));
        let list_var = Term::Var(format!("__lc_val_{counter}"));
        let key_vars = self.keys(&item, &body)?;
        let body = self.demanded(counter, &key_vars, body);
        unknown_collection(&body, &key_vars)?;
        let helper_head_args: Vec<Term> = key_vars
            .iter()
            .cloned()
            .map(Term::Var)
            .chain(std::iter::once(Term::Func {
                name: "collect_list".to_string(),
                args: vec![item],
            }))
            .collect();
        self.helpers.push(Stmt::Rule(RuleStmt::helper(
            Helper::Comprehension,
            Atom {
                pred: lc_pred.clone(),
                args: helper_head_args,
                record: None,
                span: Default::default(),
            },
            body,
        )));
        let mut join_args: Vec<Term> = key_vars.into_iter().map(Term::Var).collect();
        join_args.push(list_var.clone());
        let join = Lit::Pos(Atom {
            pred: lc_pred,
            args: join_args,
            record: None,
            span: Default::default(),
        });
        Ok((list_var, vec![join]))
    }

    /// `body` after the keys it only reads and the rule binds around it
    /// (`p in ports` of a parameter `ports`, `not { r(x, y) }` of a clause's
    /// `x`), as `__lc_N_in(k..) :- around`: the helper is grouped by its
    /// keys, so it must bind them, and it binds them as the rule does.
    fn demanded(&mut self, counter: usize, keys: &[String], body: Vec<Lit>) -> Vec<Lit> {
        let own = binds(&body);
        let around = crate::ast::bound_vars(&self.around);
        let given: Vec<Term> = keys
            .iter()
            .filter(|k| !own.contains(*k) && around.contains(*k))
            .cloned()
            .map(Term::Var)
            .collect();
        if given.is_empty() {
            return body;
        }
        let demand = Atom {
            pred: Name::internal(format!("__lc_{counter}_in")),
            args: given,
            record: None,
            span: Default::default(),
        };
        self.helpers.push(Stmt::Rule(RuleStmt::helper(
            Helper::Comprehension,
            demand.clone(),
            self.around.clone(),
        )));
        std::iter::once(Lit::Pos(demand)).chain(body).collect()
    }

    /// The comprehension `[item | body]`'s keys: the variables it shares
    /// with the rest of the rule, each given its values in `body`.
    fn keys(&self, item: &Term, body: &[Lit]) -> Result<Vec<String>> {
        let comp_counts = count_vars_in_listcomp(item, body);
        let body_vars = count_vars_in_lits(body);
        let mut key_vars = Vec::new();
        for (v, c) in &comp_counts {
            let total = self.counts_all.get(v).copied().unwrap_or(0);
            if total > *c {
                // Var appears outside this comprehension.
                if !body_vars.contains_key(v) {
                    bail!("`{v}` in a comprehension is given its values inside it, after its `|`");
                }
                key_vars.push(v.clone());
            }
        }
        key_vars.sort();
        key_vars.dedup();
        Ok(key_vars)
    }
}

/// The literals of `after` that do not read what `lit` binds outside its
/// comprehensions, directly or through another of them.
fn independent(lit: &Lit, after: &[Lit]) -> Vec<Lit> {
    let mut read = outer_vars(lit);
    let mut rest: Vec<&Lit> = after.iter().collect();
    loop {
        let (reading, other): (Vec<&Lit>, Vec<&Lit>) = rest
            .into_iter()
            .partition(|l| count_vars_in_lit(l).keys().any(|v| read.contains(v)));
        if reading.is_empty() {
            return other.into_iter().cloned().collect();
        }
        for l in reading {
            read.extend(count_vars_in_lit(l).into_keys());
        }
        rest = other;
    }
}

/// The variables of `lit` outside its comprehensions.
fn outer_vars(lit: &Lit) -> BTreeSet<String> {
    fn term(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Var(v) => {
                out.insert(v.clone());
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| term(a, out)),
            Term::Obj(m) => m.values().for_each(|v| term(v, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    match lit {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| term(t, &mut out)),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            term(a, &mut out);
            term(b, &mut out);
        }
    }
    out
}

/// The variables a comprehension's body binds itself: a membership reads
/// its collection, it binds only its pattern.
fn binds(body: &[Lit]) -> BTreeSet<String> {
    let patterns: Vec<Lit> = body
        .iter()
        .map(|l| match l {
            Lit::Pos(a) if is_membership(a) => Lit::Pos(Atom {
                args: a.args[1..].to_vec(),
                ..a.clone()
            }),
            l => l.clone(),
        })
        .collect();
    crate::ast::bound_vars(&patterns)
}

/// A membership as the program writes it: `p in ports`, `(k, v) in m`.
fn membership_text(a: &Atom) -> String {
    let redact = crate::query::Redactor::default();
    let w = spell::Written::new(&redact);
    let pattern: Vec<String> = a.args[1..].iter().map(|t| w.term(t)).collect();
    let pattern = match pattern.as_slice() {
        [one] => one.clone(),
        many => format!("({})", many.join(", ")),
    };
    format!("{pattern} in {}", w.term(&a.args[0]))
}

fn is_membership(a: &Atom) -> bool {
    matches!(a.pred.name(), "member" | "enumerate") && a.args.len() >= 2
}

/// `x in l` in a comprehension's body, `l` bound by nothing its helper
/// reads: an error at the membership, before the engine meets it. `keys`
/// are the variables the comprehension shares with its rule.
fn unknown_collection(body: &[Lit], keys: &[String]) -> Result<()> {
    let bound = binds(body);
    for l in body {
        let Lit::Pos(a) = l else { continue };
        if !is_membership(a) {
            continue;
        }
        let Some(unknown) = count_vars_in_term(&a.args[0])
            .into_keys()
            .find(|v| !bound.contains(v))
        else {
            continue;
        };
        let name = crate::syntax::resolve::source_name(&unknown);
        let goal = membership_text(a);
        let help = match keys.contains(&unknown) {
            true => format!(
                "`{name}` is bound only through the comprehension's own value: bind it after \
                 `where` from what does not read the list"
            ),
            false => format!("bind `{name}` after the comprehension's `|`, before `{goal}`"),
        };
        let d = Diagnostic::error(
            a.span,
            format!("`{name}` must be known before `{goal}` can run"),
        )
        .with_help(help);
        return Err(Diagnostics(vec![d]).into());
    }
    Ok(())
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
                    pred: Name::user(&f.name),
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
            nodes: program.nodes.clone(),
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
        nodes: program.nodes.clone(),
    })
}

/// `resource T N [@rank] { [for body] k = v [@rank] ... }` is `want(T, N)`
/// plus one contribution `arg(T, N, k, v, Rank)` per field, each with the
/// whole body, each rule's origin the statement. `+=` is a plain
/// contribution: the lattice decides the merge.
fn resource_to_stmts(r: Resource) -> Result<Vec<Stmt>> {
    let origin = crate::ast::Origin {
        stmt: r.span,
        call: None,
        through: None,
    };
    let mut out = resource_rules(r)?;
    for s in &mut out {
        if let Stmt::Rule(rule) = s {
            rule.origin.get_or_insert(origin);
        }
    }
    Ok(out)
}

/// `resource T N = VALUE` with a value known only when the rule runs
/// (R-126): a contribution per key of the object it is, `arg(T, N, P, V,
/// Rank) :- body, X = __body(VALUE), member(X, K, V), P =
/// __segment(K)` (`__body` is the value when it is an object): its path,
/// its value and its body.
fn body_entries(typ: &Term, name: &Term, value: Term, body: &[Lit]) -> (Term, Term, Vec<Lit>) {
    let used: BTreeSet<String> =
        count_vars_in_term(&Term::List(vec![typ.clone(), name.clone(), value.clone()]))
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
    let mut body = body.to_vec();
    body.push(Lit::Eq(
        Term::Var(x.clone()),
        Term::Func {
            name: crate::address::RESOURCE_BODY.into(),
            args: vec![value],
        },
    ));
    body.push(Lit::Pos(atom(
        "member",
        vec![Term::Var(x), Term::Var(k.clone()), Term::Var(v.clone())],
    )));
    body.push(Lit::Eq(
        Term::Var(p.clone()),
        Term::Func {
            name: crate::address::NAME_SEGMENT.into(),
            args: vec![Term::Var(k)],
        },
    ));
    (Term::Var(p), Term::Var(v), body)
}

fn resource_rules(r: Resource) -> Result<Vec<Stmt>> {
    let body = r.body.unwrap_or_default();
    let mut out = vec![fact_or_rule(
        Atom {
            pred: Name::user("want"),
            args: vec![r.typ.clone(), r.name.clone()],
            record: None,
            span: r.span,
        },
        &body,
    )];
    for f in r.fields {
        let rank = f.rank.or(r.rank).unwrap_or(Rank::Normal);
        // `resource T N = VALUE`: a contribution per key ([`body_entries`]).
        let (path, value, body) = match f.key.is_empty() {
            true => body_entries(&r.typ, &r.name, f.value, &body),
            false => {
                let value = match f.op {
                    FieldOp::Replace => whole(f.value),
                    FieldOp::Assign | FieldOp::Add => f.value,
                };
                (str_term(&f.key), value, body.clone())
            }
        };
        let head = Atom {
            pred: Name::user("arg"),
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
        let Some(exists) = read_exists(read) else {
            continue;
        };
        let (rest, bound) = binding_rest(body, reads[k..].iter().copied().collect());
        out.extend(unread_field_report(r, read, exists, rest, &bound, n));
    }
    out
}

/// What a field read `read` finds nothing without: the resource's `want`
/// row, a live object's `cloud_attr`; `None` for any other read.
fn read_exists(read: &Atom) -> Option<Atom> {
    match (read.pred.name(), read.args.as_slice()) {
        ("attr", [t, a, _, _]) => Some(atom("want", vec![t.clone(), a.clone()])),
        ("cloud_attr", [t, a, _, _]) => Some(atom(
            "cloud_attr",
            vec![t.clone(), a.clone(), Term::Wildcard, Term::Wildcard],
        )),
        _ => None,
    }
}

/// The literals of `body` but those `dropped` and what only they bind,
/// and the variables they bind.
fn binding_rest(body: &[Lit], mut dropped: BTreeSet<usize>) -> (Vec<Lit>, BTreeSet<String>) {
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
    let rest = (0..body.len())
        .filter(|j| !dropped.contains(j))
        .map(|j| body[j].clone())
        .collect();
    (rest, bound)
}

/// The two rules that report the field read `read` of `r` finding
/// nothing where `exists` holds: `__field_read_N(Xs)` over `rest` and
/// the read, and the warning over `rest`, `exists` and its negation.
/// `bound`: what `rest` binds; `n` numbers the helper.
fn unread_field_report(
    r: &Resource,
    read: &Atom,
    exists: Atom,
    rest: Vec<Lit>,
    bound: &BTreeSet<String>,
    n: &mut usize,
) -> [Stmt; 2] {
    let xs: Vec<Term> = count_vars_in_term(&Term::List(read.args.clone()))
        .into_keys()
        .filter(|v| bound.contains(v))
        .map(|v| var(&v))
        .collect();
    let helper = internal(&format!("__field_read_{n}"), xs);
    *n += 1;
    let mut ctx = BTreeMap::from([
        ("resource".to_string(), resource_named(r, bound)),
        ("read".to_string(), str_term(&spell::atom(read))),
    ]);
    if let Some(at) = diag::place(read.span) {
        ctx.insert("at".to_string(), str_term(&at));
    }
    let mut with_read = rest.clone();
    with_read.push(Lit::Pos(read.clone()));
    let found = Stmt::Rule(RuleStmt::helper(
        Helper::FieldRead,
        helper.clone(),
        with_read,
    ));
    let mut body = rest;
    body.push(Lit::Pos(exists));
    body.push(Lit::Not(helper));
    let warn = Stmt::Rule(RuleStmt::new(
        Atom {
            span: read.span,
            ..atom("warn", vec![str_term(UNREAD_FIELD), Term::Obj(ctx)])
        },
        body,
    ));
    [found, warn]
}

/// The resource `r` as a warning names it: its address formatted from
/// its type and name when `bound` binds them, else as written.
fn resource_named(r: &Resource, bound: &BTreeSet<String>) -> Term {
    let names_bound = count_vars_in_term(&Term::List(vec![r.typ.clone(), r.name.clone()]))
        .keys()
        .all(|v| bound.contains(v));
    if names_bound {
        Term::Func {
            name: crate::address::FORMAT.into(),
            args: vec![str_term("%s.%s"), r.typ.clone(), r.name.clone()],
        }
    } else {
        str_term(&format!(
            "{}.{}",
            spell::term(&r.typ).trim_matches('"'),
            spell::term(&r.name).trim_matches('"')
        ))
    }
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
    Some(Stmt::Rule(RuleStmt::helper(
        Helper::Check,
        Atom {
            span: r.span,
            ..internal(NOT_PLANNED, vec![r.typ.clone(), r.name.clone()])
        },
        body,
    )))
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

/// The atom of the program's relation `pred` over `args`, of no place.
fn atom(pred: &str, args: Vec<Term>) -> Atom {
    crate::ast::atom(pred, args, Span::default())
}

/// The atom of the compiler's relation `pred` over `args`, of no place.
fn internal(pred: &str, args: Vec<Term>) -> Atom {
    Atom {
        pred: Name::internal(pred),
        args,
        record: None,
        span: Default::default(),
    }
}

/// The schema paths the prelude mints a null for: every `computed` and every
/// `optional_computed` path outside a list element, with its class and the
/// rank the null contributes at (`@internal` for Optional+Computed: the
/// provider's own, so a program's value wins at any rank, `@default`
/// too). Minted paths do not nest (R-116): a path with
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
            out.push((t.clone(), p.clone(), *c, Rank::Internal.name()));
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

/// `resolve(__label(T, A, P), V)`, the world's value of a minted path of
/// an object state maps, and `resolved(L)`, the labels it gives a value.
fn resolve_rules() -> [RuleStmt; 2] {
    let resolve = RuleStmt::new(
        atom(
            "resolve",
            vec![
                Term::Func {
                    name: "__label".into(),
                    args: vec![var("__T"), var("__A"), var("__P")],
                },
                var("__V"),
            ],
        ),
        vec![
            Lit::Pos(atom("identity", vec![var("__T"), var("__A"), var("__Rid")])),
            Lit::Pos(atom(
                "world_attr",
                vec![var("__T"), var("__Rid"), var("__P"), var("__V")],
            )),
        ],
    );
    let resolved = RuleStmt::new(
        atom("resolved", vec![var("__L")]),
        vec![Lit::Pos(atom("resolve", vec![var("__L"), Term::Wildcard]))],
    );
    [resolve, resolved]
}

/// The compiler-generated prelude, expanded per schema row (E §2.5, §4.3):
///
/// ```text
/// resolve(__label(T, A, P), V) :- identity(T, A, Rid), world_attr(T, Rid, P, V).
/// resolved(L) :- resolve(L, _).
/// % per computed (T, P), at rank normal (computed) or internal (optional_computed):
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
    let mut out = resolve_rules().to_vec();
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
            out.push(RuleStmt::new(
                contribution(var("__V")),
                vec![
                    want.clone(),
                    Lit::Pos(atom("resolve", vec![label(&tt, &pt), var("__V")])),
                ],
            ));
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
        out.push(RuleStmt::new(contribution(null), body));
    }
    out
}

/// The facts `remote_name(T, A, Program, Name)`: dform named the object
/// at `T`/`A` `Name` where the program names it `Program` (R-189,
/// `state::StateEntry::name`).
pub const REMOTE_NAME: &str = "remote_name";

/// The rules that make a read of a remote name answer the one dform gave
/// the object (R-189): per type whose provider generates its remote name
/// (`type_remote_name(T, P)`), the name state recorded wins over the
/// program's, as an `@override` would, while the program still writes the
/// name it was generated from. A program that writes another name gets it.
///
/// ```text
/// arg(T, A, P, Name, override) :- remote_name(T, A, Program, Name),
///     arg(T, A, P, V, Rank), Rank != override, V.P == Program.
/// ```
pub fn remote_name_prelude(schema: &Schema) -> Vec<RuleStmt> {
    let mut out = Vec::new();
    for (t, p) in &schema.remote_name {
        let segs = crate::address::path_segments(p);
        let rest = segs[1..].join(".");
        let tt = str_term(t);
        let (top, value) = normalize_contribution(t, p, var("__Name"));
        let written = match rest.is_empty() {
            true => var("__V"),
            false => Term::Func {
                name: "__path".into(),
                args: vec![var("__V"), str_term(&rest)],
            },
        };
        out.push(RuleStmt::new(
            atom(
                "arg",
                vec![
                    tt.clone(),
                    var("__A"),
                    str_term(&top),
                    value,
                    str_term("override"),
                ],
            ),
            vec![
                Lit::Pos(atom(
                    REMOTE_NAME,
                    vec![tt.clone(), var("__A"), var("__Program"), var("__Name")],
                )),
                Lit::Pos(atom(
                    "arg",
                    vec![tt, var("__A"), str_term(&top), var("__V"), var("__Rank")],
                )),
                Lit::Neq(var("__Rank"), str_term("override")),
                Lit::Eq(written, var("__Program")),
            ],
        ));
    }
    out
}

/// A contribution to a plain `computed` path is a compile error: the provider
/// owns it (E §2.5). Optional+Computed paths may be set.
pub fn check_computed_writes(rules: &[RuleStmt], facts: &[Atom], schema: &Schema) -> Result<()> {
    let heads = rules
        .iter()
        .map(|r| (&r.head, spell::rule(r)))
        .chain(facts.iter().map(|a| (a, spell::atom(a))));
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
                    Term::Val(Value::Str(a)) => crate::address::Address {
                        typ: t.to_string(),
                        name: a.clone(),
                    }
                    .to_string(),
                    Term::Val(v) => format!("{t}[{}]", spell::value(v)),
                    other => format!("{t}[{}]", spell::term(other)),
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
        let p = crate::address::path_join(prefix, k);
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
/// wait, and has none. Nor has a secret's: a provider holds it and no
/// tick fills it, and a template over it is a value at once (R-218).
pub fn computed_reads(statements: &[Stmt], schema: &Schema) -> Vec<(Span, String)> {
    // A block's rules (its `want` and each field's `arg`) share one body:
    // the block is known by its reads' places.
    let mut blocks = BTreeSet::new();
    let mut out = Vec::new();
    for s in statements {
        let Stmt::Rule(r) = s else { continue };
        let block = matches!(
            (r.head.pred.name(), r.head.args.len()),
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
        let key: Vec<Span> = reads.iter().map(|a| a.span).collect();
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
            let path = paths.into_iter().find(|q| {
                schema
                    .class_of(t, q)
                    .is_some_and(|c| c != NullClass::Secret)
            })?;
            let at = match addr {
                Term::Val(Value::Str(a)) => crate::address::scope_split(a)
                    .map_or(a.as_str(), |(_, n)| n)
                    .to_string(),
                other => format!("{t}[{}]", spell::term(other)),
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
    let mut refs = RefReads { schema, n: 0 };
    let mut out_rules = Vec::new();
    let mut out_facts = Vec::new();
    // After every rule, so a rule's index (its id in `why`) does not move.
    let mut unanswered = Vec::new();
    let rules = facts
        .into_iter()
        .map(|f| RuleStmt::new(f, vec![]))
        .chain(rules);
    for r in rules {
        let mut head_reads = Vec::new();
        let head = refs.atom(&r.head, &mut head_reads);
        let wants = reference_wants(&head, schema);
        if r.body.is_empty() && head_reads.is_empty() && wants.is_empty() {
            out_facts.push(head);
            continue;
        }
        let (helper, origin) = (r.helper, r.origin);
        let body_only = refs.body(r.body);
        for read in &wants {
            unanswered.push(unanswered_read(&head, read, "", body_only.clone()));
        }
        // A ref to an address no rule wants would make the attr join empty
        // and the field vanish: it is an error at the read instead (R-194).
        for (read, path) in &head_reads {
            unanswered.push(unanswered_read(&head, read, path, body_only.clone()));
        }
        let mut body = body_only;
        body.extend(head_reads.iter().map(|(read, _)| Lit::Pos(read.clone())));
        body.extend(wants.into_iter().map(Lit::Pos));
        out_rules.extend(ref_deps(&head, &head_reads, &body));
        out_rules.push(RuleStmt {
            head,
            body,
            helper,
            origin,
        });
    }
    out_rules.extend(unanswered);
    (out_rules, out_facts)
}

/// A contribution `head` that holds a reference (`vpc = main`, R-43)
/// joins the resource's identity as a read of it would, so it holds once
/// the resource is wanted and is pending while that may derive; the value
/// stays the reference (`resources::compile_resources` gives the provider
/// the id). To an address no rule wants it is the same error. The reads
/// it joins.
fn reference_wants(head: &Atom, schema: &Schema) -> Vec<Atom> {
    if head.pred != "arg" || head.args.len() != 5 {
        return Vec::new();
    }
    let mut to = Vec::new();
    references(&head.args[3], &mut to);
    to.into_iter()
        .map(|(typ, addr)| identity_read(typ, addr, schema))
        .collect()
}

/// The value a contribution `head` reads (`head_reads`) is read now, but
/// the order of Apply still follows the ref: a contribution that reads
/// another resource's attribute depends on it
/// (`resources::compile_resources` reads `__ref_dep`), a rule over its
/// `body` each.
fn ref_deps(head: &Atom, head_reads: &[(Atom, String)], body: &[Lit]) -> Vec<RuleStmt> {
    if head.pred != "arg" || head.args.len() != 5 {
        return Vec::new();
    }
    head_reads
        .iter()
        .map(|(read, _)| {
            RuleStmt::helper(
                Helper::RefDep,
                internal(
                    REF_DEP,
                    vec![
                        head.args[0].clone(),
                        head.args[1].clone(),
                        read.args[0].clone(),
                        read.args[1].clone(),
                    ],
                ),
                body.to_vec(),
            )
        })
        .collect()
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

/// `__unanswered({type, addr, path, from, at}) :- Body, not want(T, A).`
/// for one ref `read` (`attr(T, A, P0, V)`) in the head of a rule with
/// `body`: a read of a row that does not exist, which a run refuses at its
/// site before any provider is asked (R-194, `report::Unanswered`). `from`
/// names what holds the ref: the resource `T.A` for a contribution, else
/// the head's predicate; `at` is where that is written. A row that may
/// still derive (`want` undetermined) leaves it undetermined: an open null
/// a later tick fills is no such read.
fn unanswered_read(head: &Atom, read: &Atom, path: &str, mut body: Vec<Lit>) -> RuleStmt {
    let (typ, addr) = (read.args[0].clone(), read.args[1].clone());
    let from = if head.pred == "arg" && head.args.len() == 5 {
        Term::Func {
            name: crate::address::FORMAT.into(),
            args: vec![
                str_term("%s.%s"),
                head.args[0].clone(),
                head.args[1].clone(),
            ],
        }
    } else {
        str_term(head.pred.name())
    };
    // As early as the address is bound: the rest of the body may be stuck
    // on a null (member over a computed list) for an address that is
    // wanted, and that must not make this read undetermined.
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
        // The attribute as the program writes it: `spec.jobTemplate.spec
        // .template.spec = {..}` is one object under `spec` per segment.
        if let Term::Val(Value::Str(top)) = &head.args[2] {
            let mut attr = top.clone();
            let mut v = &head.args[3];
            while let Term::Obj(m) = v
                && m.len() == 1
                && let Some((k, x)) = m.first_key_value()
            {
                attr = format!("{attr}.{k}");
                v = x;
            }
            ctx.insert("attr".to_string(), str_term(&attr));
        }
    }
    if let Some(at) = diag::place(head.span) {
        ctx.insert("at".to_string(), str_term(&at));
    }
    RuleStmt::helper(
        Helper::Check,
        Atom {
            span: head.span,
            ..internal(UNANSWERED, vec![Term::Obj(ctx)])
        },
        body,
    )
}

/// Each reference `ref(T, A, "")` of a constant type in `t`: its type and
/// address.
pub(crate) fn references(t: &Term, out: &mut Vec<(Term, Term)>) {
    match t {
        Term::Func { name, args } if name == crate::address::REF && args.len() == 3 => {
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

/// `__unanswered(Ctx)`: a contribution's read of an address no rule
/// wants ([`unanswered_read`]).
pub const UNANSWERED: &str = "__unanswered";

/// A computed attribute's references rewritten as reads
/// ([`rewrite_computed_refs`]): the schema they are of, and the count of
/// the variables the reads bind (`__refN`).
struct RefReads<'s> {
    schema: &'s Schema,
    n: usize,
}

impl RefReads<'_> {
    /// A body, each literal after the reads its references make.
    fn body(&mut self, body: Vec<Lit>) -> Vec<Lit> {
        let mut out = Vec::new();
        for l in body {
            let mut reads = Vec::new();
            let l = l
                .map(|a| Atom { record: None, ..a }, |t| t)
                .map_terms(|t| self.term(&t, &mut reads));
            out.extend(reads.into_iter().map(|(read, _)| Lit::Pos(read)));
            out.push(l);
        }
        out
    }

    /// The atom `a`, its references' reads added to `reads`.
    fn atom(&mut self, a: &Atom, reads: &mut Vec<(Atom, String)>) -> Atom {
        Atom {
            pred: a.pred.clone(),
            args: a.args.iter().map(|t| self.term(t, reads)).collect(),
            record: a.record.clone(),
            span: a.span,
        }
    }

    /// The term `t`, its references' reads added to `reads`.
    fn term(&mut self, t: &Term, reads: &mut Vec<(Atom, String)>) -> Term {
        match t {
            Term::Func { name, args } if name == crate::address::REF && args.len() == 3 => {
                if let Some(read) = self.reference(args, reads) {
                    return read;
                }
                Term::Func {
                    name: name.clone(),
                    args: args.iter().map(|x| self.term(x, reads)).collect(),
                }
            }
            Term::Func { name, args } => Term::Func {
                name: name.clone(),
                args: args.iter().map(|x| self.term(x, reads)).collect(),
            },
            Term::List(xs) => Term::List(xs.iter().map(|x| self.term(x, reads)).collect()),
            Term::Obj(m) => Term::Obj(
                m.iter()
                    .map(|(k, x)| (k.clone(), self.term(x, reads)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// The reference `ref(T, A, P)` (its `args`) to a computed attribute
    /// of the schema's: the variable its read binds (the read added to
    /// `reads`), a path into it when `P` goes deeper; a type no provider
    /// serves, its null. `None` for any other reference.
    fn reference(&mut self, args: &[Term], reads: &mut Vec<(Atom, String)>) -> Option<Term> {
        let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) = (&args[0], &args[2]) else {
            return None;
        };
        let schema = self.schema;
        let class = schema
            .class_of(typ, path)
            .or_else(|| schema.optional_computed_class(typ, path))?;
        let addr = self.term(&args[1], reads);
        if !schema.provider_of.contains_key(typ) {
            let ty = schema
                .attr(typ, path)
                .map(|a| a.ty.clone())
                .unwrap_or_default();
            return Some(Term::Func {
                name: "__null".into(),
                args: vec![
                    args[0].clone(),
                    addr,
                    args[2].clone(),
                    str_term(class.name()),
                    str_term(&ty),
                ],
            });
        }
        self.n += 1;
        let v = var(&format!("__ref{}", self.n));
        let (top, rest) = match crate::address::path_split_first(path) {
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
        Some(match rest {
            None => v,
            Some(rest) => Term::Func {
                name: "__path".into(),
                args: vec![v, str_term(rest)],
            },
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A block that interpolates a computed value waits for the tick that
    /// makes it, and is told so; one that interpolates a secret a
    /// provider holds waits for nothing, and is told nothing.
    #[test]
    fn a_secrets_read_is_no_wait() {
        let schema = crate::parser::parse_program(
            "type_attr(\"db.postgres\", \"endpoint\", \"string\", [\"computed\"])\n\
             type_attr(\"vault.token\", \"value\", \"string\", [\"computed\", \"sensitive\"])\n",
        )
        .unwrap()
        .statements
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Fact(a) => Some(a),
            _ => None,
        })
        .collect::<Vec<_>>();
        let schema = Schema::from_facts(&schema).unwrap();
        let program = crate::parser::parse_program(
            "resource db.postgres db { size = 1 }\n\
             resource vault.token t { name = \"t\" }\n\
             resource compute.vm app { url = \"pg://${db.endpoint}/app\" }\n\
             resource compute.vm vm { user_data = \"token: ${t.value}\" }\n",
        )
        .unwrap();
        let lowered = lower(&program).unwrap();
        let notes: Vec<String> = computed_reads(&lowered.program.statements, &schema)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(
            notes,
            [
                "<input>:3:41: reads `db.endpoint` now, a computed value: this block waits for the tick that \
              creates `db`; a field written `= db.endpoint` would be an edge and apply with it"
            ]
        );
    }
}
