//! An aggregate rule (`count`, `sum`, `min`, `max`, `any`, `all`, `collect_set`,
//! `collect_list`): its body's rows grouped by the head's key, each group folded, Rule 2 and
//! 3 for a group.

use super::body::{Choice, Derived, Row, Src, cmp_order, eval_body, read_pattern};
use super::builtins::{eval_term, order};
use super::nulls::Rec;
use super::obj;
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::ir::ops::{self, AggKind};
use crate::ir::store::TupleId;
use crate::lattice::nulls_in;
use crate::spell;
use crate::stuck;
use crate::value::Value;
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// An aggregate rule. Rule 2: the group key is a content position, and so
/// is the aggregated value of `count`, `sum`, `min`, `max`, `any` and `all`
/// (a fresh null too: its order and its sum are content);
/// `collect_set`/`collect_list` forward nulls. Rule 3: a group is undetermined, and not derived, when
/// its key unifies with a stuck instance of this rule or with a stuck head
/// of a predicate the body reads.
///
/// `count` and `sum` fold every body match of the group, `min` and `max`
/// their least and greatest, `any` and `all` their bools; `collect_list`
/// keeps the rows' order. A group has at least one match, so an empty
/// group derives nothing. A group `sum` has a non-int in, `min`/`max` one
/// that is neither an int nor a string or a mix of the two, or `any`/`all`
/// a non-bool, derives a deny naming the group and the value instead of
/// its head.
pub(super) fn eval_rule_collect(
    rule: &RuleStmt,
    src: &Src,
    idx: usize,
    kind: AggKind,
    rec: &Rec,
) -> Result<Vec<Derived>> {
    let Term::Func { name, args } = &rule.head.args[idx] else {
        bail!("internal: collect idx not func");
    };
    if args.len() != 1 {
        bail!("collect*(...) must have exactly one argument");
    }
    let item_term = args[0].clone();

    let rows = eval_body(&rule.body, src, rec)?;
    let mut groups_set: BTreeMap<Vec<Value>, BTreeSet<Value>> = BTreeMap::new();
    // `collect_list` keeps each item's row order (`cmp_order`).
    let mut groups_list: BTreeMap<Vec<Value>, Vec<(Vec<Choice>, Value)>> = BTreeMap::new();
    // Σ over the group: every body fact and negation of every member.
    let mut group_prov: BTreeMap<Vec<Value>, (BTreeSet<TupleId>, BTreeSet<Atom>)> = BTreeMap::new();
    for Row {
        s: b,
        used,
        absent,
        order,
    } in rows
    {
        if rec.any_blocked(&rule.head.args, &b) {
            continue;
        }
        let mut key = Vec::new();
        let mut key_nulls = BTreeSet::new();
        for (i, t) in rule.head.args.iter().enumerate() {
            if i == idx {
                continue;
            }
            let v = eval_term(t, &b).ok_or_else(|| anyhow!("non-ground head term"))?;
            key_nulls.extend(nulls_in(&v));
            key.push(v);
        }
        let item = eval_term(&item_term, &b).ok_or_else(|| anyhow!("non-ground collect item"))?;
        if !key_nulls.is_empty() {
            rec.stuck(&b, key_nulls, "aggregate group key carries a null");
            continue;
        }
        if !matches!(kind, AggKind::Set | AggKind::List) && stuck::has_null(&item) {
            rec.stuck(&b, nulls_in(&item), format!("{name} over a null"));
            continue;
        }
        let prov = group_prov.entry(key.clone()).or_default();
        prov.0.extend(used);
        prov.1.extend(absent);
        match kind {
            AggKind::Set => {
                groups_set.entry(key).or_default().insert(item);
            }
            _ => {
                groups_list.entry(key).or_default().push((order, item));
            }
        }
    }

    // Rule 3: a stuck head of a body predicate makes the groups it could
    // feed undetermined.
    for lit in &rule.body {
        let Lit::Pos(a) = lit else { continue };
        if a.pred == "member" || a.pred == "enumerate" || ops::is_builtin_pred(&a.pred) {
            continue;
        }
        let pat = read_pattern(a, &HashMap::new());
        for (p, nulls) in rec.known.borrow().matching(&pat) {
            let mut b = HashMap::new();
            for (t, v) in a.args.iter().zip(&p.args) {
                if let (Term::Var(x), Term::Val(v)) = (t, v) {
                    b.insert(x.clone(), v.clone());
                }
            }
            rec.stuck(
                &b,
                nulls,
                format!("aggregate over {}, which has a stuck instance", a.pred),
            );
        }
    }
    let undetermined: Vec<Atom> = rec.found.borrow().iter().map(|s| s.head.clone()).collect();

    let mut out = Vec::new();
    let mut emit_group = |key: Vec<Value>, items: Vec<Value>| {
        let (used, absent) = group_prov.remove(&key).unwrap_or_default();

        let mut key_pat = Atom {
            pred: rule.head.pred.clone(),
            args: Vec::with_capacity(rule.head.args.len()),
            record: None,
            span: Default::default(),
        };
        let mut k = 0usize;
        for i in 0..rule.head.args.len() {
            if i == idx {
                key_pat.args.push(Term::Wildcard);
            } else {
                key_pat.args.push(Term::Val(key[k].clone()));
                k += 1;
            }
        }
        if undetermined
            .iter()
            .any(|u| stuck::patterns_unify(u, &key_pat))
        {
            return;
        }
        let head = match fold(name, kind, items) {
            Ok(v) => {
                let mut group = key_pat;
                group.args[idx] = Term::Val(v);
                group
            }
            Err(msg) => Atom {
                pred: "deny".into(),
                args: vec![
                    Term::Val(Value::Str(format!("{}: {msg}", spell::atom(&key_pat)))),
                    Term::Val(obj(vec![
                        ("pred", Value::Str(rule.head.pred.clone())),
                        ("group", Value::List(key)),
                        ("rule", Value::Str(rec.text.to_string())),
                    ])),
                ],
                record: None,
                span: rule.head.span,
            },
        };
        let n = out.len() as u32;
        out.push(Derived {
            head,
            used: used.into_iter().collect(),
            absent: absent.into_iter().collect(),
            bindings: vec![],
            order: vec![Choice::Nth(n)],
        });
    };

    for (key, items) in groups_set {
        emit_group(key, items.into_iter().collect());
    }
    for (key, mut items) in groups_list {
        // `collect_list` is in the order of the body's rows: the first
        // relation's rows in order (a list's elements in its order), then
        // the next's. The other folds see their items sorted.
        if kind == AggKind::List {
            items.sort_by(|(a, x), (b, y)| cmp_order(src.store, a, b).then_with(|| x.cmp(y)));
        } else {
            items.sort_by(|(_, x), (_, y)| x.cmp(y));
        }
        emit_group(key, items.into_iter().map(|(_, v)| v).collect());
    }

    Ok(out)
}

