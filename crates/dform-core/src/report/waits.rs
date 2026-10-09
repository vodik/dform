//! What a held change waits on, and what its waits are made from (R-156, R-193):
//! the boundary or the nulls it waits on, the deployment or the wait outside the
//! plan it is followed to (`Until`), the resources of this plan it resolves
//! after, a block planned provisionally against an offline schema.

use super::Input;
use super::deformation::Deformation;
use super::labels::{address, extern_label, label, reference};
use crate::ast::{Atom, RuleStmt, Term};
use crate::ir::Address;
use crate::provider::{Action, ActionKind};
use crate::stuck::{Sections, Stuck};
use crate::value::{Value, null_owner};
use std::collections::{BTreeMap, BTreeSet};

/// What a deformation waits on: a boundary (the evaluator's sections), or
/// a comparison against an open null (the Z-set's pending update). `None`
/// when it is definite.
pub fn waits_on(a: &Action, sections: &Sections) -> Option<Vec<String>> {
    let key = (a.addr.typ.clone(), a.addr.name.clone());
    let on: Vec<String> = match (sections.pending.get(&key), &a.kind) {
        (Some(ns), _) => ns.iter().cloned().collect(),
        (None, ActionKind::Pending) => a.on.iter().cloned().collect(),
        // A deposed object's delete held for its dependents (`executor`).
        (None, _) if !a.on.is_empty() => a.on.iter().cloned().collect(),
        (None, _) => return None,
    };
    Some(on)
}

#[derive(Debug, Clone)]
pub struct PendingBlock {
    pub on: Vec<String>,
    pub resolves_after: Option<usize>,
    pub deformations: Vec<Deformation>,
    /// What it waits on outside this plan, as the summary says it
    /// (R-193): `platform[env=lab]` for a kubeconfig read from it.
    pub until: BTreeSet<Until>,
    /// Planned against its provider's offline schema while its
    /// connection waits (R-193): the settings it waits on, `kubeconfig`.
    pub provisional: Option<Vec<String>>,
}

/// What a held change, or an undetermined deny, waits on outside this
/// plan, followed through what its waits are made from (R-193): a
/// deployment not applied yet, or a wait the plan cannot follow further
/// (a read that said "not yet", a provider configured from outside).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Until {
    Applied(String),
    Waits(String),
}

/// The line under a provisional block's header (R-193).
pub(super) fn provisional_text(keys: &[String]) -> String {
    let keys = match keys {
        [] => "its connection".to_string(),
        keys => keys.join(", "),
    };
    format!("provisional: planned against the offline schema; planned again once {keys} is known")
}

/// `after platform[env=lab] is applied`, `waiting on provider k8s  schema`:
/// what `until` waits on, as the summary's clause ends.
pub(super) fn until_text(until: &BTreeSet<Until>) -> String {
    let stacks: Vec<&str> = until
        .iter()
        .filter_map(|u| match u {
            Until::Applied(s) => Some(s.as_str()),
            Until::Waits(_) => None,
        })
        .collect();
    let waits: Vec<&str> = until
        .iter()
        .filter_map(|u| match u {
            Until::Waits(w) => Some(w.as_str()),
            Until::Applied(_) => None,
        })
        .collect();
    let mut out = Vec::new();
    match stacks.as_slice() {
        [] => {}
        [s] => out.push(format!("after {s} is applied")),
        [rest @ .., last] => out.push(format!("after {} and {last} are applied", rest.join(", "))),
    }
    if !waits.is_empty() {
        out.push(format!("waiting on {}", waits.join(", ")));
    }
    out.join(", ")
}

/// Whether a held block waits only on providers' connections, which
/// planned it against their offline schemas (R-193): the settings, as
/// the waits name them (`kubeconfig` of `provider k8s  kubeconfig = ..`).
pub(super) fn provisional(on: &[String], sections: &Sections) -> Option<Vec<String>> {
    if on.is_empty() || !on.iter().all(|l| sections.provisional.contains(l)) {
        return None;
    }
    let mut keys: Vec<String> = on
        .iter()
        .filter_map(|l| l.split_once("  ").map(|(_, s)| s))
        .flat_map(|s| s.split(", "))
        .filter_map(|kv| kv.split_once(" = ").map(|(k, _)| k.to_string()))
        .collect();
    keys.dedup();
    Some(keys)
}

