//! `dform dev effects TARGET` (DESIGN.org R-11c): what each scope reads,
//! writes and offers, read off the lowered program and the partition
//! graph's path normalization. Its coeffects (R-155), the reads the
//! context satisfies, are listed by kind and grant, a capability list a
//! review reads: `reads ssh://*` (a location by scheme and host, a
//! project file by its path), `reads provider ovh` (a data source),
//! `needs secret TOKEN` (`env.var`), `reads clock` (`time.now()`),
//! `reads memo KEY` (`memo.first`). No evaluation: a variable-typed write
//! (a non-constant type or path) prints `*` rather than guess what it
//! would be once evaluated.
//!
//! A scope is the stack, an instance (by its name) or a module `use`d. `modules::expand` already tags every statement it lowers
//! out of an instance or a pack with an origin (`diag::origin`); `compute`
//! reads that tag back off each rule's head and each fact to recover the
//! scope that wrote it. A rule's body literals carry no origin
//! of their own (`modules::set_origin` only marks heads and facts), so
//! they are read structurally instead: a literal of `attr(input, ...)`,
//! `attr(output, ...)` or `world(...)`, a call to
//! an extern, or (by predicate name) a read of the scope's own input.

use crate::ast::{Lit, Program, Span, Stmt};
use crate::diag;
use crate::inputs::Declared;
use crate::modules;
use crate::partition::{self, const_str};
use crate::schema::Schema;
use crate::transform;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The stack's own scope name, used when a statement carries no origin.
pub const STACK: &str = "stack";

/// What a scope reads, writes and offers, each a short, deterministic
/// string (sorted in the set, so the printer and the golden need not
/// sort again). `offers` is name -> its declared type text.
#[derive(Debug, Clone, Default)]
pub struct ScopeEffects {
    pub reads: BTreeSet<String>,
    /// A secret it needs by name (`env.var("TOKEN")`, R-155).
    pub needs: BTreeSet<String>,
    pub writes: BTreeSet<String>,
    pub offers: BTreeMap<String, String>,
    /// Each guarded provider the scope uses (R-104), and where its
    /// clause holds: the combinations of the enum inputs it reads, or the
    /// clause as written when it reads more.
    pub uses: BTreeSet<String>,
}

/// Every scope's effects of `program`: the stack, each module instance
/// and each pack `use`d.
pub fn compute(program: &Program, schema: &Schema) -> Result<BTreeMap<String, ScopeEffects>> {
    let mut out: BTreeMap<String, ScopeEffects> = BTreeMap::new();
    out.entry(STACK.to_string()).or_default().uses = guarded_providers(program);
    collect_offers(program, &mut out);

    let compiled = partition::compile(program, &schema.facts)?;
    let inputs_by_scope = input_names(&compiled.inputs);
    let externs: BTreeSet<&str> = compiled.externs.iter().map(|e| e.pred.as_str()).collect();
    let relations = relations(program);

    for a in &compiled.facts {
        let scope = scope_of(a.span);
        let entry = out.entry(scope.clone()).or_default();
        if let Some(w) = classify_write(a).or_else(|| relations.write(&scope, &a.pred)) {
            entry.writes.insert(w);
        }
    }
    for r in &compiled.rules {
        let scope = scope_of(r.head.span);
        {
            let entry = out.entry(scope.clone()).or_default();
            if let Some(w) =
                classify_write(&r.head).or_else(|| relations.write(&scope, &r.head.pred))
            {
                entry.writes.insert(w);
            }
        }
        for lit in &r.body {
            let a = match lit {
                Lit::Pos(a) | Lit::Not(a) => a,
                _ => continue,
            };
            if externs.contains(a.pred.as_str()) {
                if let Some((effect, what)) = coeffect(a, &r.body) {
                    let entry = out.entry(scope.clone()).or_default();
                    match effect {
                        Coeffect::Reads => entry.reads.insert(what),
                        Coeffect::Needs => entry.needs.insert(what),
                    };
                }
                continue;
            }
            if let Some(rd) = classify_read(&scope, a, &inputs_by_scope)
                .or_else(|| relations.read(&scope, &a.pred))
                .or_else(|| rows_read(&scope, a))
            {
                out.entry(scope.clone()).or_default().reads.insert(rd);
            }
        }
    }
    Ok(out)
}

