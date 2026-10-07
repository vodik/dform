//! Modules, components and instances (DESIGN.org R-65,
//! proposals/I-modules.md).
//!
//! A `use m [as n] { k = V } where B` imports module `m`, a file, and an
//! `resource c n { k = V } where B` copies component `c`, an item of one:
//! one mechanism, the body under the scope `n`:
//!
//! - resource names are scoped (`n.name`, its address `T["n.name"]`, R-112); in
//!   a component a resource written as a variable is the copy's own, in a
//!   module any its user sees (the module's rules merge into the user's
//!   scope);
//! - every predicate the body defines is `n`'s (`n::p`, a name no source
//!   can spell): a module's read by its user as `n.p`, a component's
//!   private to the copy, a value leaving it through an output;
//! - `input k: T [= D] [where R]` is read inside as `k(V)`: the collapsed
//!   cell `(input, n, k)` of the attribute aggregate, where the block's
//!   `k = V :- B` contributes at the normal rank and `D` at `@default`;
//! - `output k: T` declares an output and `output k = t` (or a rule for
//!   `output(k, V)`) defines it, readable anywhere as `output(n, k, V)`;
//! - a name the body does not define reads outward, its user's;
//! - for a copy, the fact `instance_of(c, user, n)`, which `c[t]`
//!   enumerates; with a clause `B`, the body's own gate `n::__instance(c)
//!   :- B` holds every rule and resource of it.
//!
//! A body is expanded inside out: its own `use`s and `instance`s are
//! expanded first, their names relative to it, and the body's scope is put
//! in front of them (`n/inner/x`, `output("n.inner", k, V)`); a name the
//! resolver wrote as its user's, `__scope(t)`, is left as it is. What a
//! module or a copy writes is not granted: ranks are the ownership model,
//! and the stratifier partitions a write by its head's constant type and
//! path.

use crate::ast::{
    Atom, FieldAssign, InputDecl, Lit, OutputDecl, Program, Resource, RuleStmt, Span, Stmt, Term,
    TypeExpr, atom, str_term,
};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::inputs::Declared;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The attribute aggregate's pseudo-type for inputs: `(input, Scope, key)`,
/// scope `""` for the stack's own.
pub const INPUT: &str = "input";

/// The attribute aggregate's pseudo-type for `let` (R-3): `(let, Scope,
/// k)`, scope `""` for the program's own, `n` in the instance `n`, `m` in
/// the activation `use m`. The resolver writes a `let` as `let(k, t, rank)`, a
/// head no source can spell, and `lets` makes it the cell's contribution.
pub const LET: &str = "let";

/// A head with no body and no variables is a fact; anything else a rule.
fn fact_or_rule(head: Atom, body: Vec<Lit>) -> Stmt {
    if body.is_empty() && head.args.iter().all(is_ground) {
        Stmt::Fact(head)
    } else {
        Stmt::Rule(RuleStmt { head, body })
    }
}

fn is_ground(t: &Term) -> bool {
    match t {
        Term::Var(_) | Term::Wildcard | Term::ListComp { .. } => false,
        Term::Val(_) => true,
        Term::Func { args, .. } => args.iter().all(is_ground),
        Term::List(xs) => xs.iter().all(is_ground),
        Term::Obj(m) => m.values().all(is_ground),
    }
}

/// A component's interface: what its top-level statements declare.
#[derive(Default)]
struct Interface {
    inputs: Vec<InputDecl>,
    /// `input p`: relations the user gives the rows of (R-55).
    relations: Vec<crate::ast::Extern>,
    /// `output p`: relations the copy exports.
    relation_outputs: Vec<OutputDecl>,
    outputs: BTreeMap<String, OutputDecl>,
    output_values: Vec<OutputDecl>,
}

/// Split a module's or component's body into its interface and its
/// statements; the definitions nested in it are left out.
fn interface(body: &[Stmt]) -> (Interface, Vec<Stmt>) {
    let mut i = Interface::default();
    let mut rest = Vec::new();
    for s in body {
        match s {
            Stmt::Input(d) => i.inputs.push(d.clone()),
            Stmt::RelationInput(e) => i.relations.push(e.clone()),
            Stmt::Output(o) if o.relation.is_some() => i.relation_outputs.push(o.clone()),
            Stmt::Output(o) if o.value.is_none() => {
                i.outputs.entry(o.name.clone()).or_insert_with(|| o.clone());
            }
            Stmt::Output(o) => i.output_values.push(o.clone()),
            Stmt::Module(_) => {}
            other => rest.push(other.clone()),
        }
    }
    (i, rest)
}

/// Every predicate the statements define (heads and facts), by name, with
/// the arity and span of its first definition.
fn defined_preds(stmts: &[Stmt], out: &mut BTreeMap<String, (usize, Span)>) {
    for s in stmts {
        match s {
            Stmt::Fact(a) | Stmt::Rule(RuleStmt { head: a, .. }) => {
                let (pred, arity) = match let_key(a) {
                    Some(k) => (k.to_string(), 1),
                    None => (a.pred.clone(), a.args.len()),
                };
                out.entry(pred).or_insert((arity, a.span));
            }
            // A `decl` of a relation with no rules: a module of facts may
            // declare one it has no rows of yet (R-39).
            Stmt::Extern(e) => {
                out.entry(e.pred.clone()).or_insert((e.arity, e.span));
            }
            _ => {}
        }
    }
}

/// The key `k` of a `let(k, t, rank)` head.
fn let_key(a: &Atom) -> Option<&str> {
    match (a.pred.as_str(), a.args.as_slice()) {
        (LET, [Term::Val(Value::Str(k)), _, _]) => Some(k),
        _ => None,
    }
}

/// Each `let(k, t, rank) :- B` of `stmts` as its contribution to the cell
/// `(let, scope, k)`, `arg("let", scope, "k", t, rank) :- B`, and one
/// reader per key, `k(V) :- attr("let", scope, "k", V)` (`k` renamed by
/// `names` in an instance or an activation): a read of `k` is the
/// collapsed cell, so two rows that agree are one value and two that
/// disagree a conflict.
/// `__secret_let(k, path)`: the `let k` is declared secret at `path`
/// (`""` all of it), which [`lets`] makes the scope's `secret_cell`.
pub const SECRET_LET: &str = "__secret_let";

fn lets(stmts: Vec<Stmt>, scope: &str, names: Option<&Names>) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    let mut keys: BTreeMap<String, Span> = BTreeMap::new();
    // A `let` declared `secret(T)`, or of an object type with a secret
    // field (`SECRET_LET`, R-153), is a secret cell, each such path of it.
    let mut secret: Vec<(String, Span)> = Vec::new();
    for s in stmts {
        let (head, body) = match s {
            Stmt::Fact(a) if a.pred == SECRET_LET => {
                if let [Term::Val(Value::Str(k)), Term::Val(Value::Str(q))] = a.args.as_slice() {
                    secret.push((crate::types::dotted(k, q), a.span));
                }
                continue;
            }
            Stmt::Fact(a) if let_key(&a).is_some() => (a, Vec::new()),
            Stmt::Rule(r) if let_key(&r.head).is_some() => (r.head, r.body),
            other => {
                out.push(other);
                continue;
            }
        };
        let [k, v, rank]: [Term; 3] = head.args.try_into().expect("let(k, t, rank)");
        let Term::Val(Value::Str(key)) = &k else {
            unreachable!("let_key")
        };
        keys.entry(key.clone()).or_insert(head.span);
        let arg = Atom {
            pred: "arg".into(),
            args: vec![str_term(LET), str_term(scope), k, v, rank],
            ..head
        };
        out.push(fact_or_rule(arg, body));
    }
    for (k, span) in &secret {
        let root = k.split('.').next().unwrap_or(k);
        if keys.contains_key(root) {
            out.push(Stmt::Fact(atom(
                crate::transform::SECRET_CELL,
                vec![str_term(LET), str_term(scope), str_term(k)],
                *span,
            )));
        }
    }
    for (k, span) in keys {
        let pred = names
            .and_then(|n| n.get(&k))
            .cloned()
            .unwrap_or_else(|| k.clone());
        let v = Term::Var("V".into());
        out.push(Stmt::Rule(RuleStmt {
            head: atom(&pred, vec![v.clone()], span),
            body: vec![Lit::Pos(atom(
                "attr",
                vec![str_term(LET), str_term(scope), str_term(&k), v],
                span,
            ))],
        }));
    }
    out
}

/// One declaration of a name a scope declares more than once (R-104):
/// what it is as written (`resource pg_aws db`), its clause, where.
pub struct Alternative {
    pub what: String,
    pub clause: Vec<Lit>,
    pub span: Span,
}

