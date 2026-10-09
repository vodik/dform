//! Builtins: a term's value under a row's bindings (a function applied to its parameters),
//! and the builtin literals: `=`, `!=`, an ordering comparison, a builtin predicate.

use super::errors::{
    at_suffix, failed_builtin, missing_walk, not_an_object, ref_and_string, side_unanswered,
    unanswered,
};
use super::nulls::{Rec, forwards_nulls, spreads};
use super::unify::bind_term;
use crate::ast::{Atom, Lit, Term};
use crate::lattice::{Truth, nulls_in};
use crate::spell;
use crate::stuck;
use crate::value::Value;
use anyhow::{Result, anyhow, bail};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A builtin predicate (a function to bool, `str.starts_with(c.image, "ghcr.io/")`)
/// over the row: whether it holds, `None` when that is undetermined. An
/// argument holding a null is a content position (Rule 2): the literal is
/// stuck. A call in an argument that has no value (`oci.with_digest` of a
/// tag) fails the literal, as its binding would.
pub(super) fn eval_builtin_pred(
    atom: &Atom,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<bool>> {
    let mut vals = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        if let Some(v) = eval_term(t, state) {
            vals.push(v);
            continue;
        }
        let mut free = BTreeSet::new();
        unbound(t, state, &mut free);
        if let Some(v) = free.into_iter().next() {
            bail!(
                "`{}(..)`{}: `{}` is not bound here",
                atom.pred,
                at_suffix(atom.span),
                crate::syntax::resolve::source_name(&v)
            );
        }
        if let Some(r) = side_unanswered(t, state, rec) {
            r?;
        }
        return Ok(Some(false));
    }
    if !forwards_nulls(&atom.pred) && vals.iter().any(stuck::has_null) {
        let nulls = vals.iter().flat_map(nulls_in).collect();
        rec.stuck(
            state,
            nulls,
            format!(
                "builtin {} over a null",
                crate::functions::shown_call(&atom.pred)
            ),
        );
        return Ok(None);
    }
    let body = crate::functions::body(&atom.pred)
        .ok_or_else(|| anyhow!("internal: no body for the builtin {}", atom.pred))?;
    match body(&as_params(&atom.pred, vals.clone())) {
        Some(Value::Bool(b)) => Ok(Some(b)),
        Some(other) => bail!(
            "`{}(..)`{} is not true or false: {}",
            atom.pred,
            at_suffix(atom.span),
            spell::value(&other)
        ),
        None if crate::functions::get(&atom.pred).is_some_and(|f| f.partial) => Ok(Some(false)),
        None => bail!(
            "`{}({})`{} is not defined for these arguments",
            atom.pred,
            vals.iter().map(spell::value).collect::<Vec<_>>().join(", "),
            at_suffix(atom.span)
        ),
    }
}

/// The variables of `t` with no value in `state`.
pub(super) fn unbound(t: &Term, state: &HashMap<String, Value>, out: &mut BTreeSet<String>) {
    match t {
        Term::Var(v) if !state.contains_key(v) => {
            out.insert(v.clone());
        }
        Term::Func { args: xs, .. } | Term::List(xs) => {
            xs.iter().for_each(|x| unbound(x, state, out));
        }
        Term::Obj(m) => m.values().for_each(|x| unbound(x, state, out)),
        _ => {}
    }
}

