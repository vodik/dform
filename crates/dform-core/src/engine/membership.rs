//! Membership, `x in e`: a list's or an object's entries enumerated, a scalar's
//! (a string, an `inet`, a range) tested, and their negations.

use super::builtins::eval_term;
use super::errors::{missing_walk, ref_and_string};
use super::nulls::Rec;
use super::unify::unify_term;
use crate::ast::{Atom, Term};
use crate::lattice::{Truth, nulls_in};
use crate::spell;
use crate::stuck;
use crate::value::Value;
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeSet, HashMap};

pub(super) fn eval_member_like(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    match atom.pred.as_str() {
        "member" => {
            if atom.args.len() == 2 {
                return eval_member2(atom, state, out, rec);
            }
            if atom.args.len() == 3 {
                return eval_member3(atom, state, out, rec);
            }
            bail!("member/2 or member/3 expected");
        }
        "enumerate" => {
            if atom.args.len() != 3 {
                bail!("enumerate/3 expected");
            }
            eval_member3(atom, state, out, rec)
        }
        _ => bail!("internal: eval_member_like called for non-member"),
    }
}

pub(super) fn eval_member2(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    if missing_walk(&atom.args[0], state, rec)? {
        return Ok(());
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        // Rule 2: member over a null list is a content position.
        rec.stuck(state, nulls_in(&list_v), "member/2 over a null list");
        return Ok(());
    }
    // `n in r` with `n` unbound: each member of a discrete range, in
    // order (R-180); a bound `n` is tested.
    if let Value::Range(r) = &list_v
        && eval_term(&atom.args[1], state).is_none()
    {
        let members = r.members().map_err(|e| anyhow!("`{}`: {e}", rec.text))?;
        for item in &members {
            let mut s2 = state.clone();
            if unify_term(&atom.args[1], item, &mut s2, rec)? {
                out.push(s2);
            }
        }
        return Ok(());
    }
    if let Some(held) = in_scalar(atom, &list_v, state, rec)? {
        if held {
            out.push(state.clone());
        }
        return Ok(());
    }
    let items = match list_v {
        Value::List(items) => items,
        Value::Obj(_) => bail!(
            "`x in e` over an object, {}, in `{}`: an object's entries are matched by a \
             pattern, `(key, value) in e` (R-58)",
            spell::value(&list_v),
            rec.text
        ),
        _ => bail!("member/2 first argument must be a list"),
    };
    let given = eval_term(&atom.args[1], state);
    for item in &items {
        if let Some(v) = &given
            && let Some(e) = ref_and_string(&atom.args[1], v, "in", &atom.args[0], item, rec)
        {
            return Err(e);
        }
        let mut s2 = state.clone();
        if unify_term(&atom.args[1], item, &mut s2, rec)? {
            out.push(s2);
        }
    }
    Ok(())
}

pub(super) fn eval_not_member2(
    atom: &Atom,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        rec.stuck(state, nulls_in(&list_v), "not member/2 over a null list");
        return Ok(false);
    }
    if let Some(held) = in_scalar(atom, &list_v, state, rec)? {
        return Ok(!held);
    }
    let Value::List(items) = list_v else {
        bail!("member/2 first argument must be a list");
    };
    let item_v = eval_term(&atom.args[1], state)
        .ok_or_else(|| anyhow!("unsafe not member: item is not ground"))?;
    let mut unknown = BTreeSet::new();
    for x in &items {
        if let Some(e) = ref_and_string(&atom.args[1], &item_v, "in", &atom.args[0], x, rec) {
            return Err(e);
        }
        match crate::lattice::eq3(x, &item_v) {
            Truth::True => return Ok(false),
            Truth::Unknown => {
                unknown.extend(nulls_in(x));
                unknown.extend(nulls_in(&item_v));
            }
            Truth::False => {}
        }
    }
    if !unknown.is_empty() {
        rec.stuck(state, unknown, "not member/2: membership undecidable");
        return Ok(false);
    }
    Ok(true)
}