/// The providers a stack uses under a clause (R-104), each with where it
/// holds: `provider aws when cloud == "aws"` for `use aws { .. } where cloud
/// == "aws"`, a combination of the enum
/// inputs the clause reads per line it holds in; a clause that reads
/// anything else as written.
fn guarded_providers(program: &Program) -> BTreeSet<String> {
    let space = crate::lint::enum_inputs(program);
    let mut out = BTreeSet::new();
    for s in &program.statements {
        let Stmt::Rule(r) = s else { continue };
        let (crate::modules::DECLARED, [crate::ast::Term::Val(crate::value::Value::Str(name)), _]) =
            (r.head.pred.as_str(), r.head.args.as_slice())
        else {
            continue;
        };
        // A provider's `use` is the group `use NAME` (a module's or a
        // copy's is its bare name): the row says which provider.
        let Some(provider) = name.strip_prefix("use ") else {
            continue;
        };
        let name = format!("provider {provider}");
        let reads = crate::lint::guard_reads(&r.body, &space);
        let held: Option<Vec<String>> = crate::lint::combinations(&reads)
            .into_iter()
            .filter_map(|c| match crate::lint::guard_holds(&r.body, &c) {
                Some(true) => Some(Some(crate::lint::combination_text(&c))),
                Some(false) => None,
                None => Some(None),
            })
            .collect();
        match held {
            Some(cs) if cs.is_empty() => {
                out.insert(format!("{name} never"));
            }
            Some(cs) => out.extend(cs.into_iter().map(|c| format!("{name} when {c}"))),
            None => {
                let body: Vec<String> = r.body.iter().map(partition::fmt_lit).collect();
                out.insert(format!("{name} where {}", body.join(", ")));
            }
        }
    }
    out
}

/// The relations that cross a scope's edge (R-55), by the copy's scope:
/// those it takes (`input p`, its user gives the rows) and those it
/// exports (`output p`, its user reads them). A copy's relation `p` is
/// the predicate `SCOPE::p`.
#[derive(Default)]
struct Relations {
    inputs: BTreeMap<String, BTreeSet<String>>,
    outputs: BTreeMap<String, BTreeSet<String>>,
}

impl Relations {
    fn split(pred: &str) -> Option<(&str, &str)> {
        pred.rsplit_once("::")
    }

    /// Rows `scope` gives another scope's relation input: `rows S.p`.
    fn write(&self, scope: &str, pred: &str) -> Option<String> {
        let (s, p) = Self::split(pred)?;
        (s != scope && self.inputs.get(s).is_some_and(|ps| ps.contains(p)))
            .then(|| format!("rows {s}.{p}"))
    }

    /// A relation that crosses into `scope`: its own relation input
    /// (`input p`), or another scope's exported relation (`output S.p`).
    fn read(&self, scope: &str, pred: &str) -> Option<String> {
        let (s, p) = Self::split(pred)?;
        if s == scope && self.inputs.get(s).is_some_and(|ps| ps.contains(p)) {
            return Some(format!("input {p}"));
        }
        (s != scope && self.outputs.get(s).is_some_and(|ps| ps.contains(p)))
            .then(|| format!("output {s}.{p}"))
    }
}

/// Another scope's exported relation read through its rows
/// (`modules::ROWS`, `__rows("blue", "made", [..])`): `output S.p`.
fn rows_read(scope: &str, a: &crate::ast::Atom) -> Option<String> {
    if a.pred != modules::ROWS {
        return None;
    }
    let (s, p) = (const_str(a.args.first()?)?, const_str(a.args.get(1)?)?);
    let s = canon_scope(&s);
    (s != scope).then(|| format!("output {s}.{p}"))
}

/// Each `instance` and `use`'s relation inputs and exported relations,
/// from its component's or module's interface.
fn relations(program: &Program) -> Relations {
    let mut defs = Vec::new();
    fn definitions<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a crate::ast::Module>) {
        for s in stmts {
            if let Stmt::Module(m) = s {
                out.push(m);
                definitions(&m.body, out);
            }
        }
    }
    definitions(&program.statements, &mut defs);
    let mut out = Relations::default();
    for s in &program.statements {
        let (Stmt::Instance(u) | Stmt::Use(u)) = s else {
            continue;
        };
        let Some(m) = defs.iter().find(|m| m.name == u.module) else {
            continue;
        };
        for st in &m.body {
            match st {
                Stmt::RelationInput(e) => {
                    out.inputs
                        .entry(u.name.clone())
                        .or_default()
                        .insert(e.pred.clone());
                }
                Stmt::Output(o) if o.value.is_none() && o.ty.is_none() => {
                    out.outputs
                        .entry(u.name.clone())
                        .or_default()
                        .insert(o.name.clone());
                }
                _ => {}
            }
        }
    }
    out
}

/// `""` (the stack's own) prints as `stack`; anything else is already
/// `module.instance` or a pack's name.
fn canon_scope(s: &str) -> String {
    if s.is_empty() {
        STACK.to_string()
    } else {
        s.to_string()
    }
}