/// What the waits of what no tick of this plan makes are made from
/// (R-193): a provider's wait, the values its settings hold; a change
/// `later` holds (a CRD), its own waits; a value of one, the same; until
/// a deployment not applied yet, or what the plan cannot follow.
pub(super) struct Follow<'a> {
    pub(super) i: &'a Input<'a>,
    /// The waits of each held change no tick makes, by its full and its
    /// printed address and by its key.
    pub(super) later: BTreeMap<String, Vec<String>>,
    by_key: BTreeMap<(String, String), Vec<String>>,
}

impl<'a> Follow<'a> {
    pub(super) fn new(
        i: &'a Input<'a>,
        held: &[&Action],
        tick_of: &BTreeMap<(String, String), usize>,
    ) -> Follow<'a> {
        let mut later = BTreeMap::new();
        let mut by_key = BTreeMap::new();
        for a in held {
            let key = (a.addr.typ.clone(), a.addr.name.clone());
            if tick_of.contains_key(&key) {
                continue;
            }
            let on = waits_on(a, i.sections).unwrap_or_default();
            later.insert(a.addr.to_string(), on.clone());
            later.insert(address(&a.addr), on.clone());
            by_key.insert(key, on);
        }
        Follow { i, later, by_key }
    }

    pub(super) fn until(&self, on: &[String]) -> BTreeSet<Until> {
        let (mut seen, mut out) = (BTreeSet::new(), BTreeSet::new());
        for l in on {
            self.walk(l, &mut seen, &mut out);
        }
        out
    }

    pub(super) fn walk(&self, l: &str, seen: &mut BTreeSet<String>, out: &mut BTreeSet<Until>) {
        if !seen.insert(l.to_string()) {
            return;
        }
        let waits = |l: &str| Until::Waits(waited(&BTreeSet::from([l.to_string()])).concat());
        if let Some(rest) = l.strip_prefix("provider ") {
            let name = rest.split_once("  ").map_or(rest, |(n, _)| n);
            let nulls = self.settings_nulls(name);
            if nulls.is_empty() {
                out.insert(Until::Waits(l.to_string()));
            }
            for n in nulls {
                self.walk(&n, seen, out);
            }
            return;
        }
        if let Some(on) = self.later.get(l) {
            for x in on {
                self.walk(x, seen, out);
            }
            return;
        }
        match crate::value::null_parts(l) {
            Some((typ, name, _)) if typ == crate::stack::UNAPPLIED => {
                out.insert(Until::Applied(name));
            }
            Some((typ, name, _)) if self.by_key.contains_key(&(typ.clone(), name.clone())) => {
                for x in &self.by_key[&(typ, name)] {
                    self.walk(x, seen, out);
                }
            }
            _ => {
                out.insert(waits(l));
            }
        }
    }

    /// The nulls provider `name`'s settings hold, as its `provider_config`
    /// row has them.
    fn settings_nulls(&self, name: &str) -> BTreeSet<String> {
        self.i
            .res
            .facts
            .iter()
            .filter(|a| a.pred == "provider_config")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(n)), Term::Val(v)] if n == name => Some(v),
                _ => None,
            })
            .flat_map(crate::lattice::nulls_in)
            .collect()
    }
}

pub(super) type Resolves<'a> =
    dyn Fn(&[String], &BTreeMap<(String, String), usize>) -> Option<usize> + 'a;

/// What makes each wait of a held change that no null names (R-156): the
/// resources of this plan it is resolved after. A CRD's wait (R-126), the
/// CRD; a provider's (`provider k8s  kubeconfig = ..`, `provider k8s
/// schema`, R-110), what its settings are made from ([`settings_owners`]).
/// A wait not here, or one whose owners this plan does not schedule, is
/// outside the plan: another stack's output, a provider configured from
/// outside, a read no tick makes answerable.
pub(super) fn boundary_owners(
    i: &Input,
    held: &[&Action],
) -> BTreeMap<String, Vec<(String, String)>> {
    let key = |a: &Address| (a.typ.clone(), a.name.clone());
    let printed: BTreeMap<String, (String, String)> = i
        .plan
        .actions
        .iter()
        .map(|a| (address(&a.addr), key(&a.addr)))
        .collect();
    let mut out = BTreeMap::new();
    for a in held {
        for l in waits_on(a, i.sections).unwrap_or_default() {
            if out.contains_key(&l) {
                continue;
            }
            let owners = match (printed.get(&l), l.strip_prefix("provider ")) {
                (Some(k), _) => Some(vec![k.clone()]),
                (None, Some(rest)) => {
                    let name = rest.split_once("  ").map_or(rest, |(n, _)| n);
                    settings_owners(i, name).map(|o| o.into_iter().collect())
                }
                (None, None) => None,
            };
            if let Some(o) = owners {
                out.insert(l, o);
            }
        }
    }
    out
}

