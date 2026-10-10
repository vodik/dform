//! An aggregate rule (`count`, `sum`, `min`, `max`, `any`, `all`, `collect_set`,
//! `collect_list`): its body's rows grouped by the head's key, each group folded, Rule 2 and
//! 3 for a group.

use super::body::{Choice, Derived, Row, Src, cmp_order, eval_body, read_pattern};
use super::builtins::{eval_term, order};
use super::collapse::obj;
use super::nulls::Rec;
use super::ops::{self, AggKind};
use super::store::TupleId;
use crate::ast::{Atom, Lit, RuleStmt, Term};
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

    let mut groups = Groups {
        rule,
        idx,
        kind,
        name,
        rec,
        set: BTreeMap::new(),
        list: BTreeMap::new(),
        prov: BTreeMap::new(),
    };
    for row in eval_body(&rule.body, src, rec)? {
        groups.add(row, &item_term)?;
    }
    stuck_reads(rule, rec);
    let undetermined: Vec<Atom> = rec.found.borrow().iter().map(|s| s.head.clone()).collect();
    Ok(groups.into_derived(&undetermined, src))
}

/// Rule 3: a stuck head of a body predicate makes the groups it could
/// feed undetermined.
fn stuck_reads(rule: &RuleStmt, rec: &Rec) {
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
}

/// An aggregate rule's groups as its body's rows arrive: by the head's key
/// (every column but the aggregated one), each group's items, and its Σ.
struct Groups<'a> {
    rule: &'a RuleStmt,
    /// The aggregated column.
    idx: usize,
    kind: AggKind,
    name: &'a str,
    rec: &'a Rec<'a>,
    set: BTreeMap<Vec<Value>, BTreeSet<Value>>,
    /// `collect_list` keeps each item's row order (`cmp_order`).
    list: BTreeMap<Vec<Value>, Vec<(Vec<Choice>, Value)>>,
    /// Σ over the group: every body fact and negation of every member.
    prov: BTreeMap<Vec<Value>, (BTreeSet<TupleId>, BTreeSet<Atom>)>,
}

impl Groups<'_> {
    /// Add a row's item to its group; Rule 2 for its key and its item.
    fn add(&mut self, row: Row, item_term: &Term) -> Result<()> {
        let (rule, idx, rec) = (self.rule, self.idx, self.rec);
        let Row {
            s: b,
            used,
            absent,
            order,
        } = row;
        if rec.any_blocked(&rule.head.args, &b) {
            return Ok(());
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
        let item = eval_term(item_term, &b).ok_or_else(|| anyhow!("non-ground collect item"))?;
        if !key_nulls.is_empty() {
            rec.stuck(&b, key_nulls, "aggregate group key carries a null");
            return Ok(());
        }
        if !matches!(self.kind, AggKind::Set | AggKind::List) && stuck::has_null(&item) {
            rec.stuck(&b, nulls_in(&item), format!("{} over a null", self.name));
            return Ok(());
        }
        let prov = self.prov.entry(key.clone()).or_default();
        prov.0.extend(used);
        prov.1.extend(absent);
        match self.kind {
            AggKind::Set => {
                self.set.entry(key).or_default().insert(item);
            }
            _ => {
                self.list.entry(key).or_default().push((order, item));
            }
        }
        Ok(())
    }

    /// Every group's head, but those `undetermined` unifies with.
    fn into_derived(mut self, undetermined: &[Atom], src: &Src) -> Vec<Derived> {
        let mut out = Vec::new();
        for (key, items) in std::mem::take(&mut self.set) {
            self.emit(key, items.into_iter().collect(), undetermined, &mut out);
        }
        for (key, mut items) in std::mem::take(&mut self.list) {
            // `collect_list` is in the order of the body's rows: the first
            // relation's rows in order (a list's elements in its order), then
            // the next's. The other folds see their items sorted.
            if self.kind == AggKind::List {
                items.sort_by(|(a, x), (b, y)| cmp_order(src.store, a, b).then_with(|| x.cmp(y)));
            } else {
                items.sort_by(|(_, x), (_, y)| x.cmp(y));
            }
            self.emit(
                key,
                items.into_iter().map(|(_, v)| v).collect(),
                undetermined,
                &mut out,
            );
        }
        out
    }

    /// The group `key` folded: its head, or the deny naming what is wrong
    /// with its items.
    fn emit(
        &mut self,
        key: Vec<Value>,
        items: Vec<Value>,
        undetermined: &[Atom],
        out: &mut Vec<Derived>,
    ) {
        let (rule, idx) = (self.rule, self.idx);
        let (used, absent) = self.prov.remove(&key).unwrap_or_default();

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
        let head = match fold(self.name, self.kind, items) {
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
                        ("rule", Value::Str(self.rec.text.to_string())),
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
    }
}

/// A group's aggregated value, from its items sorted; `Err` with what is
/// wrong with them.
fn fold(name: &str, kind: AggKind, items: Vec<Value>) -> std::result::Result<Value, String> {
    match kind {
        AggKind::Set | AggKind::List => Ok(Value::List(items)),
        AggKind::Count => Ok(Value::Int(items.len() as i64)),
        AggKind::Sum if matches!(items.first(), Some(Value::Quantity(_))) => {
            sum_quantities(name, &items)
        }
        AggKind::Sum if items.iter().any(|v| matches!(v, Value::Float(_))) => {
            sum_floats(name, &items)
        }
        AggKind::Min | AggKind::Max
            if matches!(items.first(), Some(Value::Quantity(_) | Value::Time(_)))
                || items.iter().any(|v| matches!(v, Value::Float(_))) =>
        {
            extreme_by_order(name, kind, &items)
        }
        AggKind::Sum => sum_ints(name, &items),
        AggKind::Min | AggKind::Max => extreme_sorted(name, kind, &items),
        AggKind::Any | AggKind::All => any_all(name, kind, &items),
    }
}

/// A sum of quantities is of their dimension (R-66).
fn sum_quantities(name: &str, items: &[Value]) -> std::result::Result<Value, String> {
    let mut total: Option<crate::quantity::Quantity> = None;
    for v in items {
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

/// A sum with a float in it is a float (R-75).
fn sum_floats(name: &str, items: &[Value]) -> std::result::Result<Value, String> {
    let mut total = 0f64;
    for v in items {
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

/// A sum of ints, which does not overflow.
fn sum_ints(name: &str, items: &[Value]) -> std::result::Result<Value, String> {
    let mut total = 0i64;
    for v in items {
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

/// The least and greatest quantity or time (R-66, R-62), in its
/// dimension's order, and number with a float among them by value.
fn extreme_by_order(
    name: &str,
    kind: AggKind,
    items: &[Value],
) -> std::result::Result<Value, String> {
    let mut best = items[0].clone();
    for v in &items[1..] {
        let o = order(v, &best).map_err(|e| format!("{name}() over {e}"))?;
        if (kind == AggKind::Min && o.is_lt()) || (kind == AggKind::Max && o.is_gt()) {
            best = v.clone();
        }
    }
    Ok(best)
}

/// The least or greatest of ints or of strings: sorted, kind first, every
/// item is of one kind iff the first and the last are.
fn extreme_sorted(
    name: &str,
    kind: AggKind,
    items: &[Value],
) -> std::result::Result<Value, String> {
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

/// `any` or `all` of bools.
fn any_all(name: &str, kind: AggKind, items: &[Value]) -> std::result::Result<Value, String> {
    let mut bools = Vec::with_capacity(items.len());
    for v in items {
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
