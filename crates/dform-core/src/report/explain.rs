//! How much of why each change is planned the report says (R-79, R-111): each
//! line's site, the writes a value beat, its chain and the statement's bindings
//! at `-v` and `-vv`, a create's lines folded, what changed since the last apply,
//! and the places relative to the project's root.

use super::deformation::{Deformation, Line};
use super::fold::{element_site, folded};
use super::groups::group_address;
use super::labels::{address, address_text, kind_name};
use super::lines::{gone, values};
use super::policy::denied;
use super::tree;
use super::tree::Site;
use super::waits::waited;
use super::{Report, Why};
use crate::address::Address;
use crate::ast::{Atom, RuleStmt, Term};
use crate::engine::EvalResult;
use crate::provider::ActionKind;
use crate::query::Redactor;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

impl Report {
    /// When a change this plan holds runs, and what it waits on, as `why`
    /// says it (After R-156): `tick 2  waits on  main.endpoint`; a resource
    /// rule's group `tick 2+  ..`, its tick a lower bound (what it is
    /// stuck on first); `later  waits on  ..` for what no tick of this plan
    /// makes. `at`: a change's address, full or as the plan prints it, or
    /// a group's. `None` for a change this tick makes, or none.
    pub fn when(&self, at: &str) -> Option<String> {
        let line = |tick: Option<usize>, plus: &str, on: &[String]| {
            let when = match tick {
                Some(t) => format!("tick {}{plus}", t + 1),
                None => "later".into(),
            };
            let on = waited(&on.iter().cloned().collect()).join(", ");
            match on.is_empty() {
                true => when,
                false => format!("{when}  waits on  {on}"),
            }
        };
        let named = |full: &str, shown: String| full == at || shown == at;
        for b in &self.pending {
            if b.deformations
                .iter()
                .any(|d| named(&d.addr.to_string(), address(&d.addr)))
            {
                return Some(line(b.resolves_after, "", &b.on));
            }
        }
        let g = self
            .groups
            .iter()
            .find(|g| named(&g.pattern, address_text(&group_address(g))))?;
        Some(line(g.resolves_after, "+", &g.on))
    }

    /// The object's value of `path` of `addr`, given at its creation only,
    /// where the plan keeps it (R-198): `"n1"`, `(sensitive)`.
    pub fn kept_value(&self, addr: &Address, path: &str) -> Option<String> {
        let pending = self.pending.iter().flat_map(|b| b.deformations.iter());
        self.kept
            .iter()
            .chain(&self.definite)
            .chain(pending)
            .filter(|d| d.addr == *addr)
            .flat_map(|d| &d.kept)
            .find(|k| k.line.path == path)
            .map(|k| k.line.before.said(Why::Line))
    }