/// The resources provider `name`'s settings are made from, when every
/// null they wait on is one's (R-156): the nulls its `provider_config`
/// row holds, or, when no row derives yet, those of the stuck instances
/// its settings' rules read (a kubeconfig read from the server a tick
/// creates). `None` when they wait on something no resource makes (a
/// read that said "not yet", a stand-in) or on nothing this run can see.
fn settings_owners(i: &Input, name: &str) -> Option<BTreeSet<(String, String)>> {
    let named = |a: &Atom| matches!(a.args.first(), Some(Term::Val(Value::Str(n))) if n == name);
    let mut nulls: BTreeSet<String> = BTreeSet::new();
    let rows: Vec<&Atom> = i
        .res
        .facts
        .iter()
        .filter(|a| a.pred == "provider_config" && named(a))
        .collect();
    for a in &rows {
        if let Some(Term::Val(v)) = a.args.get(1) {
            nulls.extend(crate::lattice::nulls_in(v));
        }
    }
    if rows.is_empty() {
        let rules: Vec<&RuleStmt> = crate::modules::reached(i.program)
            .into_iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Rule(r) => Some(r),
                _ => None,
            })
            .collect();
        let body = |r: &RuleStmt| -> Vec<Atom> {
            r.body
                .iter()
                .filter_map(|l| match l {
                    crate::ast::Lit::Pos(a) | crate::ast::Lit::Not(a) => Some(a.clone()),
                    _ => None,
                })
                .collect()
        };
        // The atoms the settings read, through the rules that derive them.
        let mut read: Vec<Atom> = rules
            .iter()
            .filter(|r| r.head.pred == "provider_config" && named(&r.head))
            .flat_map(|r| body(r))
            .collect();
        let mut used: BTreeSet<usize> = BTreeSet::new();
        let mut next = 0;
        while next < read.len() {
            let a = read[next].clone();
            next += 1;
            for (k, r) in rules.iter().enumerate() {
                if !used.contains(&k) && crate::stuck::patterns_unify(&r.head, &a) {
                    used.insert(k);
                    read.extend(body(r));
                }
            }
        }
        for s in &i.res.stuck {
            if read
                .iter()
                .any(|a| crate::stuck::patterns_unify(a, &s.head))
            {
                nulls.extend(s.nulls.iter().cloned());
            }
        }
    }
    let owners = nulls
        .iter()
        .map(|l| null_owner(l).filter(|o| !crate::externs::in_process(&o.0)))
        .collect::<Option<BTreeSet<_>>>()?;
    (!owners.is_empty()).then_some(owners)
}

pub(super) fn nulls(s: &Stuck) -> Vec<String> {
    s.nulls.iter().cloned().collect()
}

/// The values a tick waits on, as the references they are,
/// `k3s.server.public_ip` (R-111).
pub(crate) fn waited(on: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in on {
        let w = match extern_label(n) {
            // What dform's own extern has not answered yet (a host still
            // booting, a file not written yet), as the call.
            Some(call) => call,
            None => label(n),
        };
        // A deployment not applied yet once, whatever of it is read.
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// The values `on` as [`waited`] says them, each resource's as the
/// resource: `ns` for `ns.metadata.uid, ns.metadata.generation`.
pub(super) fn owners(on: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in on {
        let w = match crate::value::null_parts(n) {
            Some((typ, name, _))
                if extern_label(n).is_none()
                    && !name.is_empty()
                    && typ != crate::stack::UNAPPLIED
                    && typ != crate::transform::OUTPUT =>
            {
                reference(&Address { typ, name }, "")
            }
            _ => label(n),
        };
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}
