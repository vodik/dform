//! `dform dev effects TARGET` (DESIGN.org R-11c): what each scope reads,
//! writes and offers, read off the lowered program and the partition
//! graph's path normalization. No evaluation: a variable-typed write
//! (a non-constant type or path) prints `*` rather than guess what it
//! would be once evaluated.
//!
//! A scope is the stack, a module instance (`module.instance`) or a policy
//! pack `use`d. `modules::expand` already tags every statement it lowers
//! out of an instance or a pack with an origin (`diag::origin`); `compute`
//! reads that tag back off each rule's head and each fact to recover the
//! scope that wrote it. A rule's body literals carry no origin
//! of their own (`modules::set_origin` only marks heads and facts), so
//! they are read structurally instead: a literal of `attr(input, ...)`,
//! `attr(settings, ...)`, `attr(output, ...)` or `world(...)`, a call to
//! an extern, or (by predicate name) a read of the scope's own input.

use crate::ast::{Lit, Program, Span, Stmt, Term};
use crate::diag;
use crate::inputs::Declared;
use crate::modules;
use crate::partition;
use crate::schema::Schema;
use crate::transform;
use crate::value::Value;
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
    pub writes: BTreeSet<String>,
    pub offers: BTreeMap<String, String>,
}

/// Every scope's effects of `program`: the stack, each module instance
/// and each pack `use`d.
pub fn compute(program: &Program, schema: &Schema) -> Result<BTreeMap<String, ScopeEffects>> {
    let mut out: BTreeMap<String, ScopeEffects> = BTreeMap::new();
    out.entry(STACK.to_string()).or_default();
    collect_offers(program, &mut out);

    let compiled = partition::compile(program, &schema.facts)?;
    let inputs_by_scope = input_names(&compiled.inputs);
    let externs: BTreeSet<&str> = compiled.externs.iter().map(|e| e.pred.as_str()).collect();

    for a in &compiled.facts {
        let scope = scope_of(a.span);
        let entry = out.entry(scope).or_default();
        if let Some(w) = classify_write(a) {
            entry.writes.insert(w);
        }
    }
    for r in &compiled.rules {
        let scope = scope_of(r.head.span);
        {
            let entry = out.entry(scope.clone()).or_default();
            if let Some(w) = classify_write(&r.head) {
                entry.writes.insert(w);
            }
        }
        for lit in &r.body {
            let a = match lit {
                Lit::Pos(a) | Lit::Not(a) => a,
                _ => continue,
            };
            if let Some(rd) = classify_read(&scope, a, &inputs_by_scope, &externs) {
                out.entry(scope.clone()).or_default().reads.insert(rd);
            }
        }
    }
    Ok(out)
}

fn const_str(t: &Term) -> Option<String> {
    match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
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

/// The scope a rule's head or a fact was lowered out of: `module M
/// instance I` becomes `M.I`, `policy P` becomes `P`, untagged is the
/// stack's own.
fn scope_of(span: Span) -> String {
    match diag::origin(span) {
        None => STACK.to_string(),
        Some(o) => {
            if let Some(rest) = o.strip_prefix("module ") {
                return match rest.split_once(" instance ") {
                    Some((m, i)) => format!("{m}.{i}"),
                    None => rest.to_string(),
                };
            }
            o.strip_prefix("policy ").map(str::to_string).unwrap_or(o)
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

/// A write a head atom makes: a resource/world cell `(T, P)`, a settings
/// leaf `settings.P`, or another (or its own) instance's input cell
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
                Some(transform::SETTINGS) => {
                    let path = const_str(&a.args[2])?;
                    Some(format!("settings.{path}"))
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
/// an input by name, a settings leaf, a world type, an extern, another
/// instance's output (`output m.i.k`; its own is not "another instance's"
/// and is left out), or, read like a resource attribute but not one of
/// those pseudo-types, `type.path` (a resource the scope reads, its own
/// included: `iam.policy.id` read by a rule of `iam.role_policy_attachment`
/// inside module `iam` is still worth seeing). `None` for anything else.
fn classify_read(
    scope: &str,
    a: &crate::ast::Atom,
    inputs_by_scope: &BTreeMap<String, BTreeSet<String>>,
    externs: &BTreeSet<&str>,
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
                Some(transform::SETTINGS) => {
                    let path = const_str(&a.args[2])?;
                    Some(format!("settings.{path}"))
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
        (pred, _) if externs.contains(pred) => Some(format!("extern {pred}")),
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

/// Declared outputs, by scope: the stack root's own and, per `instance`,
/// its module's. Collected from the program before `modules::expand`
/// (which drops the interface once it has scoped the body), so an output
/// that is declared but never given a value is still offered.
fn collect_offers(program: &Program, out: &mut BTreeMap<String, ScopeEffects>) {
    let mut module_outputs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for s in &program.statements {
        let Stmt::Module(m) = s else { continue };
        let mut outs = Vec::new();
        for st in &m.body {
            if let Stmt::Output(o) = st
                && o.value.is_none()
            {
                let ty =
                    o.ty.as_ref()
                        .map(crate::inputs::type_text)
                        .unwrap_or_else(|| "?".into());
                outs.push((o.name.clone(), ty));
            }
        }
        module_outputs.insert(m.name.clone(), outs);
    }
    for s in &program.statements {
        match s {
            Stmt::Output(o) if o.value.is_none() => {
                let ty =
                    o.ty.as_ref()
                        .map(crate::inputs::type_text)
                        .unwrap_or_else(|| "?".into());
                out.entry(STACK.to_string())
                    .or_default()
                    .offers
                    .insert(o.name.clone(), ty);
            }
            Stmt::Instance(u) => {
                let scope = format!("{}.{}", u.module, u.name);
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