    /// Say why each change is planned, at level `why` (R-79), from the
    /// provenance of `res`, the evaluation the plan was made from: each
    /// entry where it is derived, with its bindings; each attribute it
    /// changes where its winning value was written; at `Full`, under
    /// each, the derivation compressed to its leaves (a create by its
    /// `want`, an update by the winning contributions to each attribute it
    /// changes, a delete by state alone). Every line passes through `r`.
    pub fn explain(&mut self, why: Why, res: &EvalResult, r: &Redactor) {
        self.why = why;
        if why == Why::None {
            return;
        }
        let p = tree::Printer {
            circuit: &res.circuit,
            redact: r,
            all: false,
        };
        let rules = &res.rules;
        let attrs = self.changed_attrs(res);
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        let created: Vec<(Address, BTreeSet<(String, String)>)> = self
            .definite
            .iter()
            .filter(|d| matches!(d.kind, ActionKind::Create))
            .map(|d| (d.addr.clone(), values(d, |l| &l.after)))
            .collect();
        let live = crate::zset::Instances::from_facts(&res.facts);
        let removing = self.removing;
        let instances = &self.instances;
        for d in self.definite.iter_mut().chain(pending) {
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                // A destroy's deletes need no reason: the operation is it.
                // A deposed object is the one a replace left: no rule
                // was ever to want it.
                if !removing && matches!(d.kind, ActionKind::Delete) {
                    d.gone = gone(d, &created, instances, &live, res, r);
                }
                continue;
            }
            d.site = p.want_site(rules, &d.addr);
            let facts = attrs
                .get(&(d.addr.typ.clone(), d.addr.name.clone()))
                .map(Vec::as_slice)
                .unwrap_or_default();
            // A create the default level folds prints few of its lines
            // (a document's are its row, R-131): each printed line's site
            // is found as the fold prints it, unless every line says its
            // own (`every_site`: `plan --json`).
            let folds = why < Why::Full && matches!(d.kind, ActionKind::Create | ActionKind::Adopt);
            if folds && !self.every_site {
                let mut site = |l: &Line| {
                    attr_holding(facts, &l.path)
                        .and_then(|(a, keys, whole)| {
                            p.attr_sites(rules, a, &[(keys, whole)]).pop().flatten()
                        })
                        .or_else(|| element_site(&p, rules, facts, l))
                };
                d.folded = folded(d, &p, rules, facts, &res.facts, why, &mut site);
                continue;
            }
            line_sites(d, &p, rules, facts, why, &self.keys);
            if folds {
                d.folded = folded(d, &p, rules, facts, &res.facts, why, &mut |l| {
                    l.site.clone()
                });
            }
        }
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            d.lines
                .iter_mut()
                .chain(d.folded.iter_mut())
                .for_each(Line::mask);
        }
        self.site_others(&p, res);
    }

    /// The attribute facts of each changed resource, by its address.
    fn changed_attrs<'r>(&self, res: &'r EvalResult) -> BTreeMap<(String, String), Vec<&'r Atom>> {
        let changed: BTreeSet<(String, String)> = self
            .definite
            .iter()
            .chain(self.pending.iter().flat_map(|b| b.deformations.iter()))
            .map(|d| (d.addr.typ.clone(), d.addr.name.clone()))
            .collect();
        let mut attrs: BTreeMap<(String, String), Vec<&Atom>> = BTreeMap::new();
        for f in res.facts.iter().filter(|f| f.pred == "attr") {
            if let [Term::Val(Value::Str(t)), Term::Val(Value::Str(a)), ..] = f.args.as_slice() {
                let k = (t.clone(), a.clone());
                if changed.contains(&k) {
                    attrs.entry(k).or_default().push(f);
                }
            }
        }
        attrs
    }

    /// The sites of what is not a change: a kept value's, a group's, a
    /// policy's, what is not planned, a deny over the plan's, an approval's.
    fn site_others(&mut self, p: &tree::Printer, res: &EvalResult) {
        let rules = &res.rules;
        for d in &mut self.kept {
            d.site = p.want_site(rules, &d.addr);
        }
        for g in &mut self.groups {
            g.site = g
                .rule
                .as_ref()
                .and_then(|id| p.rule_site(rules, id, &g.bindings));
        }
        for x in &mut self.policies {
            x.site = x.rule.as_ref().and_then(|id| p.rule_site(rules, id, &[]));
        }
        // A check's site is its rule's.
        for l in self.policy.iter_mut().filter(|l| l.at.is_empty()) {
            let site = self
                .policies
                .iter()
                .find_map(|x| match x.message == l.text {
                    true => x.site.as_ref(),
                    false => None,
                });
            if let Some(site) = site {
                l.at = site.at.clone();
            }
        }
        for n in &mut self.not_planned {
            n.site = n.rule.as_ref().and_then(|id| p.rule_site(rules, id, &[]));
        }
        self.denied = self
            .denies
            .iter()
            .map(|text| denied(p, res, text))
            .collect();
        for a in &mut self.approvals {
            a.site = res
                .facts
                .iter()
                .filter(|f| f.pred == "requires_approval" && f.args.len() == 2)
                .find(|f| {
                    crate::approval::needs(&BTreeSet::from([(*f).clone()]))
                        == [(a.addr.clone(), a.reason.clone())]
                })
                .and_then(|f| res.circuit.fact_id(&crate::engine::circuit_fact(f)))
                .and_then(|id| p.site(rules, id));
        }
    }

    /// Under each change, the leaf that changed since the last apply
    /// (R-79): `then` is the snapshot of the program the last apply ran,
    /// `now` this plan's, both for the plan's addresses. A delete is also
    /// said where the last apply derived it (`was FILE:LINE`).
    pub fn because(&mut self, then: &crate::diff::Snapshot, now: &crate::diff::Snapshot) {
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            let addr = d.addr.to_string();
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                d.site = then.sites.get(&addr).cloned();
            }
            let paths: Vec<String> = d.lines.iter().map(|l| l.path.clone()).collect();
            d.because = crate::diff::because_since(kind_name(&d.kind), &addr, &paths, then, now);
        }
    }

    /// Every site's place relative to `top`, the project's root (the
    /// program's directory outside a project), whatever directory the run
    /// is in and however it named the program.
    pub fn relative_to(&mut self, top: &std::path::Path) {
        let place = |at: &str| relative_place(at, top);
        let fix = |s: &mut Option<Site>| {
            if let Some(s) = s
                && let Some(at) = place(&s.at)
            {
                s.at = at;
            }
        };
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            fix(&mut d.site);
            for l in d.lines.iter_mut().chain(d.folded.iter_mut()) {
                fix(&mut l.site);
                for step in l.chain.iter_mut() {
                    if let Some(at) = place(&step.at) {
                        step.at = at;
                    }
                }
            }
        }
        self.kept.iter_mut().for_each(|d| fix(&mut d.site));
        self.groups.iter_mut().for_each(|g| fix(&mut g.site));
        self.policies.iter_mut().for_each(|p| fix(&mut p.site));
        for l in &mut self.policy {
            if let Some(at) = place(&l.at) {
                l.at = at;
            }
        }
        self.approvals.iter_mut().for_each(|a| fix(&mut a.site));
        self.denied.iter_mut().for_each(|d| fix(&mut d.site));
        for d in self.conflicts.iter_mut().chain(self.shadowed.iter_mut()) {
            let witnesses = d.witnesses.iter_mut().flat_map(|w| w.at.iter_mut());
            for at in witnesses.chain(d.at.as_mut()) {
                if let Some(p) = place(at) {
                    *at = p;
                }
            }
        }
    }
}

