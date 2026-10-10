//! One attribute group collapsed (E §2.5, F DR-9 revised): its ranked contributions, element
//! writes and refinements joined in the path's lattice, and the cell's facts: the value, a
//! conflict or a violated refinement with its deny, a stuck disagreement, a shadowed warning.

use super::contributions::{Contribution, ElemContribution, GroupKey, Origins, Ready, rank_name};
use super::errors::with_place;
use super::policy::policy_fact;
use super::store::{Store, TupleId};
use crate::ast::{Atom, Term, str_term};
use crate::diag;
use crate::lattice::{self, Collapsed, Lattice, RankedContribution, Shadowed, Witnesses};
use crate::spell;
use crate::value::Value;
use std::collections::BTreeMap;

pub(super) fn obj(kv: Vec<(&str, Value)>) -> Value {
    Value::Obj(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// The collapsed cell as facts: the value, or the conflict with a `deny`
/// naming every witness, or the stuck disagreement; plus a warning per
/// shadowed disagreement at a losing rank.
pub(super) fn collapse_group(
    group: &Ready,
    refs: &[&(TupleId, crate::refine::Stated)],
    lat: &Lattice,
    nested: &BTreeMap<String, Lattice>,
    origins: &Origins,
    store: &Store,
) -> Vec<Atom> {
    let (key, contribs, elems) = (&group.key, &group.contribs[..], &group.elems[..]);
    // An element write's witness follows the contributions', a
    // refinement's the element writes'.
    let writes = elem_writes(elems, contribs.len());
    let first_ref = contribs.len() + elems.len();
    let below = lattice::Below {
        lattices: Some(nested),
        elems: &writes,
    };
    let collapsed = lattice::lub_ranked_refined(
        lat,
        &key.2,
        &ranked(contribs),
        &refinements(refs, first_ref),
        below,
    );
    let mut cell = Collapse {
        key,
        contribs,
        elems,
        refs,
        first_ref,
        origins,
        store,
        out: Vec::new(),
    };
    for sh in cell.collapsed(collapsed) {
        cell.shadowed(sh);
    }
    cell.out
}

/// The contributions as the lattice ranks them, each its own witness.
fn ranked(contribs: &[Contribution]) -> Vec<RankedContribution> {
    contribs
        .iter()
        .enumerate()
        .map(|(i, (_, r, v))| (i as u32, *r, v.clone()))
        .collect()
}

/// The element writes, their witnesses numbered from `first`.
fn elem_writes(elems: &[ElemContribution], first: usize) -> Vec<lattice::ElemWrite> {
    elems
        .iter()
        .enumerate()
        .map(|(i, (_, r, list, k, v))| lattice::ElemWrite {
            witness: (first + i) as u32,
            rank: *r,
            list: list.clone(),
            key: k.clone(),
            value: v.clone(),
        })
        .collect()
}

/// The refinements joined into the cell, their witnesses numbered from
/// `first`.
fn refinements(
    refs: &[&(TupleId, crate::refine::Stated)],
    first: usize,
) -> Vec<lattice::Refinement> {
    refs.iter()
        .enumerate()
        .map(|(i, (_, r))| lattice::Refinement {
            path: r.path.clone(),
            constraint: r.constraint.clone(),
            witness: (first + i) as u32,
        })
        .collect()
}

/// A cell being collapsed, its witnesses numbered: the contributions', the
/// element writes' after them, the refinements' last; and its facts.
struct Collapse<'a> {
    key: &'a GroupKey,
    contribs: &'a [Contribution],
    elems: &'a [ElemContribution],
    refs: &'a [&'a (TupleId, crate::refine::Stated)],
    /// The witness of the first refinement.
    first_ref: usize,
    origins: &'a Origins,
    store: &'a Store,
    out: Vec<Atom>,
}

