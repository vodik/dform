//! Unification: a body atom's pattern matched against a fact, a term bound to a value,
//! and an atom grounded or instantiated under a row's bindings.

use super::builtins::eval_term;
use super::nulls::Rec;
use crate::ast::{Atom, Term};
use crate::value::Value;
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};

pub(super) fn unify_atom(
    pattern: &Atom,
    fact: &Atom,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<HashMap<String, Value>>> {
    if pattern.args.len() != fact.args.len() {
        return Ok(None);
    }
    let mut out = state.clone();
    for (p, f) in pattern.args.iter().zip(&fact.args) {
        let Term::Val(fv) = f else {
            bail!("internal: non-ground fact");
        };
        if !unify_term(p, fv, &mut out, rec)? {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

pub(super) fn unify_term(
    pat: &Term,
    fv: &Value,
    out: &mut HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    match pat {
        Term::Val(v) => Ok(rec.eq(v, fv, out, "unification")),
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(rec.eq(bound, fv, out, "unification"))
            } else {
                out.insert(name.clone(), fv.clone());
                Ok(true)
            }
        }
        Term::Wildcard => Ok(true),
        Term::List(items) => unify_list(items, fv, out, rec),
        Term::Obj(m) => unify_obj(m, fv, out, rec),
        // Special pattern unification for scoped(Scope, LocalName).
        // This allows rules to join on component-scoped resources while still
        // binding LocalName variables.
        Term::Func { name, args } if name == crate::ir::SCOPED && args.len() == 2 => {
            unify_scoped(&args[0], &args[1], fv, out, rec)
        }
        // `ref(T, A, P)` as a pattern takes a reference apart (R-42):
        // `deformation(k, ref("aws.vpc", A, ""), _)` binds `A`.
        Term::Func { name, args }
            if name == crate::ir::REF
                && args.len() == 3
                && let Value::Ref { typ, name, attr } = fv
                && eval_term(pat, out).is_none() =>
        {
            let mut tmp = out.clone();
            for (t, v) in args.iter().zip([typ, name, attr]) {
                if !unify_term(t, &Value::Str(v.clone()), &mut tmp, rec)? {
                    return Ok(false);
                }
            }
            *out = tmp;
            Ok(true)
        }
        Term::Func { .. } => {
            let pv = match eval_term(pat, out) {
                Some(v) => v,
                None => return Ok(false),
            };
            Ok(rec.eq(&pv, fv, out, "unification"))
        }
        Term::ListComp { .. } => {
            // Comprehensions must be lowered before evaluation.
            Ok(false)
        }
    }
}

/// A tuple pattern against a list of its length, element by element;
/// `out` binds only when every element unifies.
fn unify_list(
    items: &[Term],
    fv: &Value,
    out: &mut HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    let Value::List(vs) = fv else {
        return Ok(false);
    };
    if items.len() != vs.len() {
        return Ok(false);
    }
    let mut tmp = out.clone();
    for (t, v) in items.iter().zip(vs) {
        if !unify_term(t, v, &mut tmp, rec)? {
            return Ok(false);
        }
    }
    *out = tmp;
    Ok(true)
}

/// An object pattern against an object of its keys, field by field;
/// `out` binds only when every field unifies.
fn unify_obj(
    m: &BTreeMap<String, Term>,
    fv: &Value,
    out: &mut HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    let Value::Obj(vm) = fv else {
        return Ok(false);
    };
    if m.len() != vm.len() {
        return Ok(false);
    }
    let mut tmp = out.clone();
    for (k, t) in m {
        let Some(v) = vm.get(k) else {
            return Ok(false);
        };
        if !unify_term(t, v, &mut tmp, rec)? {
            return Ok(false);
        }
    }
    *out = tmp;
    Ok(true)
}

/// `scoped(Scope, LocalName)` against a value: an address in the scope
/// binds its local name.
fn unify_scoped(
    scope: &Term,
    local: &Term,
    fv: &Value,
    out: &mut HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    let Some(Value::Str(scope)) = eval_term(scope, out) else {
        return Ok(false);
    };
    let Value::Str(full) = fv else {
        return Ok(false);
    };
    let prefix = crate::ir::scoped(&scope, "");
    if let Some(suffix) = full.strip_prefix(&prefix)
        && !crate::ir::is_scoped(suffix)
    {
        let mut tmp = out.clone();
        if unify_term(local, &Value::Str(suffix.to_string()), &mut tmp, rec)? {
            *out = tmp;
            return Ok(true);
        }
    }
    // As the function: a bound name that is already an address
    // is itself (R-65), another copy's resource read through
    // its output. An unbound one ranges over the scope's own.
    Ok(crate::ir::is_scoped(full) && eval_term(local, out).is_some_and(|v| v == *fv))
}

/// A negated atom's pattern: every argument bound, but a wildcard, which
/// matches anything (`not p(x, _)`: no `p` row with `x` first).
pub(super) fn ground_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        if matches!(t, Term::Wildcard) {
            args.push(Term::Wildcard);
            continue;
        }
        let v = eval_term(t, state).ok_or_else(|| anyhow!("unbound var in negation"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
        span: Default::default(),
    })
}

pub(super) fn instantiate_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        let v = eval_term(t, state).ok_or_else(|| anyhow!("non-ground head"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
        span: Default::default(),
    })
}

/// `t = v` with `t` unbound: a variable binds, a tuple pattern `[A, _]`
/// unifies element by element with a list of its length (R-58).
pub(super) fn bind_term(
    t: &Term,
    v: Value,
    out: &mut HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    match t {
        Term::List(_) => unify_term(t, &v, out, rec),
        // `ref(T, A, "") = V` takes a reference apart (`through`).
        Term::Func { name, .. } if name == crate::ir::REF => unify_term(t, &v, out, rec),
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(bound == &v)
            } else {
                out.insert(name.clone(), v);
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}