/// The relation each declaration of a guarded name holds in, `__declared(
/// name, i)`, by its clause: a pair that both hold is the deny naming
/// both.
pub const DECLARED: &str = "__declared";

/// A name declared several times in one scope, each under a clause
/// (R-104): the compiler does not prove the clauses exclusive, the
/// evaluation does. Each declaration's clause derives `__declared(name,
/// i)`, and two that both hold are a deny naming both sites.
pub fn exclusive(name: &str, alts: &[Alternative]) -> Vec<Stmt> {
    let mut out: Vec<Stmt> = alts
        .iter()
        .enumerate()
        .map(|(i, a)| declared(name, i, a.clause.clone(), a.span))
        .collect();
    let sites: Vec<(String, Span)> = alts.iter().map(|a| (a.what.clone(), a.span)).collect();
    out.extend(denies(name, &sites));
    out
}

/// The `i`th declaration of `name` holds while its clause does:
/// `__declared(name, i) :- clause`.
pub fn declared(name: &str, i: usize, clause: Vec<Lit>, span: Span) -> Stmt {
    fact_or_rule(held(name, i, span), clause)
}

fn held(name: &str, i: usize, span: Span) -> Atom {
    atom(
        DECLARED,
        vec![str_term(name), Term::Val(Value::Int(i as i64))],
        span,
    )
}

/// A deny for each pair of the declarations of `name` (what each is, and
/// where) that both hold, naming both.
pub fn denies(name: &str, sites: &[(String, Span)]) -> Vec<Stmt> {
    let site = |(what, span): &(String, Span)| match diag::at(*span) {
        Some(at) => format!("`{what}` at {at}"),
        None => format!("`{what}`"),
    };
    let mut out = Vec::new();
    for (i, a) in sites.iter().enumerate() {
        for (j, b) in sites.iter().enumerate().skip(i + 1) {
            let msg = format!(
                "`{name}` is declared twice and both declarations hold: {} and {}",
                site(a),
                site(b)
            );
            out.push(Stmt::Rule(RuleStmt {
                head: atom("deny", vec![str_term(&msg)], b.1),
                body: vec![Lit::Pos(held(name, i, a.1)), Lit::Pos(held(name, j, b.1))],
            }));
        }
    }
    out
}

/// The guarded groups of a body's `use`s and `instance`s: each name
/// bound more than once, its checks (`exclusive`).
fn exclusive_copies(stmts: &[Stmt]) -> Vec<Stmt> {
    let mut groups: BTreeMap<&str, Vec<Alternative>> = BTreeMap::new();
    for s in stmts {
        let (u, what) = match s {
            Stmt::Use(u) if u.module.rsplit('.').next() == Some(u.name.as_str()) => {
                (u, format!("use {}", u.module))
            }
            Stmt::Use(u) => (u, format!("use {} as {}", u.module, u.name)),
            Stmt::Instance(u) => (u, format!("resource {} {}", u.module, u.name)),
            _ => continue,
        };
        groups.entry(&u.name).or_default().push(Alternative {
            what,
            clause: u.clause.clone().unwrap_or_default(),
            span: u.span,
        });
    }
    groups
        .into_iter()
        .filter(|(_, alts)| alts.len() > 1)
        .flat_map(|(name, alts)| exclusive(name, &alts))
        .collect()
}

/// Predicates a module may never make private: the compiler's, the
/// provider's and the policy heads, and a provider's settings: a
/// provider's `use` in a module configures it for the deployment (R-129).
fn is_shared(pred: &str) -> bool {
    crate::loader::is_core_pred(pred)
        || pred == "provider_config"
        || pred == crate::plugin::providers::EXPECT_ACCOUNT
}

/// The relation a copy is recorded in (R-65): `instance_of(Path, User,
/// Name)`, the component's path, the scope that made the copy (`""` the
/// program) and the copy's name there, which `c[t]` enumerates: `c["n"]`
/// is the copy `n`.
pub const INSTANCE_OF: &str = "instance_of";

/// The rows a copy exports (R-55), `__rows(Scope, p, [x, ..])` for its
/// `output p`: what `copy.p(x, ..)` and `c[t].p(x, ..)` read.
pub const ROWS: &str = "__rows";

/// A scope the resolver wrote as its user's, `__scope(t)`: a read inside a
/// component of a copy the component does not make (`blue.vpc` of the
/// stack's `blue`). Expansion puts no copy's scope in front of it, and
/// takes the mark off once done.
pub const ABSOLUTE: &str = "__scope";

/// How predicate names are renamed in one copy or activation.
struct Names {
    /// Predicate name -> its private name (`n::p`; `n.inner::p` for the
    /// private `inner::p` of a copy inside it).
    map: BTreeMap<String, String>,
}

impl Names {
    fn get(&self, pred: &str) -> Option<&String> {
        self.map.get(pred)
    }

    /// Every predicate `defined` names but the shared ones, private to
    /// `scope`. `instance_of` is the program's: a copy's copies are told
    /// apart by their user's scope, its second column.
    fn private(scope: &str, defined: BTreeMap<String, (usize, Span)>) -> Names {
        let map = defined
            .into_keys()
            .filter(|p| !is_shared(p) && p != INSTANCE_OF && p != ROWS)
            .map(|p| {
                let n = private_name(scope, &p);
                (p, n)
            })
            .collect();
        Names { map }
    }
}

/// `p` private to `scope`: `scope::p`, or `scope.inner::p` for a name
/// already private to a copy inside it.
fn private_name(scope: &str, p: &str) -> String {
    match p.contains("::") {
        true => format!("{scope}.{p}"),
        false => format!("{scope}::{p}"),
    }
}

/// A program with its modules and components expanded, and the interface
/// the later passes check: every typed input, and every output cell
/// declared `secret(T)` (scope, key; `conn.password` for a field of an
/// object type).
pub struct Expanded {
    pub program: Program,
    pub inputs: Vec<Declared>,
    pub secret_outputs: Vec<(String, String)>,
    /// Every output declared with a type, by (scope, name).
    pub output_types: BTreeMap<(String, String), TypeExpr>,
}

/// The cells of an output its type declares secret: `k` for `secret(T)`,
/// `k.password` for an object type with a `password: secret(T)` field.
fn secret_paths(o: &OutputDecl) -> Vec<String> {
    o.ty.iter()
        .flat_map(crate::types::secret_fields)
        .map(|(p, _)| crate::types::dotted(&o.name, &p))
        .collect()
}

/// Every definition of the program, by path, wherever it stands.
fn definitions<'a>(stmts: &'a [Stmt], out: &mut BTreeMap<String, &'a crate::ast::Module>) {
    for s in stmts {
        if let Stmt::Module(m) = s {
            out.insert(m.name.clone(), m);
            definitions(&m.body, out);
        }
    }
}

/// The program's statements and those of every module a `use` imports and
/// every component a `resource` copies, each definition's once: where a
/// declaration of the deployment may stand (a provider's `use` and its
/// settings, R-129). A definition no statement reaches is left out.
pub fn reached(program: &Program) -> Vec<&Stmt> {
    let mut defs = BTreeMap::new();
    definitions(&program.statements, &mut defs);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&[Stmt]> = vec![&program.statements];
    while let Some(stmts) = todo.pop() {
        for s in stmts {
            if let Stmt::Use(u) | Stmt::Instance(u) = s
                && let Some(&m) = defs.get(u.module.as_str())
                && seen.insert(u.module.as_str())
            {
                todo.push(&m.body);
            }
            out.push(s);
        }
    }
    out
}

/// The expansion's state.
struct Cx<'a> {
    defs: BTreeMap<String, &'a crate::ast::Module>,
    diags: Vec<Diagnostic>,
    declared: Vec<Declared>,
    secret_outputs: Vec<(String, String)>,
    output_types: BTreeMap<(String, String), TypeExpr>,
    /// Private names by plain name, for the error when the program reads
    /// one.
    private: BTreeMap<String, (String, Option<String>)>,
    /// The definitions being expanded, outermost first: a component that
    /// reaches itself is an error, not a loop.
    expanding: Vec<String>,
    /// The definitions whose interface was checked.
    checked: BTreeSet<String>,
    /// The scopes the stack reaches by `use` alone, `""` first: their
    /// inputs are the stack's to give (R-55).
    flat: BTreeSet<String>,
}

fn check_types(who: &str, inputs: &[InputDecl], diags: &mut Vec<Diagnostic>) {
    for i in inputs.iter().flat_map(crate::inputs::leaves) {
        let i = &i;
        if let Err(e) = crate::inputs::check_type(&i.ty) {
            diags.push(Diagnostic::error(
                i.span,
                format!("{who} input {}: {e}", i.name),
            ));
        }
        let (_, rest) = crate::refine::split_input(i);
        diags.extend(crate::refine::check_rest(&rest, i.span));
    }
}

