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
//! Each copy is its definition's items lowered again from the program's
//! nodes under it, outside in (`program::lower::instances`). This module
//! writes the stack's own inputs, outputs and `let`s as their cells,
//! checks each copy's interface (its block's inputs, its definition's
//! inputs and outputs, the relations it takes), and says the errors of
//! reading a copy's private name and of a `set` of an input the stack does
//! not address. What a module or a copy writes is not granted: ranks are
//! the ownership model, and the stratifier partitions a write by its
//! head's constant type and path.

use crate::ast::{
    Atom, FieldAssign, Helper, InputDecl, Lit, OutputDecl, Program, Resource, RuleStmt, Span, Stmt,
    Term, TypeExpr, atom, str_term,
};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::inputs::Declared;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

mod lexical;
mod named;

pub use lexical::{LEXICAL, LEXICAL_OF, lexical_name, lexical_parts, lexical_pred, lexical_term};

/// The attribute aggregate's pseudo-type for inputs: `(input, Scope, key)`,
/// scope `""` for the stack's own.
pub const INPUT: &str = "input";

/// The attribute aggregate's pseudo-type for `let` (R-3): `(let, Scope,
/// k)`, scope `""` for the program's own, `n` in the instance `n`, `m` in
/// the activation `use m`. The resolver writes a `let` as `let(k, t, rank)`, a
/// head no source can spell, and `lets` makes it the cell's contribution.
pub const LET: &str = "let";