impl Collapse<'_> {
    /// The cell's facts for what the lattice made of it; the disagreements
    /// it shadowed.
    fn collapsed(&mut self, collapsed: Collapsed) -> Vec<Shadowed> {
        match collapsed {
            Collapsed::Bottom => vec![],
            Collapsed::Val {
                value,
                shadowed,
                deferred,
                ..
            } => {
                self.value(value, deferred);
                shadowed
            }
            Collapsed::Violated {
                path: at,
                constraint,
                value,
                witnesses: ws,
                refinement,
                shadowed,
            } => {
                self.violated(at, &constraint, value, &ws, &refinement);
                shadowed
            }
            Collapsed::Stuck {
                nulls, shadowed, ..
            } => {
                self.out.push(self.head(
                    "attr_stuck",
                    vec![Value::List(nulls.into_iter().map(Value::Str).collect())],
                ));
                shadowed
            }
            Collapsed::Conflict {
                a,
                b,
                reason,
                witnesses: ws,
                shadowed,
                ..
            } => {
                self.conflict(&a.1, &b.1, reason, &ws);
                shadowed
            }
        }
    }

    /// A fact of the cell: `pred(T, A, P, rest..)`.
    fn head(&self, pred: &str, rest: Vec<Value>) -> Atom {
        let (typ, addr, path) = self.key;
        Atom {
            pred: pred.into(),
            args: [str_term(typ), Term::Val(addr.clone()), str_term(path)]
                .into_iter()
                .chain(rest.into_iter().map(Term::Val))
                .collect(),
            record: None,
            span: Default::default(),
        }
    }

    /// The refinement witness `w` is, if it is one.
    fn refinement_of(&self, w: u32) -> Option<&(TupleId, crate::refine::Stated)> {
        self.refs
            .get((w as usize).checked_sub(self.first_ref)?)
            .copied()
    }

    /// The witness `w` as a policy's context says it: its rank, its value
    /// and where it is from.
    fn witness(&self, w: u32) -> Value {
        let store = self.store;
        if let Some((t, r)) = self.refinement_of(w) {
            let a = store.get(*t);
            return obj(vec![
                ("rank", Value::Str("refinement".into())),
                ("value", Value::Str(r.constraint.to_string())),
                (
                    "from",
                    Value::List(vec![Value::Str(with_place(spell::atom(a), a.span))]),
                ),
            ]);
        }
        let (t, r, v) = match self.contribs.get(w as usize) {
            Some((t, r, v)) => (t, r, v),
            None => {
                let (t, r, _, _, v) = &self.elems[w as usize - self.contribs.len()];
                (t, r, v)
            }
        };
        obj(vec![
            ("rank", Value::Str(rank_name(*r).into())),
            ("value", v.clone()),
            (
                "from",
                Value::List(
                    self.origins
                        .of(*t, store.get(*t))
                        .into_iter()
                        .map(Value::Str)
                        .collect(),
                ),
            ),
        ])
    }

    fn witnesses(&self, ws: &Witnesses) -> Value {
        Value::List(ws.iter().map(|w| self.witness(*w)).collect())
    }

    /// The first of `ws`, or an empty object.
    fn first(&self, ws: &Witnesses) -> Value {
        ws.iter()
            .next()
            .map(|w| self.witness(*w))
            .unwrap_or(Value::Obj(BTreeMap::new()))
    }

    /// A policy's context of the cell: its type, address and path, then
    /// `extra`.
    fn ctx(&self, extra: Vec<(&str, Value)>) -> Value {
        let (typ, addr, path) = self.key;
        let mut kv = vec![
            ("type", Value::Str(typ.clone())),
            ("addr", addr.clone()),
            ("path", Value::Str(path.clone())),
        ];
        kv.extend(extra);
        obj(kv)
    }

    /// The cell's value, and a refinement whose value it does not know yet,
    /// deferred.
    fn value(&mut self, value: Value, deferred: Vec<lattice::Deferred>) {
        let (typ, addr, _) = self.key;
        self.out.push(self.head("attr", vec![value]));
        for d in deferred {
            let nulls = lattice::nulls_in(&d.value);
            self.out.push(Atom {
                pred: crate::refine::DEFERRED.into(),
                args: [
                    Value::Str(typ.clone()),
                    addr.clone(),
                    Value::Str(d.path),
                    Value::Str(d.constraint.to_string()),
                    Value::List(nulls.into_iter().map(Value::Str).collect()),
                ]
                .into_iter()
                .map(Term::Val)
                .collect(),
                record: None,
                span: Default::default(),
            });
        }
    }

    /// A value that violates a refinement at `at`: the conflict, and a deny
    /// naming the refinement's place.
    fn violated(
        &mut self,
        at: String,
        constraint: &crate::lattice::Constraint,
        value: Value,
        ws: &Witnesses,
        refinement: &Witnesses,
    ) {
        let (typ, addr, _) = self.key;
        self.out.push(self.head(
            "attr_conflict",
            vec![self.first(ws), self.first(refinement)],
        ));
        let place = refinement
            .iter()
            .find_map(|w| self.refinement_of(*w))
            .and_then(|(t, _)| diag::place(self.store.get(*t).span))
            .unwrap_or_default();
        let mut kv = vec![
            ("type", Value::Str(typ.clone())),
            ("addr", addr.clone()),
            ("path", Value::Str(at)),
            ("constraint", Value::Str(constraint.to_string())),
            (
                "reason",
                Value::Str(format!("{} violates {constraint}", spell::value(&value))),
            ),
            ("value", value),
            (
                "witnesses",
                self.witnesses(&ws.union(refinement).copied().collect()),
            ),
        ];
        if !place.is_empty() {
            kv.push(("at", Value::Str(place)));
        }
        self.out
            .push(policy_fact("deny", crate::refine::VIOLATED, obj(kv)));
    }

    /// Contributions that conflict: the conflict, and a deny naming every
    /// witness.
    fn conflict(&mut self, a: &Witnesses, b: &Witnesses, reason: String, ws: &Witnesses) {
        self.out
            .push(self.head("attr_conflict", vec![self.first(a), self.first(b)]));
        let ctx = self.ctx(vec![
            ("reason", Value::Str(reason)),
            ("witnesses", self.witnesses(ws)),
        ]);
        self.out.push(policy_fact(
            "deny",
            "conflicting attribute contributions",
            ctx,
        ));
    }

    /// A disagreement at a losing rank, overridden: a warning.
    fn shadowed(&mut self, sh: Shadowed) {
        let (rank, what, ws) = match sh {
            Shadowed::Stuck {
                rank,
                nulls,
                witnesses,
            } => (
                rank,
                format!(
                    "undecided until {}",
                    nulls
                        .iter()
                        .map(|n| format!("?{}", crate::ir::label(n)))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                witnesses,
            ),
            Shadowed::Conflict {
                rank,
                path,
                reason,
                witnesses,
            } => (rank, format!("{reason} at {path}"), witnesses),
        };
        let ctx = self.ctx(vec![
            ("rank", Value::Str(rank_name(rank).into())),
            ("reason", Value::Str(what)),
            ("witnesses", self.witnesses(&ws)),
        ]);
        self.out.push(policy_fact(
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
            ctx,
        ));
    }
}