/// `scope.rest`, the scope `""` being the program's.
fn join_scope(scope: &str, rest: &str) -> String {
    match (scope.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => scope.to_string(),
        _ => format!("{scope}.{rest}"),
    }
}

pub fn expand(program: &Program) -> Result<Expanded> {
    let mut defs = BTreeMap::new();
    definitions(&program.statements, &mut defs);
    let mut cx = Cx {
        defs,
        diags: Vec::new(),
        declared: Vec::new(),
        secret_outputs: Vec::new(),
        output_types: BTreeMap::new(),
        private: BTreeMap::new(),
        expanding: Vec::new(),
        checked: BTreeSet::new(),
        flat: BTreeSet::from([String::new()]),
    };
    let mut out = exclusive_copies(&program.statements);
    for s in &program.statements {
        match s {
            // The stack's own input: read as `k(V)`, given by `--set` (an
            // `input(k, V)` fact) at the normal rank.
            Stmt::Input(i) => {
                check_types("stack", std::slice::from_ref(i), &mut cx.diags);
                out.extend(input_reader("", i, &|p: &str| p.to_string()));
                out.extend(given_rules("", "", i));
                for leaf in crate::inputs::leaves(i) {
                    out.extend(refinement(&leaf, ""));
                    let address = Some(leaf.name.clone());
                    cx.declared.push(Declared::new("", leaf, address, false));
                }
            }
            // The stack's own relation, exported: published as its rows.
            Stmt::Output(o) if o.relation.is_some() => out.push(published_rows(o)),
            // The stack's own output: `output(k, V)` in the root scope.
            Stmt::Output(o) if o.value.is_none() => {
                cx.secret_outputs
                    .extend(secret_paths(o).into_iter().map(|k| (String::new(), k)));
                if let Some(t) = &o.ty {
                    cx.output_types
                        .insert((String::new(), o.name.clone()), t.clone());
                }
            }
            Stmt::Output(o) => out.push(fact_or_rule(
                atom(
                    "output",
                    vec![str_term(&o.name), o.value.clone().unwrap()],
                    o.span,
                ),
                Vec::new(),
            )),
            Stmt::Use(u) => out.extend(cx.instance(u, "", true)),
            Stmt::Instance(u) => out.extend(cx.instance(u, "", false)),
            Stmt::Module(_) => {}
            other => out.push(other.clone()),
        }
    }
    // The program's own `let`s.
    let mut expanded = lets(out, "", None);
    // `instance_of` (and a copy's private one) is a fact for a copy with
    // no clause and a rule for one with: both, by design.
    let mut mixed = BTreeMap::new();
    for s in &expanded {
        if let Stmt::Fact(a) | Stmt::Rule(RuleStmt { head: a, .. }) = s
            && a.pred == INSTANCE_OF
        {
            mixed.entry(a.pred.clone()).or_insert(a.span);
        }
    }
    expanded.extend(mixed.into_iter().map(|(pred, span)| {
        Stmt::Mixed(crate::ast::Extern {
            pred,
            arity: 3,
            span,
        })
    }));

    // A read of a name only a module or a component defines: say it is
    // private.
    let mut defined = BTreeMap::new();
    defined_preds(&expanded, &mut defined);
    let externs: BTreeSet<&str> = expanded
        .iter()
        .filter_map(|s| match s {
            Stmt::Extern(e) => Some(e.pred.as_str()),
            _ => None,
        })
        .collect();
    let mut reads = Vec::new();
    for s in &expanded {
        body_atoms(s, &mut reads);
    }
    for a in reads {
        if let Some((owner, help)) = cx.private.get(&a.pred)
            && !defined.contains_key(&a.pred)
            && !externs.contains(a.pred.as_str())
        {
            let d = Diagnostic::error(
                a.span,
                format!("{}/{} is private to {owner}", a.pred, a.args.len()),
            );
            cx.diags.push(match help {
                Some(h) => d.with_help(h.clone()),
                None => d,
            });
        }
    }

    // `set k = t where B`, alone or in a block, gives an input the stack
    // addresses (R-54, R-38): its own, a field of an object one, a used
    // module's (its cell `(input, m, k)`).
    let mut paths = BTreeSet::new();
    let mut flat = BTreeSet::new();
    for d in &cx.declared {
        let Some(a) = &d.address else { continue };
        let base = &a[..a.len() - d.decl.name.len()];
        paths.insert(a.clone());
        flat.insert(d.scope.clone());
        for (i, _) in d.decl.name.match_indices('.') {
            paths.insert(format!("{base}{}", &d.decl.name[..i]));
        }
    }
    let mut heads = Vec::new();
    for s in &expanded {
        head_rules(s, &mut heads);
    }
    for (h, body) in heads {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(scope)),
            Term::Val(Value::Str(k)),
            ..,
        ] = h.args.as_slice()
        else {
            continue;
        };
        if h.pred != "arg" || t != INPUT || !(scope.is_empty() || flat.contains(scope)) {
            continue;
        }
        // `--set` (`input(k, V)`) is checked where it is given.
        if matches!(body, [Lit::Pos(a)] if a.pred == INPUT) {
            continue;
        }
        let address = join_scope(scope, k);
        let what = format!("`set {address}`");
        if !paths.is_empty() && !paths.contains(&address) {
            let msg = match address.rsplit_once('.').filter(|(o, _)| paths.contains(*o)) {
                Some((o, f)) => format!("{what}: input {o} has no field {f}"),
                None => format!("{what}: the program declares no input {address}"),
            };
            cx.diags.push(Diagnostic::error(h.span, msg));
            continue;
        }
        let under =
            |a: &str, b: &str| a == b || a.strip_prefix(b).is_some_and(|r| r.starts_with('.'));
        for d in cx.declared.iter_mut() {
            if d.scope != *scope || !(under(&d.decl.name, k) || under(k, &d.decl.name)) {
                continue;
            }
            // The declaration's own default is no giving.
            let rank = h.args.get(4);
            if h.span == d.decl.span
                && matches!(rank, Some(Term::Val(Value::Str(r))) if r == "default")
            {
                continue;
            }
            if d.decl.key {
                cx.diags.push(
                    Diagnostic::error(h.span, format!("{what}: {address} is a key")).with_help(
                        format!(
                            "a key is given by the target, `STACK {}=..`, and names the deployment",
                            d.decl.name
                        ),
                    ),
                );
                break;
            }
            d.given = true;
        }
    }

    // What a `set`, a `use` block or a copy gives a typed input is read as
    // its type (R-31, R-134): a literal now, a computed value at run time.
    for s in &mut expanded {
        if let Stmt::Fact(h) | Stmt::Rule(RuleStmt { head: h, .. }) = s {
            read_input(h, &cx.declared);
        }
    }
    let expanded = expanded.into_iter().map(unmark_stmt).collect::<Vec<_>>();
    if cx.diags.is_empty() {
        Ok(Expanded {
            program: Program {
                statements: expanded,
                stack: program.stack.clone(),
            },
            inputs: cx.declared,
            secret_outputs: cx.secret_outputs,
            output_types: cx.output_types,
        })
    } else {
        Err(Diagnostics(cx.diags).into())
    }
}

/// `arg(input, scope, k, V, ..)` with `V` read as the declared type of
/// the input `k` of `scope`, or of the fields of `V` the inputs under `k`
/// declare (`gke = { subnet_cidr: "10.0.0.0/22" }`).
fn read_input(h: &mut Atom, declared: &[Declared]) {
    if h.pred != "arg" || h.args.len() < 4 {
        return;
    }
    let (Term::Val(Value::Str(t)), Term::Val(Value::Str(scope)), Term::Val(Value::Str(k))) =
        (&h.args[0], &h.args[1], &h.args[2])
    else {
        return;
    };
    if t != INPUT {
        return;
    }
    let (scope, k) = (scope.clone(), k.clone());
    for d in declared.iter().filter(|d| d.scope == scope) {
        let rest = match d.decl.name.strip_prefix(k.as_str()) {
            Some("") => "",
            Some(r) if r.starts_with('.') => &r[1..],
            _ => continue,
        };
        read_typed_at(&mut h.args[3], rest, &d.decl.ty);
    }
}