/// A group's aggregated value, from its items sorted; `Err` with what is
/// wrong with them.
pub(super) fn fold(
    name: &str,
    kind: AggKind,
    items: Vec<Value>,
) -> std::result::Result<Value, String> {
    match kind {
        AggKind::Set | AggKind::List => Ok(Value::List(items)),
        AggKind::Count => Ok(Value::Int(items.len() as i64)),
        // A sum of quantities is of their dimension (R-66).
        AggKind::Sum if matches!(items.first(), Some(Value::Quantity(_))) => {
            let mut total: Option<crate::quantity::Quantity> = None;
            for v in &items {
                let Value::Quantity(q) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not a quantity",
                        spell::value(v)
                    ));
                };
                total = Some(match total {
                    None => *q,
                    Some(t) => t.checked_add(q).ok_or_else(|| {
                        format!(
                            "{name}() over {t} and {q}, which do not add: {} and {}",
                            t.dim().name(),
                            q.dim().name()
                        )
                    })?,
                });
            }
            Ok(Value::Quantity(total.expect("a group has a row")))
        }
        // A sum with a float in it is a float (R-75).
        AggKind::Sum if items.iter().any(|v| matches!(v, Value::Float(_))) => {
            let mut total = 0f64;
            for v in &items {
                total += match v {
                    Value::Int(n) => *n as f64,
                    Value::Float(f) => f.get(),
                    v => {
                        return Err(format!(
                            "{name}() over {}, which is not a number",
                            spell::value(v)
                        ));
                    }
                };
            }
            crate::value::Float::new(total)
                .map(Value::Float)
                .ok_or_else(|| format!("{name}() overflows"))
        }
        // The least and greatest quantity or time (R-66, R-62), in its
        // dimension's order, and number with a float among them by value.
        AggKind::Min | AggKind::Max
            if matches!(items.first(), Some(Value::Quantity(_) | Value::Time(_)))
                || items.iter().any(|v| matches!(v, Value::Float(_))) =>
        {
            let mut best = items[0].clone();
            for v in &items[1..] {
                let o = order(v, &best).map_err(|e| format!("{name}() over {e}"))?;
                if (kind == AggKind::Min && o.is_lt()) || (kind == AggKind::Max && o.is_gt()) {
                    best = v.clone();
                }
            }
            Ok(best)
        }
        AggKind::Sum => {
            let mut total = 0i64;
            for v in &items {
                let Value::Int(n) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not an int",
                        spell::value(v)
                    ));
                };
                total = total
                    .checked_add(*n)
                    .ok_or_else(|| format!("{name}() overflows at {}", spell::value(v)))?;
            }
            Ok(Value::Int(total))
        }
        AggKind::Min | AggKind::Max => {
            // Sorted, kind first: every item is of one kind iff the first
            // and the last are.
            let (Some(first), Some(last)) = (items.first(), items.last()) else {
                return Err(format!("{name}() over no value"));
            };
            for v in [first, last] {
                if !matches!(v, Value::Int(_) | Value::Str(_)) {
                    return Err(format!(
                        "{name}() over {}, which is neither an int nor a string",
                        spell::value(v)
                    ));
                }
            }
            if std::mem::discriminant(first) != std::mem::discriminant(last) {
                return Err(format!(
                    "{name}() over {} and {}, an int and a string",
                    spell::value(first),
                    spell::value(last)
                ));
            }
            let v = if kind == AggKind::Min { first } else { last };
            Ok(v.clone())
        }
        AggKind::Any | AggKind::All => {
            let mut bools = Vec::with_capacity(items.len());
            for v in &items {
                let Value::Bool(b) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not a bool",
                        spell::value(v)
                    ));
                };
                bools.push(*b);
            }
            Ok(Value::Bool(if kind == AggKind::Any {
                bools.contains(&true)
            } else {
                !bools.contains(&false)
            }))
        }
    }
}
