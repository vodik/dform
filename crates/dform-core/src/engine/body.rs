//! A rule's body joined over the store, literal by literal: its rows, the tuples and choices
//! each made (the naive order, provenance), the window a relation read sees, and the heads a
//! rule derives.

use super::aggregate::eval_rule_collect;
use super::builtins::{eval_builtin_pred, eval_cmp, eval_eq, eval_neq, eval_term};
use super::errors::{head_error, reference_at_string_cell};
use super::membership::{eval_member_like, eval_not_member2, eval_not_member3};
use super::negation::eval_not;
use super::nulls::{Rec, Rule3Clause, planted, term_has_bound_null};
use super::unify::{ground_atom, instantiate_atom, unify_atom};
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::ir::ops;
use crate::ir::store::{Store, TupleId, Window};
use crate::lattice::nulls_in;
use crate::stuck;
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Ordering;
use std::collections::HashMap;

/// A body atom as a pattern under `state`: bound terms are values, the
/// rest wildcards.
pub(super) fn read_pattern(atom: &Atom, state: &HashMap<String, Value>) -> Atom {
    Atom {
        pred: atom.pred.clone(),
        args: atom
            .args
            .iter()
            .map(|t| match eval_term(t, state) {
                Some(v) => Term::Val(v),
                None => Term::Wildcard,
            })
            .collect(),
        record: None,
        span: Default::default(),
    }
}

/// One choice a body made on the way to a row: the tuple a relation read
/// matched, or the element a `member`/`enumerate` expanded to.
#[derive(Debug, Clone, Copy)]
pub(super) enum Choice {
    Tuple(TupleId),
    Nth(u32),
}

/// The order a naive evaluation over the facts in fact order would produce
/// rows in: lexicographic in the choices, a tuple by its fact.
pub(super) fn cmp_order(store: &Store, a: &[Choice], b: &[Choice]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Choice::Tuple(x), Choice::Tuple(y)) if x == y => Ordering::Equal,
            (Choice::Tuple(x), Choice::Tuple(y)) => store.get(*x).cmp(store.get(*y)),
            (Choice::Nth(x), Choice::Nth(y)) => x.cmp(y),
            (Choice::Tuple(_), Choice::Nth(_)) => Ordering::Less,
            (Choice::Nth(_), Choice::Tuple(_)) => Ordering::Greater,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// One derived head with what it was derived from, for the circuit.
pub(super) struct Derived {
    pub(super) head: Atom,
    /// The tuples the body matched.
    pub(super) used: Vec<TupleId>,
    pub(super) absent: Vec<Atom>,
    pub(super) bindings: Vec<(String, Value)>,
    pub(super) order: Vec<Choice>,
}

/// What a body reads: the store, the compiled body, and per body literal
/// the window of tuples a relation read there sees (semi-naive: the tuples
/// before the delta, the delta, or every tuple). Negation reads `all`.
pub(super) struct Src<'a> {
    pub(super) store: &'a Store,
    pub(super) body: &'a ops::Body,
    pub(super) win: Vec<Window>,
    pub(super) all: Window,
}

impl<'a> Src<'a> {
    /// Every literal of a body of `len` literals reading every tuple of
    /// `store`.
    pub(super) fn whole(store: &'a Store, body: &'a ops::Body, len: usize) -> Self {
        let all = Window::below(store.len());
        Src {
            store,
            body,
            win: vec![all; len],
            all,
        }
    }
}

pub(super) fn eval_rule(
    rule: &RuleStmt,
    plan: &ops::Rule,
    src: &Src,
    rec: &Rec,
) -> Result<Vec<Derived>> {
    if let ops::Head::Agg { col, kind } = plan.head {
        return eval_rule_collect(rule, src, col, kind, rec);
    }

    let mut out = Vec::new();
    let rows = eval_body(&rule.body, src, rec)?;
    for Row {
        s: b,
        used,
        absent,
        order,
    } in rows
    {
        // Rule 2: an address argument is a content position. A head whose
        // address carries a null is stuck, not derived.
        if let Some(t) = crate::zset::address_arg(&rule.head)
            && let Some(v) = eval_term(t, &b)
        {
            let nulls = nulls_in(&v);
            if !nulls.is_empty() {
                rec.stuck(&b, nulls, "resource address carries a null");
                continue;
            }
        }
        if rec.any_blocked(&rule.head.args, &b) {
            continue;
        }
        let head = match instantiate_atom(&rule.head, &b) {
            Ok(h) => h,
            Err(e) => match head_error(&rule.head, &b) {
                Some(why) => bail!(why),
                None => return Err(e.context(format!("instantiate head {}", rule.head.pred))),
            },
        };
        let mut bindings: Vec<(String, Value)> = b
            .into_iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .collect();
        bindings.sort();
        out.push(Derived {
            head,
            used,
            absent,
            bindings,
            order,
        });
    }
    Ok(out)
}