pub(super) fn eval_eq(
    a: &Term,
    b: &Term,
    state: HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<HashMap<String, Value>>> {
    if rec.any_blocked([a, b], &state) {
        return Ok(None);
    }
    let mut out = state;
    match (eval_term(a, &out), eval_term(b, &out)) {
        // Two numbers compare by value, an int with a float (R-75); a
        // join matches a value as it is.
        (Some(av), Some(bv)) => {
            if let Some(e) = ref_and_string(a, &av, "==", b, &bv, rec) {
                return Err(e);
            }
            Ok(match crate::value::compare_numbers(&av, &bv) {
                Some(o) => o.is_eq(),
                None => rec.eq(&av, &bv, &out, "="),
            }
            .then_some(out))
        }
        (Some(av), None) => {
            not_an_object(b, &out, rec)?;
            if bind_term(b, av, &mut out, rec)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, Some(bv)) => {
            not_an_object(a, &out, rec)?;
            if bind_term(a, bv, &mut out, rec)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, None) => {
            for t in [a, b] {
                if missing_walk(t, &out, rec)? {
                    return Ok(None);
                }
                if let Some((name, args)) = failed_builtin(t, &out) {
                    if name == crate::ir::RESOURCE_BODY {
                        let args: Vec<String> = args.iter().map(spell::value).collect();
                        bail!(
                            "`resource T NAME = VALUE` takes an object, a value of the type: not {}{}",
                            args.join(", "),
                            at_suffix(rec.head.span)
                        );
                    }
                    // A partial function's none (`T?`, R-134: a valid input
                    // with no answer) fails the literal, a tuple pattern's
                    // match included (R-58); any other function's is an
                    // error at the rule naming the call.
                    return unanswered(&name, &args, rec).map(|()| None);
                }
            }
            bail!("unsafe equality: both sides unbound")
        }
    }
}

pub(super) fn eval_neq(
    a: &Term,
    b: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    if rec.any_blocked([a, b], state) {
        return Ok(false);
    }
    match (eval_term(a, state), eval_term(b, state)) {
        (Some(av), Some(bv)) => {
            if let Some(e) = ref_and_string(a, &av, "!=", b, &bv, rec) {
                return Err(e);
            }
            Ok(match crate::value::compare_numbers(&av, &bv) {
                Some(o) => !o.is_eq(),
                None => match crate::lattice::eq3(&av, &bv) {
                    Truth::False => true,
                    Truth::True => false,
                    Truth::Unknown => {
                        let mut nulls = nulls_in(&av);
                        nulls.extend(nulls_in(&bv));
                        rec.stuck(state, nulls, "!= against an open/secret null");
                        false
                    }
                },
            })
        }
        (a_v, _) => {
            let t = if a_v.is_none() { a } else { b };
            if let Some(r) = side_unanswered(t, state, rec) {
                return r.map(|()| false);
            }
            bail!("unsafe !=: both sides must be ground")
        }
    }
}

pub(super) fn eval_cmp(
    op_lit: &Lit,
    a: &Term,
    b: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    if rec.any_blocked([a, b], state) {
        return Ok(false);
    }
    let Some(av) = eval_term(a, state) else {
        if let Some(r) = side_unanswered(a, state, rec) {
            return r.map(|()| false);
        }
        bail!("unsafe comparison: left not ground");
    };
    let Some(bv) = eval_term(b, state) else {
        if let Some(r) = side_unanswered(b, state, rec) {
            return r.map(|()| false);
        }
        bail!("unsafe comparison: right not ground");
    };
    // Rule 2: an ordering comparison is a content position.
    if stuck::has_null(&av) || stuck::has_null(&bv) {
        let mut nulls = nulls_in(&av);
        nulls.extend(nulls_in(&bv));
        rec.stuck(state, nulls, "ordering comparison over a null");
        return Ok(false);
    }
    let o = order(&av, &bv).map_err(|e| anyhow::anyhow!(e))?;
    Ok(match op_lit {
        Lit::Gt(_, _) => o.is_gt(),
        Lit::Ge(_, _) => o.is_ge(),
        Lit::Lt(_, _) => o.is_lt(),
        Lit::Le(_, _) => o.is_le(),
        _ => unreachable!(),
    })
}

pub(super) fn eval_term(term: &Term, state: &HashMap<String, Value>) -> Option<Value> {
    match term {
        Term::Val(v) => Some(v.clone()),
        Term::Var(name) => state.get(name).cloned(),
        Term::Wildcard => None,
        Term::Func { name, args } => eval_func(name, args, state),
        Term::List(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(eval_term(x, state)?);
            }
            Some(Value::List(out))
        }
        Term::Obj(m) => {
            let mut out = BTreeMap::new();
            for (k, v) in m {
                out.insert(k.clone(), eval_term(v, state)?);
            }
            Some(Value::Obj(out))
        }
        Term::ListComp { .. } => None,
    }
}