/// The term at `path` inside `t` read as `ty` ([`read_input`]).
fn read_typed_at(t: &mut Term, path: &str, ty: &TypeExpr) {
    if path.is_empty() {
        match t {
            Term::Val(v) => *t = Term::Val(crate::inputs::coerce(ty, v.clone())),
            Term::Var(_) | Term::Func { .. } => {
                let v = std::mem::replace(t, Term::Wildcard);
                *t = crate::types::at_run_time(&crate::types::of_expr(ty), v);
            }
            Term::List(xs) => {
                if let TypeExpr::Apply(n, inner) = ty
                    && (n == "list" || n == "set")
                    && let [inner] = inner.as_slice()
                {
                    xs.iter_mut().for_each(|x| read_typed_at(x, "", inner));
                }
            }
            Term::Obj(m) => {
                if let TypeExpr::Object(fs) = ty {
                    for (k, ft) in fs {
                        if let Some(x) = m.get_mut(k) {
                            read_typed_at(x, "", ft);
                        }
                    }
                }
            }
            _ => {}
        }
        return;
    }
    let (seg, rest) = path.split_once('.').unwrap_or((path, ""));
    match t {
        Term::Obj(m) => {
            if let Some(x) = m.get_mut(seg) {
                read_typed_at(x, rest, ty);
            }
        }
        Term::Val(Value::Obj(m)) => {
            if let Some(x) = m.get_mut(seg) {
                let mut held = Term::Val(std::mem::replace(x, Value::Bool(false)));
                read_typed_at(&mut held, rest, ty);
                *x = match held {
                    Term::Val(v) => v,
                    _ => Value::Bool(false),
                };
            }
        }
        _ => {}
    }
}

impl Cx<'_> {
    /// The `use`s and `instance`s of a body expanded, every name relative
    /// to the body's scope; `at` is that scope's absolute name.
    fn body(&mut self, stmts: &[Stmt], at: &str) -> Vec<Stmt> {
        let mut out = exclusive_copies(stmts);
        for s in stmts {
            match s {
                Stmt::Use(u) => out.extend(self.instance(u, at, true)),
                Stmt::Instance(u) => out.extend(self.instance(u, at, false)),
                other => out.push(other.clone()),
            }
        }
        out
    }

    /// `def`, or `None` (reported) when it is being expanded already.
    fn enter(&mut self, path: &str, span: Span) -> Option<&crate::ast::Module> {
        let def = *self.defs.get(path)?;
        if self.expanding.iter().any(|p| p == path) {
            let mut cycle = self.expanding.clone();
            cycle.push(path.to_string());
            self.diags.push(Diagnostic::error(
                span,
                format!("{path} reaches itself: {}", cycle.join(" -> ")),
            ));
            return None;
        }
        self.expanding.push(path.to_string());
        Some(def)
    }

    /// `resource c n { k = V } where B`, a component's body under the
    /// scope `n`, recorded in `instance_of`; or `use m [as n] { k = V }
    /// where B`, a module's (`used`): one mechanism (R-65).
    fn instance(&mut self, u: &crate::ast::Instance, at: &str, used: bool) -> Vec<Stmt> {
        let Some(def) = self.enter(&u.module, u.span) else {
            return Vec::new();
        };
        let kind = if used { "module" } else { "component" };
        let (iface, body) = interface(&def.body);
        if self.checked.insert(u.module.clone()) {
            check_component(kind, &u.module, &iface, &body, &mut self.diags);
            check_types(
                &format!("{kind} {}", u.module),
                &iface.inputs,
                &mut self.diags,
            );
        }
        let scope = u.name.as_str();
        let abs = join_scope(at, scope);
        let mut out = Vec::new();
        // A used module's inputs are the stack's to give, `m.k` (R-55); a
        // copy's are its instance block's.
        let flat = used && self.flat.contains(at);
        if flat {
            self.flat.insert(abs.clone());
        }
        instance_inputs(kind, u, scope, &iface, flat, &mut out, &mut self.diags);
        let body = self.body(&body, &abs);
        self.expanding.pop();
        let stmts = module_stmts(scope, &iface, body);
        let mut defined = BTreeMap::new();
        defined_preds(&stmts, &mut defined);
        for i in &iface.inputs {
            defined.insert(i.name.clone(), (1, i.span));
            for l in crate::inputs::leaves(i) {
                defined.insert(refine_pred(&l.name), (1, i.span));
            }
        }
        // A relation the copy takes is its own, its rows its user's.
        for e in &iface.relations {
            defined.insert(e.pred.clone(), (e.arity, e.span));
        }
        out.extend(instance_rows(u, scope, &iface));
        let names = Names::private(scope, defined);
        for p in names.map.keys() {
            let help = match used {
                true => format!("read it as {scope}.{p}, after `use {}`", u.module),
                false => format!(
                    "pass the value through an output of component {}, `output {p} = ...`, \
                     and read it as INSTANCE.{p}",
                    u.module
                ),
            };
            self.private
                .insert(p.clone(), (format!("{kind} {}", u.module), Some(help)));
        }
        let mut stmts = stmts;
        let origin = match used {
            true if u.module.rsplit('.').next() == Some(scope) => format!("use {}", u.module),
            true => format!("use {} as {scope}", u.module),
            false => format!("resource {} {}", u.module, u.name),
        };
        set_origin(&mut stmts, diag::origin_id(&origin));
        let stmts = stmts
            .into_iter()
            .map(|st| {
                let sc = Sc {
                    name: scope,
                    vars: !used,
                };
                rewrite_stmt(rename_stmt(st, &names), sc)
            })
            .collect();
        let mut copy = lets(stmts, scope, Some(&names));
        copy.extend(input_readers(scope, &iface.inputs, &names));
        for o in iface.outputs.values() {
            self.secret_outputs
                .extend(secret_paths(o).into_iter().map(|k| (abs.clone(), k)));
            if let Some(t) = &o.ty {
                self.output_types
                    .insert((abs.clone(), o.name.clone()), t.clone());
            }
        }
        if flat {
            for i in &iface.inputs {
                out.extend(given_rules(scope, &abs, i));
            }
        }
        for i in &iface.inputs {
            for leaf in crate::inputs::leaves(i) {
                let address = flat.then(|| join_scope(&abs, &leaf.name));
                let bound = u.inputs.iter().any(|(k, _, _)| {
                    *k == leaf.name
                        || leaf
                            .name
                            .strip_prefix(k.as_str())
                            .is_some_and(|r| r.starts_with('.'))
                });
                // A name declared twice (R-104) declares its inputs once.
                if self
                    .declared
                    .iter()
                    .any(|d| d.scope == abs && d.decl.name == leaf.name)
                {
                    continue;
                }
                let mut d = Declared::new(&abs, leaf, address, bound);
                d.used_at = flat.then_some(u.span);
                self.declared.push(d);
            }
        }
        out.extend(gate(
            copy,
            &u.module,
            scope,
            u.clause.as_deref(),
            !used,
            u.span,
        ));
        out
    }
}

/// The statements of the copy or activation `scope`, and for a copy
/// (`record`) the fact `instance_of(path, scope)`. With a clause `B` they
/// exist only while it holds: the copy's own `scope::__instance(path) :-
/// B` gates each of its rules and resources, and `instance_of(path,
/// scope)` is derived from it. The gate is the copy's own relation, so
/// that one copy's clause reading another's outputs is no cycle through
/// every copy.
fn gate(
    stmts: Vec<Stmt>,
    path: &str,
    scope: &str,
    clause: Option<&[Lit]>,
    record: bool,
    span: Span,
) -> Vec<Stmt> {
    let fact = atom(
        INSTANCE_OF,
        vec![str_term(path), str_term(""), str_term(scope)],
        span,
    );
    let Some(b) = clause.filter(|b| !b.is_empty()) else {
        let mut out = stmts;
        if record {
            out.push(Stmt::Fact(fact));
        }
        return out;
    };
    let own = atom(&format!("{scope}::{GATE}"), vec![str_term(path)], span);
    let on = Lit::Pos(own.clone());
    let mut out: Vec<Stmt> = stmts
        .into_iter()
        .map(|s| match s {
            Stmt::Fact(a) => Stmt::Rule(RuleStmt {
                head: a,
                body: vec![on.clone()],
            }),
            Stmt::Rule(mut r) => {
                r.body.insert(0, on.clone());
                Stmt::Rule(r)
            }
            Stmt::Resource(mut r) => {
                r.body.get_or_insert_with(Vec::new).insert(0, on.clone());
                r.reads = r.reads.start + 1..r.reads.end + 1;
                Stmt::Resource(r)
            }
            other => other,
        })
        .collect();
    out.push(Stmt::Rule(RuleStmt {
        head: own,
        body: b.to_vec(),
    }));
    if record {
        out.push(Stmt::Rule(RuleStmt {
            head: fact,
            body: vec![on],
        }));
    }
    out
}

/// A gated copy's own relation (`gate`): `n::__instance(path)` while the
/// copy `n` of the component at `path` exists.
pub const GATE: &str = "__instance";