/// The scope a rule's head or a fact was lowered out of: `resource C N`
/// becomes `N`, `use M` the name it binds (`M`'s last segment, or its
/// `as`), untagged is the stack's own.
fn scope_of(span: Span) -> String {
    match diag::origin(span) {
        None => STACK.to_string(),
        Some(o) => {
            if let Some(rest) = o.strip_prefix("resource ") {
                return rest.rsplit(' ').next().unwrap_or(rest).to_string();
            }
            match o.strip_prefix("use ") {
                Some(rest) => match rest.split_once(" as ") {
                    Some((_, name)) => name.to_string(),
                    None => rest.rsplit('.').next().unwrap_or(rest).to_string(),
                },
                None => o,
            }
        }
    }
}

/// Declared input names, by scope (`""` canonicalized to `stack`).
fn input_names(declared: &[Declared]) -> BTreeMap<String, BTreeSet<String>> {
    let mut m: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in declared {
        m.entry(canon_scope(&d.scope))
            .or_default()
            .insert(d.decl.name.clone());
    }
    m
}

/// A write a head atom makes: a resource/world cell `(T, P)`, or another
/// (or its own) instance's input cell
/// `input SCOPE.K`. An output is not a write here: it is an offer
/// (`collect_offers`), from the declaration, whether or not a rule ever
/// gives it a value.
fn classify_write(a: &crate::ast::Atom) -> Option<String> {
    match (a.pred.as_str(), a.args.len()) {
        ("arg" | "arg_add", 5) => {
            let typ = const_str(&a.args[0]);
            match typ.as_deref() {
                Some(modules::INPUT) => {
                    let target = canon_scope(&const_str(&a.args[1])?);
                    let key = const_str(&a.args[2])?;
                    Some(format!("input {target}.{key}"))
                }
                Some(transform::OUTPUT) => None,
                _ => {
                    let path = const_str(&a.args[2]).map(|p| partition::normalize_path(&typ, &p));
                    let t = typ.as_deref().unwrap_or("*");
                    let p = path.as_deref().unwrap_or("*");
                    Some(format!("({t}, {p})"))
                }
            }
        }
        // The stack's own input, before it collapses into arg(input, ...):
        // `input(K, V)` (a `--set` fact, a default, or a `set`).
        ("input", 2) => {
            let key = const_str(&a.args[0])?;
            Some(format!("input {STACK}.{key}"))
        }
        _ => None,
    }
}

/// A read a body literal makes, attributed to `scope` (the reading
/// rule's own scope, since a body literal carries no origin of its own):
/// an input by name, a world type, an extern, another
/// instance's output (`output m.i.k`; its own is not "another instance's"
/// and is left out), or, read like a resource attribute but not one of
/// those pseudo-types, `type.path` (a resource the scope reads, its own
/// included: `iam.policy.id` read by a rule of `iam.role_policy_attachment`
/// inside module `iam` is still worth seeing). `None` for anything else.
fn classify_read(
    scope: &str,
    a: &crate::ast::Atom,
    inputs_by_scope: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    match (a.pred.as_str(), a.args.len()) {
        ("attr" | "attr_stuck", 4) | ("attr_conflict", 5) => {
            let typ = const_str(&a.args[0]);
            match typ.as_deref() {
                Some(modules::INPUT) => {
                    let target = canon_scope(&const_str(&a.args[1])?);
                    let key = const_str(&a.args[2])?;
                    (target == scope).then(|| format!("input {key}"))
                }
                Some(transform::OUTPUT) => {
                    let target = canon_scope(&const_str(&a.args[1])?);
                    let key = const_str(&a.args[2])?;
                    (target != scope).then(|| format!("output {target}.{key}"))
                }
                Some(t) => {
                    let path = const_str(&a.args[2]).map(|p| partition::normalize_path(&typ, &p));
                    Some(format!("{t}.{}", path.as_deref().unwrap_or("*")))
                }
                None => None,
            }
        }
        ("world", n) if n >= 1 => {
            let t = const_str(&a.args[0])?;
            Some(format!("world {t}"))
        }
        // A module's own input, read through its renamed reader
        // (`m.i::k`, or `m.i.k` exported; bare `k` at the stack root).
        (pred, 1) => {
            let names = inputs_by_scope.get(scope)?;
            names
                .iter()
                .find(|name| {
                    *pred == **name
                        || *pred == format!("{scope}::{name}")
                        || *pred == format!("{scope}.{name}")
                })
                .map(|name| format!("input {name}"))
        }
        _ => None,
    }
}

/// A coeffect's kind (R-155): a read the context satisfies, or a secret
/// it must be given.
enum Coeffect {
    Reads,
    Needs,
}