/// The chain of change path `path`'s value ([`attr_site`]'s fact).
fn attr_chain(
    p: &tree::Printer,
    rules: &[RuleStmt],
    facts: &[&Atom],
    path: &str,
    stack_keys: &BTreeSet<String>,
) -> Vec<tree::Step> {
    match attr_holding(facts, path) {
        Some((a, keys, true)) => p.attr_chain(rules, a, &keys, stack_keys),
        _ => Vec::new(),
    }
}

/// The attribute fact of `facts` that holds change path `path`, the keys
/// below it, and whether they reach the leaf.
pub(super) fn attr_holding<'a>(
    facts: &[&'a Atom],
    path: &str,
) -> Option<(&'a Atom, Vec<String>, bool)> {
    let segs = crate::address::path_segments(path);
    for k in (1..=segs.len()).rev() {
        let last = segs[k - 1];
        let index = crate::address::segment_parts(last).1;
        // The segments are `path`'s own, joined by dots: the prefix to
        // `last`, its index left out, is a slice of it.
        let start = last.as_ptr() as usize - path.as_ptr() as usize;
        let prefix = &path[..start + last.len() - index.len()];
        let found = facts
            .iter()
            .find(|a| matches!(a.args.get(2), Some(Term::Val(Value::Str(p))) if p == prefix));
        if let Some(a) = found {
            let keys: Vec<String> = match index.is_empty() {
                true => segs[k..]
                    .iter()
                    .take_while(|s| crate::address::segment_parts(s).1.is_empty())
                    .map(|s| crate::address::segment_key(s).into_owned())
                    .collect(),
                false => vec![],
            };
            let whole = index.is_empty() && keys.len() == segs.len() - k;
            return Some((*a, keys, whole));
        }
    }
    None
}

/// Site `at` (`FILE:LINE`) relative to `top`, the project's root; `None`
/// when it is not under it, or not a file's.
pub fn relative_place(at: &str, top: &std::path::Path) -> Option<String> {
    let prefix = format!("{}/", top.display());
    let (file, line) = at.rsplit_once(':')?;
    if file.starts_with('<') {
        return None;
    }
    let abs = std::path::absolute(file).ok()?;
    let rest = abs.to_str()?.strip_prefix(&prefix)?;
    Some(format!("{rest}:{line}"))
}

/// Each line of `d`'s site, the lines under one attribute fact asked
/// together, and at `Full` its chain.
fn line_sites(
    d: &mut Deformation,
    p: &tree::Printer,
    rules: &[RuleStmt],
    facts: &[&Atom],
    why: Why,
    keys: &BTreeSet<String>,
) {
    // By the fact (its address): the fact, and each line's index,
    // keys and whether they reach the leaf.
    type Asked<'a> = (&'a Atom, Vec<(usize, (Vec<String>, bool))>);
    let mut asks: BTreeMap<*const Atom, Asked> = BTreeMap::new();
    for (i, l) in d.lines.iter().enumerate() {
        if let Some((a, keys, whole)) = attr_holding(facts, &l.path) {
            asks.entry(a as *const Atom)
                .or_insert_with(|| (a, Vec::new()))
                .1
                .push((i, (keys, whole)));
        }
    }
    let mut sites: Vec<Option<tree::Site>> = vec![None; d.lines.len()];
    for (a, lines) in asks.into_values() {
        let (at, asked): (Vec<usize>, Vec<(Vec<String>, bool)>) = lines.into_iter().unzip();
        for (i, site) in at.into_iter().zip(p.attr_sites(rules, a, &asked)) {
            sites[i] = site;
        }
    }
    for (l, site) in d.lines.iter_mut().zip(sites) {
        l.site = site.or_else(|| element_site(p, rules, facts, l));
        if why == Why::Full {
            l.chain = attr_chain(p, rules, facts, &l.path, keys);
        }
    }
}