/// A fact of a predicate private to a copy or an activation (`n::p`,
/// `n.inner::p`, a name no source can spell) as the program names it: a
/// gated copy's own relation as the statement that makes it, `resource
/// PATH n` (`(in USER)` for a copy inside another), anything else by its
/// name there, `p(..) (in n)`; `fmt` prints that atom and `gap` comes
/// before the parenthesis. `None` for a predicate of the program's.
pub fn private_text(a: &Atom, fmt: &dyn Fn(&Atom) -> String, gap: &str) -> Option<String> {
    let (scope, p) = a.pred.rsplit_once("::")?;
    if p == GATE
        && let Some(Term::Val(Value::Str(path))) = a.args.first()
    {
        return Some(match scope.rsplit_once('.') {
            None => format!("resource {path} {scope}"),
            Some((user, name)) => format!("resource {path} {name}{gap}(in {user})"),
        });
    }
    let own = Atom {
        pred: p.to_string(),
        ..a.clone()
    };
    Some(format!("{}{gap}(in {scope})", fmt(&own)))
}

/// The instance's `k = V :- B`, each a normal-rank contribution to its
/// input cell, with the checks against the component's declared inputs.
/// An entry may give an object input's leaf (`nodes.count = 2`) or a whole
/// object, which gives each leaf the field it has (R-54). An input nothing
/// gives is an error here, but a used module's (`flat`): the stack may
/// give that one with `--set m.k=v` (`inputs::check_required`).
fn instance_inputs(
    kind: &str,
    u: &crate::ast::Instance,
    scope: &str,
    iface: &Interface,
    flat: bool,
    out: &mut Vec<Stmt>,
    diags: &mut Vec<Diagnostic>,
) {
    let leaves: Vec<InputDecl> = iface
        .inputs
        .iter()
        .flat_map(crate::inputs::leaves)
        .collect();
    let covers =
        |k: &str, leaf: &str| leaf == k || leaf.strip_prefix(k).is_some_and(|r| r.starts_with('.'));
    let body = u.body.clone().unwrap_or_default();
    for (k, v, span) in &u.inputs {
        if !leaves.iter().any(|l| covers(k, &l.name)) {
            let declared: Vec<&str> = iface.inputs.iter().map(|i| i.name.as_str()).collect();
            let object = k
                .match_indices('.')
                .map(|(n, _)| &k[..n])
                .rfind(|o| leaves.iter().any(|l| l.name.starts_with(&format!("{o}."))));
            let d = match object {
                Some(o) => {
                    let fields: Vec<&str> = leaves
                        .iter()
                        .filter_map(|l| l.name.strip_prefix(o)?.strip_prefix('.'))
                        .collect();
                    Diagnostic::error(
                        *span,
                        format!(
                            "input {o} of {kind} {} has no field {}",
                            u.module,
                            &k[o.len() + 1..]
                        ),
                    )
                    .with_note(format!("its fields: {}", fields.join(", ")))
                }
                None => {
                    let d =
                        Diagnostic::error(*span, format!("{kind} {} has no input {k}", u.module));
                    if declared.is_empty() {
                        d.with_note(format!("{kind} {} declares no inputs", u.module))
                    } else {
                        d.with_note(format!("its inputs: {}", declared.join(", ")))
                    }
                }
            };
            diags.push(d);
            continue;
        }
        // The entry's type: the leaf's, or the object's it names.
        let ty = iface
            .inputs
            .iter()
            .find(|i| covers(&i.name, k))
            .and_then(|i| {
                k[i.name.len()..]
                    .split('.')
                    .skip(1)
                    .try_fold(&i.ty, |t, seg| match t {
                        TypeExpr::Object(fs) => fs.iter().find(|(f, _)| f == seg).map(|(_, t)| t),
                        _ => None,
                    })
            });
        // A literal is read as the input's declared type (R-31).
        let v = match ty.map(|t| crate::types::literal(&crate::types::of_expr(t), v.clone())) {
            Some(Ok(v)) => v,
            Some(Err(why)) => {
                diags.push(Diagnostic::error(
                    *span,
                    format!("input {k} of {kind} {} {why}", u.module),
                ));
                continue;
            }
            None => v.clone(),
        };
        let head = atom(
            "arg",
            vec![
                str_term(INPUT),
                str_term(scope),
                str_term(k),
                v,
                str_term(crate::transform::NORMAL),
            ],
            *span,
        );
        out.push(fact_or_rule(head, body.clone()));
    }
    // An input nothing gives a value is a stack input's error (R-65),
    // fixed the same ways: a default, or a value given.
    if flat {
        return;
    }
    for i in &leaves {
        // A dependent input is required where its clause holds: a deny.
        if i.default.is_none()
            && i.guard.is_empty()
            && !u.inputs.iter().any(|(k, _, _)| covers(k, &i.name))
        {
            diags.push(
                Diagnostic::error(
                    u.span,
                    format!(
                        "input {} is required and has no value",
                        join_scope(scope, &i.name)
                    ),
                )
                .with_label(
                    i.span,
                    format!(
                        "{}: {} declared here",
                        i.name,
                        crate::inputs::type_text(&i.ty)
                    ),
                )
                .with_help(format!(
                    "give it in the block, `{{ {} = ... }}`, or give it a default",
                    i.name
                )),
            );
        }
    }
}

/// The rows a `use` or `instance` block gives the relations its module
/// takes (R-55), written in the user's scope: each head is the copy's own
/// relation, `scope::p`, and with a clause each row holds only while it
/// does, as the copy does.
fn instance_rows(u: &crate::ast::Instance, scope: &str, iface: &Interface) -> Vec<Stmt> {
    let takes: BTreeSet<&str> = iface.relations.iter().map(|e| e.pred.as_str()).collect();
    let clause = u.clause.clone().unwrap_or_default();
    // Rows written as facts and as rules are one relation.
    let mixed = iface.relations.iter().map(|e| {
        Stmt::Mixed(crate::ast::Extern {
            pred: private_name(scope, &e.pred),
            ..e.clone()
        })
    });
    u.rows
        .iter()
        .cloned()
        .map(|st| match st {
            Stmt::Fact(mut a) if takes.contains(a.pred.as_str()) => {
                a.pred = private_name(scope, &a.pred);
                fact_or_rule(a, clause.clone())
            }
            Stmt::Rule(mut r) if takes.contains(r.head.pred.as_str()) => {
                r.head.pred = private_name(scope, &r.head.pred);
                r.body.splice(0..0, clause.iter().cloned());
                Stmt::Rule(r)
            }
            other => other,
        })
        .chain(mixed)
        .collect()
}

/// The checks on a component's interface, once per component: an input
/// or an output is declared once, an input is not also defined, and every
/// output it gives a value is declared.
fn check_component(
    kind: &str,
    path: &str,
    iface: &Interface,
    body: &[Stmt],
    diags: &mut Vec<Diagnostic>,
) {
    let mut seen = BTreeSet::new();
    for i in &iface.inputs {
        // Several declarations, each under a clause (R-104).
        if !seen.insert(&i.name) && i.guard.is_empty() {
            diags.push(Diagnostic::error(
                i.span,
                format!("{kind} {path} declares input {} twice", i.name),
            ));
        }
    }
    let mut own = BTreeMap::new();
    defined_preds(body, &mut own);
    for i in &iface.inputs {
        if let Some((_, span)) = own.get(&i.name) {
            diags.push(Diagnostic::error(
                *span,
                format!(
                    "{kind} {path} defines {}, which is its input: an input is set where it \
                     is used",
                    i.name
                ),
            ));
        }
    }
    let undeclared = |k: &str, span: Span| {
        Diagnostic::error(span, format!("{kind} {path} has no output {k}"))
            .with_help(format!("declare it: `output {k}: TYPE`"))
    };
    for o in &iface.output_values {
        if !iface.outputs.contains_key(&o.name) {
            diags.push(undeclared(&o.name, o.span));
        }
    }
    // `output(k, V)` written as a rule must name a declared output too.
    let mut heads = Vec::new();
    for s in body {
        head_atoms(s, &mut heads);
    }
    for h in heads {
        if h.pred == "output"
            && h.args.len() == 2
            && let Term::Val(Value::Str(k)) = &h.args[0]
            && !iface.outputs.contains_key(k)
        {
            diags.push(undeclared(k, h.span));
        }
    }
}