/// The coeffect an extern's call is, by kind and what grants it: a
/// location by scheme and host (`ssh://*`, `git+https://github.com`) or a
/// project file by its path (`file:config/*.yaml`), a provider's data
/// source (`provider ovh`), a secret by name (`env.var`), the clock, a
/// memo key; `None` for a table read from a value the program has.
fn coeffect(a: &crate::ast::Atom, body: &[Lit]) -> Option<(Coeffect, String)> {
    // A term as a pattern: a constant as it is, a hole `*`.
    let glob = |t: &crate::ast::Term| -> String {
        match t {
            crate::ast::Term::Var(v) => body
                .iter()
                .find_map(|l| match l {
                    Lit::Eq(crate::ast::Term::Var(x), u) | Lit::Eq(u, crate::ast::Term::Var(x))
                        if x == v =>
                    {
                        Some(pattern(u))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "*".to_string()),
            t => pattern(t),
        }
    };
    let pred = a.pred.as_str();
    if let Some((format, _)) = pred.strip_prefix("table.").and_then(|r| r.split_once('.')) {
        if format == crate::tables::VALUE {
            return None;
        }
        return Some((Coeffect::Reads, location(&glob(a.args.first()?))));
    }
    let first = || a.args.first().map(&glob).unwrap_or_else(|| "*".to_string());
    Some(match pred {
        crate::syntax::resolve::ENV_VAR => (Coeffect::Needs, format!("secret {}", first())),
        crate::externs::TIME_NOW => (Coeffect::Reads, "clock".to_string()),
        crate::memo::FIRST => (Coeffect::Reads, format!("memo {}", first())),
        p => (
            Coeffect::Reads,
            format!("provider {}", p.split_once('.').map_or(p, |(h, _)| h)),
        ),
    })
}

/// A term as a pattern of its values: a string as it is, an
/// interpolation's holes `*`, anything else `*`.
fn pattern(t: &crate::ast::Term) -> String {
    use crate::ast::Term;
    match t {
        Term::Val(crate::value::Value::Str(s)) => s.clone(),
        Term::Func { name, args } if name == crate::ir::FORMAT => match args.first() {
            Some(Term::Val(crate::value::Value::Str(t))) => t.replace("%s", "*"),
            _ => "*".to_string(),
        },
        _ => "*".to_string(),
    }
}

/// A location's grant (R-153): a uri by its scheme and host, its host `*`
/// when computed (`ssh://*`), a project file by its path, `file:PATH`.
fn location(glob: &str) -> String {
    let Some((scheme, rest)) = glob.split_once("://") else {
        return match glob.split_once(':') {
            Some((scheme @ ("data" | "file" | "git+file"), _)) => format!("{scheme}:"),
            _ => format!("file:{glob}"),
        };
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match host.contains('*') || host.is_empty() {
        true => format!("{scheme}://*"),
        false => format!("{scheme}://{host}"),
    }
}

/// An output's type as `effects` offers it; `relation/N` for `output p`
/// of a relation (R-55).
fn offered(o: &crate::ast::OutputDecl) -> String {
    match (&o.ty, &o.relation) {
        (Some(t), _) => crate::inputs::type_text(t),
        (None, Some(cols)) => format!("relation/{}", cols.len()),
        (None, None) => "?".into(),
    }
}

/// Declared outputs, by scope: the stack root's own and, per `instance`
/// or `use`, its component's or module's. Collected from the program before `modules::expand`
/// (which drops the interface once it has scoped the body), so an output
/// that is declared but never given a value is still offered.
fn collect_offers(program: &Program, out: &mut BTreeMap<String, ScopeEffects>) {
    let mut module_outputs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut defs = Vec::new();
    fn definitions<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a crate::ast::Module>) {
        for s in stmts {
            if let Stmt::Module(m) = s {
                out.push(m);
                definitions(&m.body, out);
            }
        }
    }
    definitions(&program.statements, &mut defs);
    for m in defs {
        let mut outs = Vec::new();
        for st in &m.body {
            if let Stmt::Output(o) = st
                && o.value.is_none()
            {
                let ty = offered(o);
                outs.push((o.name.clone(), ty));
            }
        }
        module_outputs.insert(m.name.clone(), outs);
    }
    for s in &program.statements {
        match s {
            Stmt::Output(o) if o.value.is_none() => {
                let ty = offered(o);
                out.entry(STACK.to_string())
                    .or_default()
                    .offers
                    .insert(o.name.clone(), ty);
            }
            Stmt::Instance(u) | Stmt::Use(u) => {
                let scope = u.name.clone();
                if let Some(outs) = module_outputs.get(&u.module) {
                    let entry = out.entry(scope).or_default();
                    for (k, t) in outs {
                        entry.offers.insert(k.clone(), t.clone());
                    }
                }
            }
            _ => {}
        }
    }
}