/// One way to satisfy a body: the bindings, and for provenance the tuples
/// it matched and the negations that held; `order` for the naive order.
pub(super) struct Row {
    pub(super) s: HashMap<String, Value>,
    pub(super) used: Vec<TupleId>,
    pub(super) absent: Vec<Atom>,
    pub(super) order: Vec<Choice>,
}

impl Row {
    pub(super) fn with(&self, s: HashMap<String, Value>) -> Row {
        Row {
            s,
            used: self.used.clone(),
            absent: self.absent.clone(),
            order: self.order.clone(),
        }
    }
}

/// The key columns of a relation read under `s`, when every one is bound
/// to a value without a null (otherwise the read scans).
pub(super) fn probe(
    atom: &Atom,
    read: &ops::Read,
    s: &HashMap<String, Value>,
) -> Option<Vec<Value>> {
    if read.key.is_empty() || read.scan_all {
        return None;
    }
    let mut out = Vec::with_capacity(read.key.len());
    for &c in &read.key {
        let v = match &atom.args[c] {
            Term::Val(v) => v.clone(),
            Term::Var(x) => s.get(x)?.clone(),
            _ => return None,
        };
        if stuck::has_null(&v) {
            return None;
        }
        out.push(v);
    }
    // A loose read compares other columns too: a null bound there can
    // make a tuple outside the bucket undecided.
    if read.loose && atom.args.iter().any(|t| term_has_bound_null(t, s)) {
        return None;
    }
    Some(out)
}

pub(super) fn eval_body(body: &[Lit], src: &Src, rec: &Rec) -> Result<Vec<Row>> {
    let mut states: Vec<Row> = vec![Row {
        s: HashMap::new(),
        used: Vec::new(),
        absent: Vec::new(),
        order: Vec::new(),
    }];
    for (i, lit) in body.iter().enumerate() {
        let mut next = Vec::new();
        // A literal that keeps or extends a row takes it: its bindings
        // (a document among them) are moved on, not copied.
        let rows = std::mem::take(&mut states);
        match lit {
            Lit::Pos(atom) if atom.pred == "member" || atom.pred == "enumerate" => {
                expand_members(atom, &rows, rec, &mut next)?
            }
            Lit::Pos(atom) if ops::is_builtin_pred(&atom.pred) => {
                for row in rows {
                    if !rec.any_blocked(&atom.args, &row.s)
                        && eval_builtin_pred(atom, &row.s, rec)? == Some(true)
                    {
                        next.push(row);
                    }
                }
            }
            Lit::Pos(atom) => read_relation(atom, i, &rows, src, rec, &mut next)?,
            Lit::Not(atom) => {
                for row in rows {
                    if let Some(row) = negate(atom, row, src, rec)? {
                        next.push(row);
                    }
                }
            }
            Lit::Eq(a, b) => {
                for mut row in rows {
                    let s = std::mem::take(&mut row.s);
                    if let Some(s2) = eval_eq(a, b, s, rec)? {
                        row.s = s2;
                        next.push(row);
                    }
                }
            }
            Lit::Neq(a, b) => {
                for row in rows {
                    if eval_neq(a, b, &row.s, rec)? {
                        next.push(row);
                    }
                }
            }
            Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                for row in rows {
                    if eval_cmp(lit, a, b, &row.s, rec)? {
                        next.push(row);
                    }
                }
            }
        }
        states = next;
        if states.is_empty() {
            break;
        }
    }
    Ok(states)
}

/// `member`/`enumerate` over each row: a row per element it expands to.
fn expand_members(atom: &Atom, rows: &[Row], rec: &Rec, next: &mut Vec<Row>) -> Result<()> {
    for row in rows {
        if !rec.any_blocked(&atom.args, &row.s) {
            let mut out = Vec::new();
            eval_member_like(atom, &row.s, &mut out, rec)?;
            next.extend(out.into_iter().enumerate().map(|(n, s)| {
                let mut r = row.with(s);
                r.order.push(Choice::Nth(n as u32));
                r
            }));
        }
    }
    Ok(())
}