/// `x in e` over a value that is no list (R-155: `in` is the one
/// membership): a substring of a string (`":" in image`), an address of
/// an `inet`, a member of a range (`a in net`, `n in 1..=3`). `None` for a list or an
/// object, which enumerate; `Some(false)` while the item waits on a null
/// (Rule 2), the literal stuck. The item is a test's, never bound here.
pub(super) fn in_scalar(
    atom: &Atom,
    coll: &Value,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<bool>> {
    if matches!(coll, Value::List(_) | Value::Obj(_)) {
        return Ok(None);
    }
    let Some(item) = eval_term(&atom.args[1], state) else {
        bail!(
            "`x in {}` in `{}`: a {} holds a value given, it enumerates none; bind `x` first",
            spell::value(coll),
            rec.text,
            crate::value::type_name(coll)
        );
    };
    if stuck::has_null(&item) {
        rec.stuck(state, nulls_in(&item), "`in` over a null");
        return Ok(Some(false));
    }
    holds(coll, &item)
        .map(Some)
        .map_err(|e| anyhow!("`{}`: {e}", rec.text))
}

/// Whether `coll`, a value that is no list, holds `item` (`in`, R-155):
/// a string a substring, an `inet` an address (a string read as one), a
/// range a value between its ends; an error naming the types `in` takes
/// for anything else.
pub fn holds(coll: &Value, item: &Value) -> std::result::Result<bool, String> {
    use crate::value::{Value as V, type_name};
    let ip = |v: &Value| match v {
        V::Ip(n) => Some(*n),
        V::Str(s) => crate::value::ipv4_to_u32(s),
        _ => None,
    };
    let addr = |what: &str| match ip(item) {
        Some(n) => Ok(n),
        None => Err(format!(
            "{what} holds addresses, and {} is no `ip`",
            spell::value(item)
        )),
    };
    match coll {
        V::Str(s) => match item {
            V::Str(needle) => Ok(s.contains(needle.as_str())),
            _ => Err(format!(
                "a string holds strings, and {} is {}: interpolate it, `\"${{x}}\" in s`",
                spell::value(item),
                crate::value::article(type_name(item))
            )),
        },
        V::IpNet { addr: base, prefix } => {
            let n = addr("a network")?;
            let mask = match prefix {
                0 => 0,
                p => u32::MAX << (32 - *p as u32),
            };
            Ok(n & mask == *base)
        }
        V::Range(r) => r.holds(item),
        v => Err(format!(
            "`in` takes a list, a string, an `inet` or a range, and {} is {}{}",
            spell::value(v),
            crate::value::article(type_name(v)),
            match v {
                V::Oci(_) | V::Uri(_) | V::Time(_) | V::Quantity(_) | V::Semver(_) | V::Ip(_) =>
                    ": its text is `\"${v}\"`",
                _ => "",
            }
        )),
    }
}

/// `not (k, v) in e`: no entry matches; a `_` in either pattern matches
/// anything.
pub(super) fn eval_not_member3(
    atom: &Atom,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    if missing_walk(&atom.args[0], state, rec)? {
        return Ok(true);
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    let entries = entries(list_v)
        .ok_or_else(|| anyhow!("member/3 first argument must be a list or an object"))?;
    for (k, v) in &entries {
        if matches_ground(&atom.args[1], k, state)? && matches_ground(&atom.args[2], v, state)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A list's indexes and elements, or an object's keys and values in key
/// order: what `(k, v) in e` enumerates.
pub(super) fn entries(v: Value) -> Option<Vec<(Value, Value)>> {
    match v {
        Value::List(items) => Some(
            items
                .into_iter()
                .enumerate()
                .map(|(i, x)| (Value::Int(i as i64), x))
                .collect(),
        ),
        Value::Obj(m) => Some(m.into_iter().map(|(k, x)| (Value::Str(k), x)).collect()),
        _ => None,
    }
}

/// Whether a negated pattern matches `v`: `_` anything, a tuple element by
/// element, anything else by its value.
pub(super) fn matches_ground(t: &Term, v: &Value, state: &HashMap<String, Value>) -> Result<bool> {
    match t {
        Term::Wildcard => Ok(true),
        Term::List(ts) => {
            let Value::List(vs) = v else { return Ok(false) };
            if ts.len() != vs.len() {
                return Ok(false);
            }
            for (t, v) in ts.iter().zip(vs) {
                if !matches_ground(t, v, state)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        t => {
            let w = eval_term(t, state)
                .ok_or_else(|| anyhow!("unsafe not member: pattern is not ground"))?;
            Ok(&w == v)
        }
    }
}

pub(super) fn eval_member3(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    if missing_walk(&atom.args[0], state, rec)? {
        return Ok(());
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        rec.stuck(state, nulls_in(&list_v), "member/3 over a null list");
        return Ok(());
    }
    // An object's keys and values (R-58), a list's indexes and elements.
    let entries = entries(list_v)
        .ok_or_else(|| anyhow!("member/3 first argument must be a list or an object"))?;
    for (k, item) in &entries {
        let mut s2 = state.clone();
        if !unify_term(&atom.args[1], k, &mut s2, rec)? {
            continue;
        }
        if !unify_term(&atom.args[2], item, &mut s2, rec)? {
            continue;
        }
        out.push(s2);
    }
    Ok(())
}