/// A head with no body and no variables is a fact; anything else a rule.
pub(crate) fn fact_or_rule(head: Atom, body: Vec<Lit>) -> Stmt {
    if body.is_empty() && head.args.iter().all(is_ground) {
        Stmt::Fact(head)
    } else {
        Stmt::Rule(RuleStmt::new(head, body))
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
pub(crate) struct Interface {
    pub inputs: Vec<InputDecl>,
    /// `input p`: relations the user gives the rows of (R-55).
    pub relations: Vec<crate::ast::Extern>,
    /// `output p`: relations the copy exports.
    pub relation_outputs: Vec<OutputDecl>,
    pub outputs: BTreeMap<String, OutputDecl>,
    pub output_values: Vec<OutputDecl>,
}

/// Split a module's or component's body into its interface and its
/// statements; the definitions nested in it are left out.
pub(crate) fn interface(body: &[Stmt]) -> (Interface, Vec<Stmt>) {
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
pub(crate) fn defined_preds(stmts: &[Stmt], out: &mut BTreeMap<String, (usize, Span)>) {
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

/// A copy's lets are its cells (`scope` its name, `name` its reader of a
/// key), each new atom as the copy writes it (`finish`).
pub(crate) fn lets(
    stmts: Vec<Stmt>,
    scope: &str,
    name: Option<&dyn Fn(&str) -> String>,
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
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
        out.push(fact_or_rule(finish(arg), body));
    }
    for (k, span) in &secret {
        let root = k.split('.').next().unwrap_or(k);
        if keys.contains_key(root) {
            out.push(Stmt::Fact(finish(atom(
                crate::transform::SECRET_CELL,
                vec![str_term(LET), str_term(scope), str_term(k)],
                *span,
            ))));
        }
    }
    for (k, span) in keys {
        let pred = name.map_or_else(|| k.clone(), |n| n(&k));
        let v = Term::Var("V".into());
        out.push(Stmt::Rule(RuleStmt::new(
            finish(atom(&pred, vec![v.clone()], span)),
            vec![Lit::Pos(finish(atom(
                "attr",
                vec![str_term(LET), str_term(scope), str_term(&k), v],
                span,
            )))],
        )));
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
    exclusive_with(name, alts, &|a| a)
}

/// [`exclusive`], each new atom as the scope writes it (`finish`).
pub fn exclusive_with(
    name: &str,
    alts: &[Alternative],
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
    let mut out: Vec<Stmt> = alts
        .iter()
        .enumerate()
        .map(|(i, a)| declared_with(name, i, a.clause.clone(), a.span, finish))
        .collect();
    let sites: Vec<(String, Span)> = alts.iter().map(|a| (a.what.clone(), a.span)).collect();
    out.extend(denies_with(name, &sites, finish));
    out
}

/// The `i`th declaration of `name` holds while its clause does:
/// `__declared(name, i) :- clause`.
pub fn declared(name: &str, i: usize, clause: Vec<Lit>, span: Span) -> Stmt {
    declared_with(name, i, clause, span, &|a| a)
}

/// [`declared`], its head as the scope writes it (`finish`).
pub fn declared_with(
    name: &str,
    i: usize,
    clause: Vec<Lit>,
    span: Span,
    finish: &dyn Fn(Atom) -> Atom,
) -> Stmt {
    let head = finish(held(name, i, span));
    match clause.is_empty() {
        true => Stmt::Fact(head),
        false => Stmt::Rule(RuleStmt::helper(Helper::Check, head, clause)),
    }
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
    denies_with(name, sites, &|a| a)
}

/// [`denies`], each atom as the scope writes it (`finish`).
pub fn denies_with(
    name: &str,
    sites: &[(String, Span)],
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
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
            out.push(Stmt::Rule(RuleStmt::new(
                finish(atom("deny", vec![str_term(&msg)], b.1)),
                vec![
                    Lit::Pos(finish(held(name, i, a.1))),
                    Lit::Pos(finish(held(name, j, b.1))),
                ],
            )));
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
            // A copy named by its clause binds no name (R-191).
            Stmt::Instance(u) if u.named.is_some() => continue,
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

/// Whether a module or a copy keeps `pred` its own when it defines it:
/// every relation but the shared ones and the records of copies.
pub fn is_private(pred: &str) -> bool {
    !is_shared(pred) && pred != INSTANCE_OF && pred != ROWS && pred != LEXICAL_OF
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

/// Every statement of `stmts` and of the definitions (modules and
/// components) nested in them, depth first.
pub fn nested(stmts: &[Stmt]) -> Vec<&Stmt> {
    let mut out = Vec::new();
    for s in stmts {
        out.push(s);
        if let Stmt::Module(m) = s {
            out.extend(nested(&m.body));
        }
    }
    out
}

/// Every definition of the program, by path, wherever it stands.
pub fn definitions(stmts: &[Stmt]) -> BTreeMap<&str, &crate::ast::Module> {
    nested(stmts)
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Module(m) => Some((m.name.as_str(), m)),
            _ => None,
        })
        .collect()
}

/// The program's statements and those of every module a `use` imports and
/// every component a `resource` copies, each definition's once: where a
/// declaration of the deployment may stand (a provider's `use` and its
/// settings, R-129). A definition no statement reaches is left out.
pub fn reached(program: &Program) -> Vec<&Stmt> {
    let defs = definitions(&program.statements);
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

/// What an expansion finds as it goes: the interface the later passes
/// check (every typed input, the secret outputs, the outputs' types),
/// what is wrong, and what the checks of later copies read.
#[derive(Default)]
pub(crate) struct Book {
    pub diags: Vec<Diagnostic>,
    pub declared: Vec<Declared>,
    pub secret_outputs: Vec<(String, String)>,
    pub output_types: BTreeMap<(String, String), TypeExpr>,
    /// Private names by plain name, for the error when the program reads
    /// one.
    pub private: BTreeMap<String, (String, Option<String>)>,
    /// The definitions whose interface was checked.
    pub checked: BTreeSet<String>,
    /// The scopes the stack reaches by `use` alone, `""` first: their
    /// inputs are the stack's to give (R-55).
    pub flat: BTreeSet<String>,
    /// The inputs each body being expanded declares, the stack's first:
    /// what a `use` in it may give by the pun (R-208).
    pub inputs_here: Vec<BTreeSet<String>>,
    /// Where each input is declared, each leaf of an object one and each
    /// declaration of a name declared twice (R-104): a default written
    /// there is the declaration's own.
    pub declared_at: BTreeSet<Span>,
}

/// One copy as its checks read it: the copy as its user writes it,
/// whether it is a module's or a component's, whether the stack reaches
/// it by `use` alone, its scope's absolute name and its statement.
pub(crate) struct Copy<'u> {
    pub u: &'u crate::ast::Instance,
    /// `module` or `component`, as messages name the definition.
    pub kind: &'static str,
    /// A used module the stack reaches by `use` alone: its inputs are the
    /// stack's to give, `m.k` (R-55).
    pub flat: bool,
    pub abs: String,
    /// The statement as the program writes it (`use m as n`).
    pub origin: String,
}

impl Copy<'_> {
    /// Its name in its user's scope.
    fn scope(&self) -> &str {
        &self.u.name
    }
}

/// The expansion of the program's own statements: what it finds, and the
/// instancing of its copies.
struct Cx<'a> {
    book: Book,
    copies: crate::program::lower::instances::Instancer<'a>,
}
pub(crate) fn check_types(who: &str, inputs: &[InputDecl], diags: &mut Vec<Diagnostic>) {
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

/// The program with its copies instanced (`program::lower::instances`,
/// from the program's nodes), its own inputs, outputs and `let`s as their
/// cells, and its interface checked.
pub fn expand(program: &Program) -> Result<Expanded> {
    let none = crate::program::Program::default();
    let nodes = program.nodes.as_deref().unwrap_or(&none);
    let mut cx = Cx::new(program, nodes);
    let out = cx.stack(&program.statements);
    // The program's own `let`s.
    let mut expanded = lets(out, "", None, &|a| a);
    expanded.extend(mixed(&expanded));
    cx.book.private_reads(&expanded);
    cx.book.givings(&expanded);
    // What a `set`, a `use` block or a copy gives a typed input is read as
    // its type (R-31, R-134): a literal now, a computed value at run time.
    for s in &mut expanded {
        if let Stmt::Fact(h) | Stmt::Rule(RuleStmt { head: h, .. }) = s {
            read_input(h, &cx.book.declared);
        }
    }
    let expanded = expanded.into_iter().map(unmark_stmt).collect::<Vec<_>>();
    cx.finish(Program {
        statements: expanded,
        stack: program.stack.clone(),
        nodes: None,
    })
}

/// `instance_of` (and a copy's private one) is a fact for a copy with no
/// clause and a rule for one with, and `secret_cell` a fact of a secret
/// `let` and a rule of a copy named by its clause (R-191), which takes
/// its scope's: both, by design.
fn mixed(stmts: &[Stmt]) -> Vec<Stmt> {
    let mut mixed = BTreeMap::new();
    for s in stmts {
        match s {
            Stmt::Fact(a) | Stmt::Rule(RuleStmt { head: a, .. }) if a.pred == INSTANCE_OF => {
                mixed.entry(a.pred.clone()).or_insert(a.span);
            }
            Stmt::Rule(RuleStmt { head: a, .. }) if a.pred == crate::transform::SECRET_CELL => {
                mixed.entry(a.pred.clone()).or_insert(a.span);
            }
            _ => {}
        }
    }
    mixed
        .into_iter()
        .map(|(pred, span)| {
            Stmt::Mixed(crate::ast::Extern {
                pred,
                arity: 3,
                span,
            })
        })
        .collect()
}

/// The addresses a `set` may give (R-54, R-38): every input the stack
/// addresses, each object an object input holds, and the scopes they are
/// declared in.
struct Settable {
    paths: BTreeSet<String>,
    scopes: BTreeSet<String>,
}

impl Settable {
    fn of(declared: &[Declared]) -> Settable {
        let mut paths = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        for d in declared {
            let Some(a) = &d.address else { continue };
            let base = &a[..a.len() - d.decl.name.len()];
            paths.insert(a.clone());
            scopes.insert(d.scope.clone());
            for (i, _) in d.decl.name.match_indices('.') {
                paths.insert(format!("{base}{}", &d.decl.name[..i]));
            }
        }
        Settable { paths, scopes }
    }

    /// The error of a `set` of `address` the stack does not address.
    fn unknown(&self, what: &str, address: &str, span: Span) -> Option<Diagnostic> {
        if self.paths.is_empty() || self.paths.contains(address) {
            return None;
        }
        let msg = match address
            .rsplit_once('.')
            .filter(|(o, _)| self.paths.contains(*o))
        {
            Some((o, f)) => format!("{what}: input {o} has no field {f}"),
            None => format!("{what}: the program declares no input {address}"),
        };
        Some(Diagnostic::error(span, msg))
    }
}

impl<'a> Cx<'a> {
    fn new(program: &Program, nodes: &'a crate::program::Program) -> Cx<'a> {
        let inputs = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Input(i) => Some(i.name.clone()),
                _ => None,
            })
            .collect();
        let records = crate::program::Records::core().with_items(nodes, &nodes.roots);
        Cx {
            book: Book {
                flat: BTreeSet::from([String::new()]),
                inputs_here: vec![inputs],
                ..Book::default()
            },
            copies: crate::program::lower::instances::Instancer::new(nodes, &records),
        }
    }

    /// The program's own statements, its `use`s and copies expanded.
    fn stack(&mut self, stmts: &[Stmt]) -> Vec<Stmt> {
        let mut out = exclusive_copies(stmts);
        for s in stmts {
            match s {
                // The stack's own input: read as `k(V)`, given by `--set`
                // (an `input(k, V)` fact) at the normal rank.
                Stmt::Input(i) => out.extend(self.stack_input(i)),
                // The stack's own relation, exported: published as its rows.
                Stmt::Output(o) if o.relation.is_some() => out.push(published_rows(o)),
                // The stack's own output: `output(k, V)` in the root scope.
                Stmt::Output(o) if o.value.is_none() => self.stack_output(o),
                Stmt::Output(o) => out.push(fact_or_rule(
                    atom(
                        "output",
                        vec![str_term(&o.name), o.value.clone().unwrap()],
                        o.span,
                    ),
                    Vec::new(),
                )),
                Stmt::Use(u) => out.extend(self.copies.top(&mut self.book, u, true)),
                Stmt::Instance(u) => out.extend(self.copies.top(&mut self.book, u, false)),
                Stmt::Module(_) => {}
                other => out.push(other.clone()),
            }
        }
        out
    }

    /// The stack's own input `i`: its reader, what `--set` gives it, its
    /// refinements, and its leaves declared.
    fn stack_input(&mut self, i: &InputDecl) -> Vec<Stmt> {
        check_types("stack", std::slice::from_ref(i), &mut self.book.diags);
        let same = |a: Atom| a;
        let mut out = input_reader("", i, &|p: &str| p.to_string(), &same);
        out.extend(given_rules("", "", i, &same));
        for leaf in crate::inputs::leaves(i) {
            out.extend(refinement(&leaf, &leaf, "", &same));
            self.book.declared_at.insert(leaf.span);
            let address = Some(leaf.name.clone());
            self.book
                .declared
                .push(Declared::new("", leaf, address, false));
        }
        out
    }

    /// The stack's own declared output `o`: its secret cells and its type.
    fn stack_output(&mut self, o: &OutputDecl) {
        self.book
            .secret_outputs
            .extend(secret_paths(o).into_iter().map(|k| (String::new(), k)));
        if let Some(t) = &o.ty {
            self.book
                .output_types
                .insert((String::new(), o.name.clone()), t.clone());
        }
    }

    /// The expansion's result: the program, or what was found wrong.
    fn finish(self, program: Program) -> Result<Expanded> {
        let book = self.book;
        if !book.diags.is_empty() {
            return Err(Diagnostics(book.diags).into());
        }
        Ok(Expanded {
            program,
            inputs: book.declared,
            secret_outputs: book.secret_outputs,
            output_types: book.output_types,
        })
    }
}

impl Book {
    /// A read of a name only a module or a component defines: say it is
    /// private.
    fn private_reads(&mut self, stmts: &[Stmt]) {
        let mut defined = BTreeMap::new();
        defined_preds(stmts, &mut defined);
        let externs: BTreeSet<&str> = stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Extern(e) => Some(e.pred.as_str()),
                _ => None,
            })
            .collect();
        let mut reads = Vec::new();
        for s in stmts {
            body_atoms(s, &mut reads);
        }
        for a in reads {
            if let Some((owner, help)) = self.private.get(&a.pred)
                && !defined.contains_key(&a.pred)
                && !externs.contains(a.pred.as_str())
            {
                let d = Diagnostic::error(a.span, format!("{} is private to {owner}", a.pred));
                self.diags.push(match help {
                    Some(h) => d.with_help(h.clone()),
                    None => d,
                });
            }
        }
    }

    /// `h` is a declaration's own default: it gives nothing, to its input
    /// or to another declaration of the same name.
    fn own_default(&self, h: &Atom) -> bool {
        let rank = h.args.get(4);
        self.declared_at.contains(&h.span)
            && matches!(rank, Some(Term::Val(Value::Str(r))) if r == "default")
    }

    /// `set k = t where B`, alone or in a block, gives an input the stack
    /// addresses (R-54, R-38): its own, a field of an object one, a used
    /// module's (its cell `(input, m, k)`).
    fn givings(&mut self, stmts: &[Stmt]) {
        let settable = Settable::of(&self.declared);
        let mut heads = Vec::new();
        for s in stmts {
            head_rules(s, &mut heads);
        }
        for (h, body) in heads {
            self.giving(h, body, &settable);
        }
    }

    /// The `set` whose head is `h`: an error when the stack addresses no
    /// such input or it is a key, else the inputs it reaches given.
    fn giving(&mut self, h: &Atom, body: &[Lit], settable: &Settable) {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(scope)),
            Term::Val(Value::Str(k)),
            ..,
        ] = h.args.as_slice()
        else {
            return;
        };
        if h.pred != "arg" || t != INPUT || !(scope.is_empty() || settable.scopes.contains(scope)) {
            return;
        }
        // `--set` (`input(k, V)`) is checked where it is given.
        if matches!(body, [Lit::Pos(a)] if a.pred == INPUT) {
            return;
        }
        let address = crate::types::dotted(scope, k);
        let what = format!("`set {address}`");
        if let Some(d) = settable.unknown(&what, &address, h.span) {
            self.diags.push(d);
            return;
        }
        if self.own_default(h) {
            return;
        }
        let under =
            |a: &str, b: &str| a == b || a.strip_prefix(b).is_some_and(|r| r.starts_with('.'));
        for d in self.declared.iter_mut() {
            if d.scope != *scope || !(under(&d.decl.name, k) || under(k, &d.decl.name)) {
                continue;
            }
            if d.decl.key {
                self.diags.push(
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

    /// The checks on the interface of the definition `c` copies, once per
    /// definition (`written`: its statements as written).
    pub fn check_once(&mut self, c: &Copy, iface: &Interface, written: &[Stmt]) {
        let path = &c.u.module;
        if self.checked.insert(path.clone()) {
            check_component(c.kind, path, iface, written, &mut self.diags);
            check_types(
                &format!("{} {path}", c.kind),
                &iface.inputs,
                &mut self.diags,
            );
        }
    }

    /// The names the copy `c` keeps private (`keys`), each with the error
    /// a read of it from outside says.
    pub fn privates(&mut self, c: &Copy, keys: &BTreeSet<String>) {
        let (scope, module) = (c.scope(), &c.u.module);
        let used = c.kind == "module";
        for p in keys {
            let help = match used {
                true => format!("read it as {scope}.{p}, after `use {module}`"),
                false => format!(
                    "pass the value through an output of component {module}, `output {p} = ...`, \
                     and read it as INSTANCE.{p}",
                ),
            };
            self.private
                .insert(p.clone(), (format!("{} {module}", c.kind), Some(help)));
        }
    }

    /// The secret cells and the types of the outputs the copy `c`
    /// declares.
    pub fn outputs(&mut self, c: &Copy, iface: &Interface) {
        for o in iface.outputs.values() {
            self.secret_outputs
                .extend(secret_paths(o).into_iter().map(|k| (c.abs.clone(), k)));
            if let Some(t) = &o.ty {
                self.output_types
                    .insert((c.abs.clone(), o.name.clone()), t.clone());
            }
        }
    }

    /// The typed inputs of the copy `c`, each leaf once: whether its block
    /// gives it, and for a used module's where the stack addresses it and
    /// whether its user gives it by the pun (`user_inputs`, R-208).
    pub fn declare_inputs(&mut self, c: &Copy, iface: &Interface, user_inputs: &BTreeSet<String>) {
        for i in &iface.inputs {
            for leaf in crate::inputs::leaves(i) {
                let address = c.flat.then(|| crate::types::dotted(&c.abs, &leaf.name));
                let bound = c.u.inputs.iter().any(|(k, _, _)| covers(k, &leaf.name));
                self.declared_at.insert(leaf.span);
                // A name declared twice (R-104) declares its inputs once.
                if self
                    .declared
                    .iter()
                    .any(|d| d.scope == c.abs && d.decl.name == leaf.name)
                {
                    continue;
                }
                let pun = user_inputs.contains(&leaf.name);
                let mut d = Declared::new(&c.abs, leaf, address, bound);
                d.used_at = c.flat.then_some(c.u.span);
                d.used_by = c.flat.then(|| (c.origin.clone(), pun));
                self.declared.push(d);
            }
        }
    }
}

/// `arg(input, scope, k, V, ..)` with `V` read as the declared type of
/// the input `k` of `scope`, or of the fields of `V` the inputs under `k`
/// declare (`gke = { subnet_cidr: "10.0.0.0/22" }`).
fn read_input(h: &mut Atom, declared: &[Declared]) {
    let Some(Term::Val(Value::Str(scope))) = h.args.get(1) else {
        return;
    };
    let scope = scope.clone();
    read_input_of(h, declared.iter().filter(|d| d.scope == scope));
}

/// [`read_input`] by the declarations `declared` of its scope.
pub(crate) fn read_input_of<'d>(h: &mut Atom, declared: impl Iterator<Item = &'d Declared>) {
    if h.pred != "arg" || h.args.len() < 4 || h.args[0] != str_term(INPUT) {
        return;
    }
    let Term::Val(Value::Str(k)) = &h.args[2] else {
        return;
    };
    let k = k.clone();
    for d in declared {
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

/// A gated copy's own relation (`gate`): `n::__instance(path)` while the
/// copy `n` of the component at `path` exists.
pub const GATE: &str = "__instance";

/// The copy a fact of a gate names, when the copy is named by its clause
/// (R-191): its address (its component's path and its scope, `node
/// agent-1`, `node main.agent-1` inside the copy `main`). Each row of the
/// clause holds one such fact.
pub fn copy_by_clause(a: &Atom) -> Option<crate::address::Address> {
    let (scope, p) = a.pred.rsplit_once("::")?;
    let (name, args) = named::runtime_scope(scope, &a.args);
    let [Term::Val(Value::Str(path))] = args else {
        return None;
    };
    (p == GATE && args.len() < a.args.len()).then(|| crate::address::Address {
        typ: path.clone(),
        name,
    })
}

/// A fact of a predicate private to a copy or an activation (`n::p`,
/// `n.inner::p`, a name no source can spell) as the program names it: a
/// gated copy's own relation as the statement that makes it, `resource
/// PATH n` (`(in USER)` for a copy inside another), anything else by its
/// name there, `p(..) (in n)`; `fmt` prints that atom and `gap` comes
/// before the parenthesis. `None` for a predicate of the program's.
pub fn private_text(a: &Atom, fmt: &dyn Fn(&Atom) -> String, gap: &str) -> Option<String> {
    let (scope, p) = a.pred.rsplit_once("::")?;
    // A copy named by its clause (R-191) is the one its columns name.
    let (scope, args) = named::runtime_scope(scope, &a.args);
    if p == GATE
        && let Some(Term::Val(Value::Str(path))) = args.first()
    {
        return Some(match crate::address::scope_split(&scope) {
            None => format!("resource {path} {scope}"),
            Some((user, name)) => format!("resource {path} {name}{gap}(in {user})"),
        });
    }
    let own = Atom {
        pred: p.to_string(),
        args: args.to_vec(),
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
pub(crate) fn instance_inputs(
    c: &Copy,
    iface: &Interface,
    out: &mut Vec<Stmt>,
    diags: &mut Vec<Diagnostic>,
    finish: &dyn Fn(Atom) -> Atom,
) {
    let leaves: Vec<InputDecl> = iface
        .inputs
        .iter()
        .flat_map(crate::inputs::leaves)
        .collect();
    let (u, kind) = (c.u, c.kind);
    let body = u.body.clone().unwrap_or_default();
    for (k, v, span) in &u.inputs {
        if !leaves.iter().any(|l| covers(k, &l.name)) {
            diags.push(no_input(c, iface, &leaves, k, *span));
            continue;
        }
        // A literal is read as the input's declared type (R-31).
        let read = entry_type(iface, k)
            .map(|t| crate::types::literal(&crate::types::of_expr(t), v.clone()));
        let v = match read {
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
                str_term(c.scope()),
                str_term(k),
                v,
                str_term(crate::transform::NORMAL),
            ],
            *span,
        );
        out.push(fact_or_rule(finish(head), body.clone()));
    }
    // An input nothing gives a value is a stack input's error (R-65),
    // fixed the same ways: a default, or a value given.
    if !c.flat {
        diags.extend(required(c, &leaves));
    }
}

/// Whether an entry `k` gives the input `leaf`: the leaf itself or an
/// object holding it.
fn covers(k: &str, leaf: &str) -> bool {
    leaf == k || leaf.strip_prefix(k).is_some_and(|r| r.starts_with('.'))
}

/// The entry `k`'s type: the leaf's, or the object's it names.
fn entry_type<'i>(iface: &'i Interface, k: &str) -> Option<&'i TypeExpr> {
    let i = iface.inputs.iter().find(|i| covers(&i.name, k))?;
    k[i.name.len()..]
        .split('.')
        .skip(1)
        .try_fold(&i.ty, |t, seg| match t {
            TypeExpr::Object(fs) => fs.iter().find(|(f, _)| f == seg).map(|(_, t)| t),
            _ => None,
        })
}

/// The error of an entry `k` that gives no input of the copy `c`: the
/// object input it names has no such field, or there is no such input.
fn no_input(c: &Copy, iface: &Interface, leaves: &[InputDecl], k: &str, span: Span) -> Diagnostic {
    let (kind, module) = (c.kind, &c.u.module);
    let object = k
        .match_indices('.')
        .map(|(n, _)| &k[..n])
        .rfind(|o| leaves.iter().any(|l| l.name.starts_with(&format!("{o}."))));
    if let Some(o) = object {
        let fields: Vec<&str> = leaves
            .iter()
            .filter_map(|l| l.name.strip_prefix(o)?.strip_prefix('.'))
            .collect();
        return Diagnostic::error(
            span,
            format!(
                "input {o} of {kind} {module} has no field {}",
                &k[o.len() + 1..]
            ),
        )
        .with_note(format!("its fields: {}", fields.join(", ")));
    }
    let declared: Vec<&str> = iface.inputs.iter().map(|i| i.name.as_str()).collect();
    let d = Diagnostic::error(span, format!("{kind} {module} has no input {k}"));
    if declared.is_empty() {
        d.with_note(format!("{kind} {module} declares no inputs"))
    } else {
        d.with_note(format!("its inputs: {}", declared.join(", ")))
    }
}

/// The errors of the copy `c`'s inputs that nothing gives a value: each
/// with no default and no clause (a dependent input is required where
/// its clause holds: a deny).
fn required(c: &Copy, leaves: &[InputDecl]) -> Vec<Diagnostic> {
    let given = |i: &InputDecl| c.u.inputs.iter().any(|(k, _, _)| covers(k, &i.name));
    leaves
        .iter()
        .filter(|i| i.default.is_none() && i.guard.is_empty() && !given(i))
        .map(|i| {
            Diagnostic::error(
                c.u.span,
                format!(
                    "input {} is required and has no value",
                    crate::types::dotted(c.scope(), &i.name)
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
            ))
        })
        .collect()
}

/// The checks on a component's interface, once per component: an input
/// or an output is declared once, an input is not also defined, and every
/// output it gives a value is declared.
pub(crate) fn check_component(
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

/// `output p` of a copy: its rows `__rows(p, [X1, ..]) :- p(X1, ..)`, the
/// copy's scope put in front by scoping. A column of the copy's resources
/// holds the references themselves (R-204), whose addresses scoping made
/// the copy's: the reader has the resource, not its address.
pub(crate) fn rows_of(o: &OutputDecl, finish: &dyn Fn(Atom) -> Atom) -> Stmt {
    let arity = o.relation.as_ref().map_or(0, Vec::len);
    let vars: Vec<Term> = (0..arity).map(|i| Term::Var(format!("X{i}"))).collect();
    Stmt::Rule(RuleStmt::helper(
        Helper::Check,
        finish(atom(
            ROWS,
            vec![str_term(&o.name), Term::List(vars.clone())],
            o.span,
        )),
        vec![Lit::Pos(finish(atom(&o.name, vars, o.span)))],
    ))
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

pub(crate) fn refine_pred(input: &str) -> String {
    format!("__refine_{input}")
}

/// `input k: T where R`: what of `R` fits the checkable table is a
/// refinement of the input's cell (`input_reader`, `crate::refine`); the
/// rest is a deny unless it holds of the input's value: `__refine_k(V) :-
/// k(V), R[k := V]` and `deny(...) :- k(V), not __refine_k(V)`. The input
/// is written by its name in `R`.
/// `written` is the input as its definition writes it, which the check's
/// message spells; each new atom is as the scope writes it (`finish`).
pub(crate) fn refinement(
    i: &InputDecl,
    written: &InputDecl,
    scope: &str,
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
    let (_, rest) = crate::refine::split_input(i);
    if rest.is_empty() {
        return Vec::new();
    }
    let v = Term::Var("__Input".into());
    // A leaf of an object input is read in its object (R-54).
    let read = match i.name.split_once('.') {
        None => vec![Lit::Pos(finish(atom(&i.name, vec![v.clone()], i.span)))],
        Some((top, rest)) => {
            let o = Term::Var("__Object".into());
            vec![
                Lit::Pos(finish(atom(top, vec![o.clone()], i.span))),
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
    let names = |v: &Term| BTreeMap::from([(i.name.clone(), v.clone())]);
    body.extend(rest.iter().map(|l| l.replace_names(&names(&v))));
    let ok = finish(atom(&refine_pred(&i.name), vec![v.clone()], i.span));
    // The check as the program writes it, one policy wherever the scope
    // that declares it is copied; the copy is the deny's context, and a
    // secret input's value is not said.
    let mut ctx = BTreeMap::new();
    if !matches!(&i.ty, TypeExpr::Apply(n, _) if n == "secret") {
        ctx.insert("value".to_string(), v.clone());
    }
    if !scope.is_empty() {
        ctx.insert("of".to_string(), str_term(scope));
    }
    let deny = finish(atom(
        "deny",
        vec![
            str_term(&crate::inputs::check_message(written)),
            Term::Obj(ctx),
        ],
        i.span,
    ));
    vec![
        Stmt::Rule(RuleStmt::helper(Helper::Check, ok.clone(), body)),
        Stmt::Rule(RuleStmt::new(
            deny,
            read.into_iter().chain([Lit::Not(ok)]).collect(),
        )),
    ]
}

/// One input's reader, named by `pred` of its name, `k(V) :- attr(input,
/// Scope, k, V)`, and its `@default` contributions and checkable
/// refinements. An object input (R-54) is one cell per leaf: each leaf's
/// default and check is the leaf's (`arg(input, Scope, "nodes.count", 1,
/// @default)`, which the aggregate merges into the object `nodes` leaf by
/// leaf), and the object is read whole.
/// Each new atom is as the scope writes it (`finish`).
pub(crate) fn input_reader(
    scope: &str,
    i: &InputDecl,
    pred: &dyn Fn(&str) -> String,
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
    let v = Term::Var("V".into());
    // A dependent input (R-104) is read, and defaults, where its clause
    // holds; where it holds with no value, it is required.
    let mut body = vec![Lit::Pos(finish(atom(
        "attr",
        vec![
            str_term(INPUT),
            str_term(scope),
            str_term(&i.name),
            v.clone(),
        ],
        i.span,
    )))];
    body.extend(i.guard.iter().cloned());
    let mut out = vec![Stmt::Rule(RuleStmt::new(
        finish(atom(&pred(&i.name), vec![v], i.span)),
        body,
    ))];
    if !i.guard.is_empty() && i.default.is_none() && i.fields.is_empty() {
        let who = match scope {
            "" => format!("input {}", i.name),
            s => format!("input {} of {s}", i.name),
        };
        let mut body = i.guard.clone();
        body.push(Lit::Not(finish(atom(
            &pred(&i.name),
            vec![Term::Wildcard],
            i.span,
        ))));
        out.push(Stmt::Rule(RuleStmt::new(
            finish(atom(
                "deny",
                vec![str_term(&format!(
                    "{who} is required and has no value: its clause holds in this deployment"
                ))],
                i.span,
            )),
            body,
        )));
    }
    for l in crate::inputs::leaves(i) {
        let (checkable, _) = crate::refine::split_input(&l);
        for c in checkable {
            let Stmt::Fact(fact) =
                crate::refine::refine_fact(INPUT, Some(scope), &l.name, &c, l.span)
            else {
                unreachable!("a refinement is a fact")
            };
            out.push(Stmt::Fact(finish(fact)));
        }
        if let Some(d) = &l.default {
            out.push(fact_or_rule(
                finish(atom(
                    "arg",
                    vec![
                        str_term(INPUT),
                        str_term(scope),
                        str_term(&l.name),
                        d.clone(),
                        str_term(crate::ast::Rank::Default.name()),
                    ],
                    l.span,
                )),
                i.guard.clone(),
            ));
        }
    }
    out
}

/// The paths an input is given by: the input, each object an object
/// input holds and each leaf (`nodes`, `nodes.pool`, `nodes.pool.size`).
fn input_paths(i: &InputDecl) -> Vec<String> {
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
/// Each new atom is as the scope writes it (`finish`).
pub(crate) fn given_rules(
    scope: &str,
    address: &str,
    i: &InputDecl,
    finish: &dyn Fn(Atom) -> Atom,
) -> Vec<Stmt> {
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
        out.push(Stmt::Rule(RuleStmt::new(
            finish(head(v.clone())),
            vec![Lit::Pos(finish(atom(
                "input",
                vec![str_term(&crate::types::dotted(address, &p)), v.clone()],
                span,
            )))],
        )));
        // A map's key, `input("labels.team", V)`: the entry `{team: V}`.
        if leaf.is_some_and(|l| crate::inputs::is_map(&l.ty)) {
            let k = Term::Var("K".into());
            let mut body = vec![Lit::Pos(finish(atom(
                "input",
                vec![k.clone(), v.clone()],
                span,
            )))];
            let prefix = crate::types::dotted(address, &p);
            body.extend(
                crate::inputs::map_entry_lits(&k, &prefix, v, "E")
                    .into_iter()
                    .map(|l| l.map(finish, |t| t)),
            );
            out.push(Stmt::Rule(RuleStmt::new(
                finish(head(Term::Var("E".into()))),
                body,
            )));
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
        ls.into_iter().map(|l| l.map_terms(term)).collect()
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
            ..r
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

    /// `nested` walks every definition's body, `definitions` names them by
    /// path wherever they stand, and `reached` keeps the bodies a `use`
    /// or a copy reaches, each once (R-129).
    #[test]
    fn reached_walks_the_bodies_a_use_reaches_once() {
        let fact = |p: &str| Stmt::Fact(crate::ast::atom(p, vec![], Span::default()));
        let module = |name: &str, body: Vec<Stmt>| {
            Stmt::Module(crate::ast::Module {
                name: name.into(),
                component: false,
                body,
                span: Span::default(),
            })
        };
        let using = |module: &str| {
            Stmt::Use(crate::ast::Instance {
                module: module.into(),
                name: module.into(),
                named: None,
                inputs: vec![],
                rows: vec![],
                body: None,
                clause: None,
                via: None,
                span: Span::default(),
            })
        };
        let program = Program {
            statements: vec![
                fact("top"),
                module("a", vec![fact("in_a"), module("a.b", vec![fact("in_b")])]),
                module("unused", vec![fact("in_unused")]),
                using("a.b"),
                using("a.b"),
            ],
            stack: None,
            nodes: None,
        };
        let preds = |stmts: Vec<&Stmt>| -> Vec<String> {
            stmts
                .into_iter()
                .filter_map(|s| match s {
                    Stmt::Fact(a) => Some(a.pred.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            preds(nested(&program.statements)),
            ["top", "in_a", "in_b", "in_unused"]
        );
        assert_eq!(
            definitions(&program.statements).keys().collect::<Vec<_>>(),
            [&"a", &"a.b", &"unused"]
        );
        assert_eq!(preds(reached(&program)), ["top", "in_b"]);
    }
}