/// The component's statements for one copy, before scoping: its body,
/// the output values and each input's refinement.
fn module_stmts(scope: &str, iface: &Interface, body: Vec<Stmt>) -> Vec<Stmt> {
    let mut out = body;
    for o in &iface.output_values {
        // An undeclared output is reported once, by `check_component`.
        let Some(decl) = iface.outputs.get(&o.name) else {
            continue;
        };
        let mut value = o.value.clone().expect("an output value");
        // An `addr` output names one of this copy's resources: scoped by
        // the copy itself (`""`), which scoping makes its own name.
        if matches!(&decl.ty, Some(TypeExpr::Name(t)) if t == "addr") {
            value = Term::Func {
                name: crate::ir::SCOPED.into(),
                args: vec![str_term(""), value],
            };
        }
        out.push(fact_or_rule(
            atom("output", vec![str_term(&o.name), value], o.span),
            Vec::new(),
        ));
    }
    for i in iface.inputs.iter().flat_map(crate::inputs::leaves) {
        out.extend(refinement(&i, scope));
    }
    out.extend(iface.relation_outputs.iter().map(rows_of));
    out
}

/// `output p` of a copy: its rows `__rows(p, [X1, ..]) :- p(X1, ..)`, the
/// copy's scope put in front by scoping; a column of the copy's resources
/// is their addresses, `scoped("", X)`, as an `addr` output's is.
fn rows_of(o: &OutputDecl) -> Stmt {
    let refs = o.relation.clone().unwrap_or_default();
    let vars: Vec<Term> = (0..refs.len())
        .map(|i| Term::Var(format!("X{i}")))
        .collect();
    let row = vars
        .iter()
        .zip(&refs)
        .map(|(v, r)| match r {
            true => Term::Func {
                name: crate::ir::SCOPED.into(),
                args: vec![str_term(""), v.clone()],
            },
            false => v.clone(),
        })
        .collect();
    Stmt::Rule(RuleStmt {
        head: atom(ROWS, vec![str_term(&o.name), Term::List(row)], o.span),
        body: vec![Lit::Pos(atom(&o.name, vars, o.span))],
    })
}

/// `output p` of a stack: the value it publishes, its rows as a list of
/// rows, `output(p, [[X1, ..] | p(X1, ..)])`, which a reader's
/// `stack[k=v].p(x, ..)` ranges over.
fn published_rows(o: &OutputDecl) -> Stmt {
    let vars: Vec<Term> = (0..o.relation.as_ref().map_or(0, Vec::len))
        .map(|i| Term::Var(format!("X{i}")))
        .collect();
    fact_or_rule(
        atom(
            "output",
            vec![
                str_term(&o.name),
                Term::ListComp {
                    item: Box::new(Term::List(vars.clone())),
                    body: vec![Lit::Pos(atom(&o.name, vars, o.span))],
                },
            ],
            o.span,
        ),
        Vec::new(),
    )
}

fn refine_pred(input: &str) -> String {
    format!("__refine_{input}")
}

/// `input k: T where R`: what of `R` fits the checkable table is a
/// refinement of the input's cell (`input_reader`, `crate::refine`); the
/// rest is a deny unless it holds of the input's value: `__refine_k(V) :-
/// k(V), R[k := V]` and `deny(...) :- k(V), not __refine_k(V)`. The input
/// is written by its name in `R`.
pub fn refinement(i: &InputDecl, scope: &str) -> Vec<Stmt> {
    let (_, rest) = crate::refine::split_input(i);
    if rest.is_empty() {
        return Vec::new();
    }
    let v = Term::Var("__Input".into());
    // A leaf of an object input is read in its object (R-54).
    let read = match i.name.split_once('.') {
        None => vec![Lit::Pos(atom(&i.name, vec![v.clone()], i.span))],
        Some((top, rest)) => {
            let o = Term::Var("__Object".into());
            vec![
                Lit::Pos(atom(top, vec![o.clone()], i.span)),
                Lit::Eq(
                    v.clone(),
                    Term::Func {
                        name: "__path".into(),
                        args: vec![o, str_term(rest)],
                    },
                ),
            ]
        }
    };
    let mut body = read.clone();
    body.extend(rest.iter().map(|l| subst_lit(l, &i.name, &v)));
    let ok = atom(&refine_pred(&i.name), vec![v.clone()], i.span);
    let named = Term::Var(i.name.clone());
    let text = rest
        .iter()
        .map(|l| crate::partition::fmt_lit(&subst_lit(l, &i.name, &named)))
        .collect::<Vec<_>>()
        .join(", ");
    let who = if scope.is_empty() {
        format!("input {}", i.name)
    } else {
        format!("input {} of {scope}", i.name)
    };
    // A secret input's value is not printed.
    let ctx = if matches!(&i.ty, TypeExpr::Apply(n, _) if n == "secret") {
        BTreeMap::new()
    } else {
        BTreeMap::from([("value".to_string(), v.clone())])
    };
    let deny = atom(
        "deny",
        vec![
            str_term(&format!("{who} fails its refinement: {text}")),
            Term::Obj(ctx),
        ],
        i.span,
    );
    vec![
        Stmt::Rule(RuleStmt {
            head: ok.clone(),
            body,
        }),
        Stmt::Rule(RuleStmt {
            head: deny,
            body: read.into_iter().chain([Lit::Not(ok)]).collect(),
        }),
    ]
}

/// `l` with the name `name` (a refinement's text) read as `v`.
pub(crate) fn subst_lit(l: &Lit, name: &str, v: &Term) -> Lit {
    let t = |x: &Term| subst_term(x, name, v);
    let a = |x: &Atom| Atom {
        args: x.args.iter().map(t).collect(),
        ..x.clone()
    };
    match l {
        Lit::Pos(x) => Lit::Pos(a(x)),
        Lit::Not(x) => Lit::Not(a(x)),
        Lit::Eq(x, y) => Lit::Eq(t(x), t(y)),
        Lit::Neq(x, y) => Lit::Neq(t(x), t(y)),
        Lit::Gt(x, y) => Lit::Gt(t(x), t(y)),
        Lit::Ge(x, y) => Lit::Ge(t(x), t(y)),
        Lit::Lt(x, y) => Lit::Lt(t(x), t(y)),
        Lit::Le(x, y) => Lit::Le(t(x), t(y)),
    }
}