/// A relation read, body literal `i`, over each row: a row per tuple it
/// unifies with in the literal's window.
fn read_relation(
    atom: &Atom,
    i: usize,
    rows: &[Row],
    src: &Src,
    rec: &Rec,
    next: &mut Vec<Row>,
) -> Result<()> {
    let read = src
        .body
        .op(i)
        .read()
        .ok_or_else(|| anyhow!("internal: {} is not a relation read", atom.pred))?;
    for row in rows {
        let s = &row.s;
        if rec.any_blocked(&atom.args, s) {
            continue;
        }
        // Rule 3: a positive reader of an undetermined
        // aggregate group is undetermined. It still reads
        // the groups that were decided.
        if rec.aggregates.contains(&atom.pred) && !planted(Rule3Clause::Reader) {
            let nulls = rec.known.borrow().blocking(&read_pattern(atom, s));
            if !nulls.is_empty() {
                rec.stuck(
                    s,
                    nulls,
                    format!("reads undetermined aggregate {}", atom.pred),
                );
            }
        }
        let key = probe(atom, read, s);
        let before = next.len();
        for t in src
            .store
            .candidates(&read.rel, &read.key, key.as_deref(), read.loose, src.win[i])
        {
            if let Some(s2) = unify_atom(atom, src.store.get(t), s, rec)? {
                let mut r = row.with(s2);
                r.used.push(t);
                r.order.push(Choice::Tuple(t));
                next.push(r);
            }
        }
        if next.len() == before
            && let Some(cell) = string_cell(atom, read)
        {
            reference_at_string_cell(atom, &cell, s, src, src.win[i], rec)?;
        }
    }
    Ok(())
}

/// `not atom` over a row: the row, with the negation it held by, or
/// `None`.
fn negate(atom: &Atom, mut row: Row, src: &Src, rec: &Rec) -> Result<Option<Row>> {
    let s = &row.s;
    if rec.any_blocked(&atom.args, s) {
        return Ok(None);
    }
    if atom.pred == "member" {
        let holds = match atom.args.len() {
            2 => eval_not_member2(atom, s, rec)?,
            3 => eval_not_member3(atom, s, rec)?,
            _ => bail!("member/2 or member/3 expected"),
        };
        return Ok(holds.then_some(row));
    }
    if atom.pred == "enumerate" {
        // `enumerate/3` is a generator; `not enumerate(...)` is meaningless
        // (it would require checking existence over an implicit domain).
        bail!("negation not supported for enumerate/3");
    }
    if ops::is_builtin_pred(&atom.pred) {
        // Negation-as-failure for builtin predicates is just boolean
        // negation; a call over a null is undetermined either way.
        let holds =
            !rec.any_blocked(&atom.args, s) && eval_builtin_pred(atom, s, rec)? == Some(false);
        return Ok(holds.then_some(row));
    }
    let grounded =
        ground_atom(atom, s).with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
    if eval_not(&grounded, src, s, rec) {
        row.absent.push(grounded);
        return Ok(Some(row));
    }
    Ok(None)
}

/// A read `attr(T, X, P, "s")` whose type is a variable (`x in resource,
/// x.vpc == "main"`), its value a string the index looks up: the key that
/// finds the cell without its value (R-204). Where the type is known the
/// compiler says a reference is never a string; here only the cell can.
fn string_cell(atom: &Atom, read: &ops::Read) -> Option<ops::Key> {
    let [Term::Var(_), _, _, Term::Val(Value::Str(_))] = atom.args.as_slice() else {
        return None;
    };
    if atom.pred != "attr" || read.scan_all {
        return None;
    }
    let key: ops::Key = read.key.iter().copied().filter(|&c| c != 3).collect();
    (key.len() < read.key.len() && !key.is_empty()).then_some(key)
}

/// The indexes [`string_cell`] reads through, for a body and its plan.
pub(super) fn string_cells(body: &[Lit], plan: &ops::Body) -> Vec<(ops::Rel, ops::Key)> {
    body.iter()
        .enumerate()
        .filter_map(|(i, l)| match l {
            Lit::Pos(a) => {
                let read = plan.op(i).read()?;
                Some((read.rel.clone(), string_cell(a, read)?))
            }
            _ => None,
        })
        .collect()
}