pub(super) fn eval_func(
    name: &str,
    args: &[Term],
    state: &HashMap<String, Value>,
) -> Option<Value> {
    // A path into a bound value is followed in place: the value is not
    // copied to be an argument (`__path` forwards nulls, its path is a
    // string).
    if name == "__path"
        && let [Term::Var(x), path] = args
        && let Some(Value::Str(path)) = eval_term(path, state)
    {
        return crate::functions::path_of(state.get(x)?, &path);
    }
    let body = crate::functions::body(name)?;
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_term(a, state)?);
    }
    // Rule 2: every builtin argument is a content position. A builtin
    // over a null has no value; the literal that needs it is stuck. But a
    // string template over secrets a provider holds, which no evaluation
    // fills: each is in place as its placeholder (R-218).
    if !forwards_nulls(name) && vals.iter().any(stuck::has_null) {
        vals = crate::secrets::held::in_template(name, &vals)?;
    }
    if spreads(name) && vals.iter().any(|v| matches!(v, Value::Null { .. })) {
        return None;
    }
    body(&as_params(name, vals))
}

/// `vals` as the parameters of the function `name` take them: a value type
/// (`oci`, `uri`, `inet`, `ip`, `time`, a quantity) where a `string` is
/// declared is its canonical text, as it is in a string column (R-133):
/// `str.starts_with(c.image, "ghcr.io/")` over an `oci`; a string where a value type
/// is declared is read as one (R-134: `time.format(cert.not_after, ..)`
/// over a string attribute), and left a string when it is not one, which
/// the body answers nothing for.
fn as_params(name: &str, vals: Vec<Value>) -> Vec<Value> {
    let Some(f) = crate::functions::get(name) else {
        return vals;
    };
    let ty = |i: usize| {
        f.params
            .get(i)
            .or_else(|| f.params.last().filter(|_| f.variadic))
            .map(|p| p.ty.as_str())
    };
    vals.into_iter()
        .enumerate()
        .map(|(i, v)| match (ty(i), &v) {
            (
                Some("string"),
                Value::Quantity(_)
                | Value::Time(_)
                | Value::Uri(_)
                | Value::Oci(_)
                | Value::Semver(_)
                | Value::Range(_),
            ) => v.typed_text().map_or(v, Value::Str),
            (Some("string"), Value::Ip(_) | Value::IpNet { .. }) => Value::Str(spell::value(&v)),
            (Some(ty), Value::Str(_)) if crate::value::is_value_type(ty) => {
                crate::value::read_typed(ty, &v).unwrap_or(v)
            }
            _ => v,
        })
        .collect()
}

/// How two values order: numbers by value (an int with a float, R-75),
/// quantities of one dimension, times by their instant; `Err` with why
/// they do not.
pub(crate) fn order(a: &Value, b: &Value) -> std::result::Result<Ordering, String> {
    if let Some(o) = crate::value::compare_numbers(a, b) {
        return Ok(o);
    }
    // A string against a time or a version is read as one, as the other
    // side of an operator gives a quantity literal its type (R-134: `v <
    // "2.0.0"`, `t < "2027-01-01T00:00:00Z"`).
    let typed = |v: &Value| match v {
        Value::Time(_) => Some("time"),
        Value::Semver(_) => Some("semver"),
        Value::Ip(_) => Some("ip"),
        Value::Quantity(q) => Some(q.dim().name()),
        _ => None,
    };
    match (a, b) {
        (Value::Str(_), t) if typed(t).is_some() => {
            let a = crate::value::read_typed(typed(t).unwrap_or_default(), a)?;
            return order(&a, b);
        }
        (t, Value::Str(_)) if typed(t).is_some() => {
            let b = crate::value::read_typed(typed(t).unwrap_or_default(), b)?;
            return order(a, &b);
        }
        _ => {}
    }
    match (a, b) {
        (Value::Time(x), Value::Time(y)) => Ok(x.instant().cmp(&y.instant())),
        (Value::Semver(x), Value::Semver(y)) => Ok(x.cmp(y)),
        (Value::Ip(x), Value::Ip(y)) => Ok(x.cmp(y)),
        (Value::Quantity(x), Value::Quantity(y)) => x.compare(y).ok_or_else(|| {
            if x.dim() == y.dim() {
                format!(
                    "{x} and {y} do not compare: a month's length depends on the date \
                     (add both to a time, `t + d`)"
                )
            } else {
                format!(
                    "{x} is {} and {y} {}: a quantity compares only within its dimension",
                    x.dim().name(),
                    y.dim().name()
                )
            }
        }),
        _ => Err(
            "comparison only supports numbers, quantities, times, versions and addresses"
                .to_string(),
        ),
    }
}