fn subst_term(t: &Term, name: &str, v: &Term) -> Term {
    match t {
        Term::Val(Value::Str(s)) if s == name => v.clone(),
        // A field of the value, `net.bits` (R-134): renamed with it, read
        // off it, or where the name prints as written, as written.
        Term::Val(Value::Str(s)) if s.strip_prefix(name).is_some_and(|r| r.starts_with('.')) => {
            let field = &s[name.len() + 1..];
            match v {
                Term::Val(Value::Str(n)) => Term::Val(Value::Str(format!("{n}.{field}"))),
                Term::Var(n) if n == name => Term::Var(s.clone()),
                v => Term::Func {
                    name: "__path".into(),
                    args: vec![v.clone(), Term::Val(Value::Str(field.to_string()))],
                },
            }
        }
        Term::Func { name: f, args } => Term::Func {
            name: f.clone(),
            args: args.iter().map(|a| subst_term(a, name, v)).collect(),
        },
        Term::List(xs) => Term::List(xs.iter().map(|a| subst_term(a, name, v)).collect()),
        Term::Obj(m) => Term::Obj(
            m.iter()
                .map(|(k, a)| (k.clone(), subst_term(a, name, v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `k(V) :- attr(input, Scope, k, V)` per input, and its `@default`
/// contribution. Written after scoping: the scope is already in the terms.
fn input_readers(scope: &str, inputs: &[InputDecl], names: &Names) -> Vec<Stmt> {
    let pred = |p: &str| names.get(p).cloned().unwrap_or_else(|| p.to_string());
    inputs
        .iter()
        .flat_map(|i| {
            // A dependent input's clause reads the copy's own names.
            let mut i = i.clone();
            i.guard = i.guard.into_iter().map(|l| rename_lit(l, names)).collect();
            input_reader(scope, &i, &pred)
        })
        .collect()
}

/// One input's reader, named by `pred` of its name, `k(V) :- attr(input,
/// Scope, k, V)`, and its `@default` contributions and checkable
/// refinements. An object input (R-54) is one cell per leaf: each leaf's
/// default and check is the leaf's (`arg(input, Scope, "nodes.count", 1,
/// @default)`, which the aggregate merges into the object `nodes` leaf by
/// leaf), and the object is read whole.
pub fn input_reader(scope: &str, i: &InputDecl, pred: &dyn Fn(&str) -> String) -> Vec<Stmt> {
    let v = Term::Var("V".into());
    // A dependent input (R-104) is read, and defaults, where its clause
    // holds; where it holds with no value, it is required.
    let mut body = vec![Lit::Pos(atom(
        "attr",
        vec![
            str_term(INPUT),
            str_term(scope),
            str_term(&i.name),
            v.clone(),
        ],
        i.span,
    ))];
    body.extend(i.guard.iter().cloned());
    let mut out = vec![Stmt::Rule(RuleStmt {
        head: atom(&pred(&i.name), vec![v.clone()], i.span),
        body,
    })];
    if !i.guard.is_empty() && i.default.is_none() && i.fields.is_empty() {
        let who = match scope {
            "" => format!("input {}", i.name),
            s => format!("input {} of {s}", i.name),
        };
        let mut body = i.guard.clone();
        body.push(Lit::Not(atom(&pred(&i.name), vec![Term::Wildcard], i.span)));
        out.push(Stmt::Rule(RuleStmt {
            head: atom(
                "deny",
                vec![str_term(&format!(
                    "{who} is required and has no value: its clause holds in this deployment"
                ))],
                i.span,
            ),
            body,
        }));
    }
    for l in crate::inputs::leaves(i) {
        let (checkable, _) = crate::refine::split_input(&l);
        for c in checkable {
            out.push(crate::refine::refine_fact(
                INPUT,
                Some(scope),
                &l.name,
                &c,
                l.span,
            ));
        }
        if let Some(d) = &l.default {
            out.push(fact_or_rule(
                atom(
                    "arg",
                    vec![
                        str_term(INPUT),
                        str_term(scope),
                        str_term(&l.name),
                        d.clone(),
                        str_term(crate::ast::Rank::Default.name()),
                    ],
                    l.span,
                ),
                i.guard.clone(),
            ));
        }
    }
    out
}

/// The paths an input is given by: the input, each object an object
/// input holds and each leaf (`nodes`, `nodes.pool`, `nodes.pool.size`).
pub fn input_paths(i: &InputDecl) -> Vec<String> {
    let mut out = vec![i.name.clone()];
    for l in crate::inputs::leaves(i) {
        let mut at = i.name.clone();
        for seg in l.name[i.name.len()..].split('.').skip(1) {
            at = format!("{at}.{seg}");
            if !out.contains(&at) {
                out.push(at.clone());
            }
        }
    }
    out
}

/// The values the outside gives an input the stack addresses as
/// `address` (R-54, R-55): `--set k=v`, an `input(k, V)` fact, is an
/// `@override` contribution to `k`, or to the object or leaf inside it the
/// fact names (`input("nodes.count", 2)`), so it wins over the default and
/// over every `set` of the program (R-38). A key's value is the target's, the
/// normal rank: nothing else gives a key.
fn given_rules(scope: &str, address: &str, i: &InputDecl) -> Vec<Stmt> {
    let leaves = crate::inputs::leaves(i);
    let rank = match i.key {
        true => crate::transform::NORMAL,
        false => crate::ast::Rank::Override.name(),
    };
    let mut out = Vec::new();
    for p in input_paths(i) {
        // A leaf's at its field, an object's at the input.
        let leaf = leaves.iter().find(|l| l.name == p);
        let span = leaf.map_or(i.span, |l| l.span);
        let v = Term::Var("V".into());
        let head = |v: Term| {
            atom(
                "arg",
                vec![
                    str_term(INPUT),
                    str_term(scope),
                    str_term(&p),
                    v,
                    str_term(rank),
                ],
                span,
            )
        };
        out.push(Stmt::Rule(RuleStmt {
            head: head(v.clone()),
            body: vec![Lit::Pos(atom(
                "input",
                vec![str_term(&join_scope(address, &p)), v.clone()],
                span,
            ))],
        }));
        // A map's key, `input("labels.team", V)`: the entry `{team: V}`.
        if leaf.is_some_and(|l| crate::inputs::is_map(&l.ty)) {
            let k = Term::Var("K".into());
            let mut body = vec![Lit::Pos(atom("input", vec![k.clone(), v.clone()], span))];
            let prefix = join_scope(address, &p);
            body.extend(crate::inputs::map_entry_lits(&k, &prefix, v, "E"));
            out.push(Stmt::Rule(RuleStmt {
                head: head(Term::Var("E".into())),
                body,
            }));
        }
    }
    out
}

/// Every head atom of a statement (a resource's are its `want` and `arg`).
fn head_atoms(s: &Stmt, out: &mut Vec<Atom>) {
    match s {
        Stmt::Fact(a) => out.push(a.clone()),
        Stmt::Rule(r) => out.push(r.head.clone()),
        _ => {}
    }
}

/// Every head atom of a fact or a rule, with the rule's body.
fn head_rules<'s>(s: &'s Stmt, out: &mut Vec<(&'s Atom, &'s [Lit])>) {
    match s {
        Stmt::Fact(a) => out.push((a, &[])),
        Stmt::Rule(r) => out.push((&r.head, &r.body)),
        _ => {}
    }
}

/// Every body atom of a statement, comprehensions included.
fn body_atoms(s: &Stmt, out: &mut Vec<Atom>) {
    fn lits(ls: &[Lit], out: &mut Vec<Atom>) {
        for l in ls {
            match l {
                Lit::Pos(a) | Lit::Not(a) => {
                    out.push(a.clone());
                    a.args.iter().for_each(|t| term(t, out));
                }
                Lit::Eq(x, y)
                | Lit::Neq(x, y)
                | Lit::Gt(x, y)
                | Lit::Ge(x, y)
                | Lit::Lt(x, y)
                | Lit::Le(x, y) => {
                    term(x, out);
                    term(y, out);
                }
            }
        }
    }
    fn term(t: &Term, out: &mut Vec<Atom>) {
        match t {
            Term::ListComp { item, body } => {
                term(item, out);
                lits(body, out);
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|t| term(t, out)),
            Term::Obj(m) => m.values().for_each(|t| term(t, out)),
            _ => {}
        }
    }
    match s {
        Stmt::Fact(a) => a.args.iter().for_each(|t| term(t, out)),
        Stmt::Rule(r) => {
            r.head.args.iter().for_each(|t| term(t, out));
            lits(&r.body, out);
        }
        Stmt::Resource(r) => {
            r.fields.iter().for_each(|f| term(&f.value, out));
            lits(r.body.as_deref().unwrap_or_default(), out);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Renaming predicates and scoping resource names.
// ---------------------------------------------------------------------------

fn rename_stmt(stmt: Stmt, names: &Names) -> Stmt {
    let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| rename_lit(l, names)).collect();
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rename_atom(a, names)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rename_atom(r.head, names),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            fields: rename_fields(r.fields, names),
            body: r.body.map(lits),
            ..r
        }),
        Stmt::Extern(mut e) => {
            if let Some(n) = names.get(&e.pred) {
                e.pred = n.clone();
            }
            Stmt::Extern(e)
        }
        Stmt::Mixed(mut e) => {
            if let Some(n) = names.get(&e.pred) {
                e.pred = n.clone();
            }
            Stmt::Mixed(e)
        }
        other => other,
    }
}

fn rename_fields(fs: Vec<FieldAssign>, names: &Names) -> Vec<FieldAssign> {
    fs.into_iter()
        .map(|f| FieldAssign {
            value: rename_term(f.value, names),
            ..f
        })
        .collect()
}

fn rename_lit(l: Lit, names: &Names) -> Lit {
    let t = |x| rename_term(x, names);
    match l {
        Lit::Pos(a) => Lit::Pos(rename_atom(a, names)),
        Lit::Not(a) => Lit::Not(rename_atom(a, names)),
        Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
        Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
        Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
        Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
        Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
        Lit::Le(a, b) => Lit::Le(t(a), t(b)),
    }
}

fn rename_atom(mut a: Atom, names: &Names) -> Atom {
    if let Some(n) = names.get(&a.pred) {
        a.pred = n.clone();
    }
    a.args = a.args.into_iter().map(|t| rename_term(t, names)).collect();
    a
}

fn rename_term(t: Term, names: &Names) -> Term {
    match t {
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rename_term(*item, names)),
            body: body.into_iter().map(|l| rename_lit(l, names)).collect(),
        },
        Term::Func { name, args } => Term::Func {
            name,
            args: args.into_iter().map(|t| rename_term(t, names)).collect(),
        },
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rename_term(t, names)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rename_term(v, names)))
                .collect(),
        ),
        other => other,
    }
}

/// How a copy's statements are scoped: its name, and whether a resource
/// written as a variable is its own (a component's copy) or any its user
/// sees (a module's, whose rules merge into the user's scope).
#[derive(Clone, Copy)]
struct Sc<'a> {
    name: &'a str,
    vars: bool,
}

/// Scope one statement of an instance: resource names in `want`, `arg`,
/// `attr`, `adopt` and `ref` become `scoped(Scope, Name)`, and `output(k, V)` is
/// `output(Scope, k, V)`.
fn rewrite_stmt(stmt: Stmt, sc: Sc) -> Stmt {
    let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| rewrite_lit(l, sc)).collect();
    let fields = |fs: Vec<FieldAssign>| {
        fs.into_iter()
            .map(|f| FieldAssign {
                value: rewrite_term(f.value, sc),
                ..f
            })
            .collect()
    };
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom(a, sc)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rewrite_atom(r.head, sc),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: rewrite_term(r.typ, sc),
            name: scoped_term(Sc { vars: true, ..sc }, r.name),
            fields: fields(r.fields),
            body: r.body.map(lits),
            ..r
        }),
        other => other,
    }
}

fn rewrite_lit(lit: Lit, sc: Sc) -> Lit {
    let t = |x| rewrite_term(x, sc);
    match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom(a, sc)),
        Lit::Not(a) => Lit::Not(rewrite_atom(a, sc)),
        Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
        Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
        Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
        Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
        Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
        Lit::Le(a, b) => Lit::Le(t(a), t(b)),
    }
}

fn rewrite_atom(mut atom: Atom, sc: Sc) -> Atom {
    let t = |x: Term| rewrite_term(x, sc);
    let pseudo =
        |a: &Term| matches!(a, Term::Val(Value::Str(s)) if s == INPUT || s == LET || s == "output");
    match (atom.pred.as_str(), atom.args.len()) {
        // A cell of a copy inside this one, `(input, inner, k)`, or a read
        // of its output: its scope is relative.
        ("arg" | "attr", 4 | 5) | (crate::refine::ATTR_REFINE, 4) if pseudo(&atom.args[0]) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[1] = prefix_scope(sc.name, args[1].clone());
            atom.args = args;
        }
        ("want", 2) | ("arg" | "attr", 4) | ("arg", 5) | ("arg_add", 4) => {
            atom.args = atom
                .args
                .into_iter()
                .enumerate()
                .map(|(i, a)| match i {
                    1 => scoped_term(sc, a),
                    _ => t(a),
                })
                .collect();
        }
        ("output", 2) | (ROWS, 2) => {
            let mut args = atom.args.into_iter().map(t);
            let (k, v) = (args.next().unwrap(), args.next().unwrap());
            atom.args = vec![str_term(sc.name), k, v];
        }
        // A copy inside this one's record: its user's scope is relative.
        (INSTANCE_OF, 3) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[1] = prefix_scope(sc.name, args[1].clone());
            atom.args = args;
        }
        // A copy inside this one's output, or its rows.
        ("output", 3) | (ROWS, 3) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[0] = prefix_scope(sc.name, args[0].clone());
            atom.args = args;
        }
        _ => atom.args = atom.args.into_iter().map(t).collect(),
    }
    atom
}

/// A scope relative to this copy, made absolute: `n.inner`, `n` for the
/// copy's own `""`.
fn prefix_scope(scope: &str, inner: Term) -> Term {
    match inner {
        Term::Val(Value::Str(s)) => str_term(&join_scope(scope, &s)),
        Term::Func { ref name, .. } if name == ABSOLUTE => inner,
        t => Term::Func {
            name: crate::ir::FORMAT.into(),
            args: vec![str_term(&format!("{scope}.%s")), t],
        },
    }
}

fn rewrite_term(term: Term, sc: Sc) -> Term {
    match term {
        Term::Func { ref name, .. } if name == ABSOLUTE => term,
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rewrite_term(t, sc)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rewrite_term(v, sc)))
                .collect::<BTreeMap<_, _>>(),
        ),
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rewrite_term(*item, sc)),
            body: body.into_iter().map(|l| rewrite_lit(l, sc)).collect(),
        },
        // A name a copy inside this one scoped: its scope is relative.
        Term::Func { name, mut args } if name == crate::ir::SCOPED && args.len() == 2 => {
            args[0] = prefix_scope(sc.name, args[0].clone());
            args[1] = rewrite_term(args[1].clone(), sc);
            Term::Func { name, args }
        }
        Term::Func { name, args } => {
            let is_ref = name == crate::ir::REF && args.len() == 3;
            let args = args
                .into_iter()
                .enumerate()
                .map(|(i, a)| match i {
                    1 if is_ref => scoped_term(sc, a),
                    _ => rewrite_term(a, sc),
                })
                .collect();
            Term::Func { name, args }
        }
        other => other,
    }
}

/// `scoped(Scope, Name)`; a name a copy inside this one scoped,
/// `scoped(inner, Name)`, is `scoped(Scope.inner, Name)`; and a name that
/// is already an address (`"blue.vpc"` names another copy's resource) is
/// itself. In a module's (`!sc.vars`), only a name the module writes out is
/// its own: a variable ranges over every resource its user sees.
fn scoped_term(sc: Sc, name: Term) -> Term {
    match name {
        Term::Func { name: ref f, .. } if f == crate::ir::SCOPED => rewrite_term(name, sc),
        Term::Func { name: ref f, .. } if f == ABSOLUTE => name,
        Term::Val(Value::Str(s)) if crate::ir::is_scoped(&s) => Term::Val(Value::Str(s)),
        name if !sc.vars && !matches!(name, Term::Val(_)) => rewrite_term(name, sc),
        name => Term::Func {
            name: crate::ir::SCOPED.to_string(),
            args: vec![str_term(sc.name), rewrite_term(name, sc)],
        },
    }
}

/// A statement with the marks of `ABSOLUTE` taken off: `__scope(t)` is
/// `t`.
fn unmark_stmt(s: Stmt) -> Stmt {
    fn term(t: Term) -> Term {
        match t {
            Term::Func { name, args } if name == ABSOLUTE && args.len() == 1 => {
                term(args.into_iter().next().unwrap())
            }
            Term::Func { name, args } => Term::Func {
                name,
                args: args.into_iter().map(term).collect(),
            },
            Term::List(xs) => Term::List(xs.into_iter().map(term).collect()),
            Term::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, term(v))).collect()),
            Term::ListComp { item, body } => Term::ListComp {
                item: Box::new(term(*item)),
                body: lits(body),
            },
            t => t,
        }
    }
    fn atom(mut a: Atom) -> Atom {
        a.args = a.args.into_iter().map(term).collect();
        a
    }
    fn lits(ls: Vec<Lit>) -> Vec<Lit> {
        ls.into_iter()
            .map(|l| match l {
                Lit::Pos(a) => Lit::Pos(atom(a)),
                Lit::Not(a) => Lit::Not(atom(a)),
                Lit::Eq(a, b) => Lit::Eq(term(a), term(b)),
                Lit::Neq(a, b) => Lit::Neq(term(a), term(b)),
                Lit::Gt(a, b) => Lit::Gt(term(a), term(b)),
                Lit::Ge(a, b) => Lit::Ge(term(a), term(b)),
                Lit::Lt(a, b) => Lit::Lt(term(a), term(b)),
                Lit::Le(a, b) => Lit::Le(term(a), term(b)),
            })
            .collect()
    }
    let fields = |fs: Vec<FieldAssign>| {
        fs.into_iter()
            .map(|f| FieldAssign {
                value: term(f.value),
                ..f
            })
            .collect()
    };
    match s {
        Stmt::Fact(a) => Stmt::Fact(atom(a)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: atom(r.head),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: term(r.typ),
            name: term(r.name),
            fields: fields(r.fields),
            body: r.body.map(lits),
            ..r
        }),
        other => other,
    }
}

/// Mark every statement (and each head, field and nested statement) as
/// lowered out of `origin` (`diag::origin_id`): a pack or a module
/// instance. What already has an origin keeps it.
pub fn set_origin(stmts: &mut [Stmt], origin: u32) {
    let fields = |fs: &mut [FieldAssign]| {
        for f in fs {
            f.span = f.span.within(origin);
        }
    };
    for s in stmts {
        match s {
            Stmt::Fact(a) => a.span = a.span.within(origin),
            Stmt::Rule(r) => r.head.span = r.head.span.within(origin),
            Stmt::Resource(r) => {
                r.span = r.span.within(origin);
                fields(&mut r.fields);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name that is already an address (`"m.i.n"`, another copy's
    /// resource) is left alone; a local one is scoped, a quoted segment
    /// too (After R-42, R-112).
    #[test]
    fn scoped_term_leaves_an_already_scoped_address_alone() {
        for vars in [true, false] {
            let sc = Sc { name: "m", vars };
            assert_eq!(scoped_term(sc, str_term("m.i.n")), str_term("m.i.n"));
            for local in ["n", r#""a.b""#] {
                assert_eq!(
                    scoped_term(sc, str_term(local)),
                    Term::Func {
                        name: crate::ir::SCOPED.to_string(),
                        args: vec![str_term("m"), str_term(local)],
                    }
                );
            }
        }
    }
}
