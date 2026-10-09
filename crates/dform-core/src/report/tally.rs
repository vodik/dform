//! The plan's headline (R-193, R-200): what one deployment's plan counts,
//! or a tree of them summed, said once at the top.

use super::Report;
use super::deformation::Deformation;
use super::labels::kind_name;
use super::waits::until_text;
use crate::provider::ActionKind;
use std::collections::BTreeMap;

/// What a plan's headline counts: its changes by kind, what `later`
/// holds by what it waits on, and the rest by kind. One deployment's
/// ([`Report::tally`]), or several summed ([`Tally::add`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// The definite changes, by kind in [`KINDS`] order.
    pub kinds: Vec<(&'static str, usize)>,
    /// The ticks with a change; none for a tree of deployments, each of
    /// which has its own.
    pub ticks: usize,
    /// `--show-noop`'s count.
    pub noops: Option<usize>,
    pub denied: usize,
    pub approvals: usize,
    pub not_planned: usize,
    /// Held objects `later` lists, no change of theirs known yet (R-177).
    pub held: usize,
    pub conflicts: usize,
    /// `later`'s changes by what they wait on outside the plan
    /// (`after stacks.platform[env=lab] is applied`), each by kind.
    pub later: Vec<(String, Vec<(&'static str, usize)>)>,
    /// The policies that hold, fail and are undetermined (R-200's policy
    /// block).
    pub policy: (usize, usize, usize),
}

impl Tally {
    /// The definite changes.
    pub fn changes(&self) -> usize {
        self.kinds.iter().map(|(_, n)| n).sum()
    }

    /// No change, now or later, and nothing that stops an apply.
    pub fn is_quiet(&self) -> bool {
        self.changes() == 0
            && self.later.is_empty()
            && self.denied == 0
            && self.conflicts == 0
            && self.approvals == 0
    }

    /// `other` counted in: a tree's headline sums its deployments'.
    pub fn add(&mut self, other: &Tally) {
        let sum = |a: &mut Vec<(&'static str, usize)>, b: &[(&'static str, usize)]| {
            for (k, n) in b {
                match a.iter_mut().find(|(x, _)| x == k) {
                    Some((_, m)) => *m += n,
                    None => a.push((k, *n)),
                }
            }
            a.sort_by_key(|(k, _)| KINDS.iter().position(|x| x == k));
        };
        sum(&mut self.kinds, &other.kinds);
        self.ticks = 0;
        self.noops = match (self.noops, other.noops) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        self.denied += other.denied;
        self.approvals += other.approvals;
        self.policy.0 += other.policy.0;
        self.policy.1 += other.policy.1;
        self.policy.2 += other.policy.2;
        self.not_planned += other.not_planned;
        self.held += other.held;
        self.conflicts += other.conflicts;
        for (until, kinds) in &other.later {
            match self.later.iter_mut().find(|(u, _)| u == until) {
                Some((_, k)) => sum(k, kinds),
                None => self.later.push((until.clone(), kinds.clone())),
            }
        }
    }

    /// `plan: 5 changes (3 create, 2 update) over 2 ticks, 1 approval`;
    /// what `later` holds, by kind and by what it waits on (R-193); the
    /// policies (R-200): `plan: 21 creates after stacks.platform[env=lab]
    /// is applied; policy: 12 hold · 2 undetermined`, never `0 changes`
    /// while `later` holds a change.
    pub fn text(&self) -> String {
        let later = self.later_clauses();
        let mut head = Vec::new();
        if self.changes() > 0 || later.is_empty() {
            let mut out = changes_text(self.changes(), &self.kinds);
            if self.ticks > 0 {
                out.push_str(&format!(" over {}", count(self.ticks, "tick")));
            }
            head.push(out.trim_start_matches("plan: ").to_string());
        }
        if let Some(n) = self.noops {
            head.push(format!("{n} no-op"));
        }
        if self.denied > 0 {
            head.push(format!("{} denied", self.denied));
        }
        if self.approvals > 0 {
            head.push(count(self.approvals, "approval"));
        }
        if self.not_planned > 0 {
            head.push(format!("{} not planned", self.not_planned));
        }
        let held = (self.held > 0).then(|| format!("{} later", self.held));
        if later.is_empty() {
            head.extend(held.clone());
        }
        if self.conflicts > 0 {
            head.push(count(self.conflicts, "conflict"));
        }
        let mut parts = Vec::new();
        if !head.is_empty() {
            parts.push(head.join(", "));
        }
        if !later.is_empty() {
            parts.extend(later);
            parts.extend(held);
        }
        let (hold, fails, undetermined) = self.policy;
        if hold + fails + undetermined > 0 {
            parts.push(format!(
                "policy: {}",
                super::policy::tally_text(hold, fails, undetermined)
            ));
        }
        format!("plan: {}", parts.join("; "))
    }

    /// The clauses for what `later` holds (R-193): its changes by kind and
    /// by what they wait on outside the plan, in `later`'s order (`21
    /// creates after platform[env=lab] is applied`). None when `later`
    /// holds no change.
    fn later_clauses(&self) -> Vec<String> {
        self.later
            .iter()
            .map(|(until, kinds)| {
                let kinds: Vec<String> = kinds
                    .iter()
                    .filter(|(_, n)| *n > 0)
                    .map(|(k, n)| count(*n, k))
                    .collect();
                format!("{} {until}", kinds.join(", "))
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

impl Report {
    /// What the headline counts of this plan.
    pub fn tally(&self) -> Tally {
        let ticks = self
            .sections()
            .values()
            .filter(|s| {
                !s.deposed.is_empty()
                    || !s.groups.is_empty()
                    || s.changes
                        .iter()
                        .any(|d| !matches!(d.kind, ActionKind::Noop))
            })
            .count();
        let later = self.later_changes();
        // Held objects `later` lists as state has them, no change of
        // theirs known yet (R-177): after the clauses when there are.
        let held = self
            .pending
            .iter()
            .filter(|b| b.resolves_after.is_none())
            .flat_map(|b| b.deformations.iter())
            .filter(|d| later.is_empty() || matches!(d.kind, ActionKind::Noop))
            .count();
        Tally {
            kinds: self.kinds(),
            ticks,
            noops: self.show_noop.then_some(self.noops),
            denied: self.denies.len(),
            approvals: self.approvals.len(),
            not_planned: self.not_planned.len(),
            held,
            conflicts: self.conflicts.len(),
            later,
            policy: super::policy::count(&self.policy),
        }
    }

    /// The headline of this plan alone.
    pub fn summary(&self) -> String {
        self.tally().text()
    }

    /// `later`'s changes by what they wait on outside the plan, in
    /// `later`'s order, each by kind; none when it holds no change.
    fn later_changes(&self) -> Vec<(String, Vec<(&'static str, usize)>)> {
        let mut changes: Vec<(String, Vec<&Deformation>)> = Vec::new();
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds = b
                .deformations
                .iter()
                .filter(|d| !matches!(d.kind, ActionKind::Noop));
            let until = until_text(&b.until);
            match changes.iter_mut().find(|(u, _)| *u == until) {
                Some((_, x)) => x.extend(ds),
                None => changes.push((until, ds.collect())),
            }
        }
        changes
            .into_iter()
            .filter(|(_, ds)| !ds.is_empty())
            .map(|(until, ds)| {
                let kinds = by_kind(ds.into_iter())
                    .into_iter()
                    .filter(|(_, n)| *n > 0)
                    .collect();
                (until, kinds)
            })
            .collect()
    }
}

/// The kinds of change a summary counts, in its order.
pub(super) const KINDS: [&str; 7] = [
    "create", "update", "replace", "drift", "delete", "adopt", "forget",
];

/// How many of `ds` are of each kind, in [`KINDS`] order.
pub(super) fn by_kind<'d>(ds: impl Iterator<Item = &'d Deformation>) -> Vec<(&'static str, usize)> {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for d in ds {
        *n.entry(kind_name(&d.kind)).or_default() += 1;
    }
    KINDS
        .into_iter()
        .map(|k| (k, n.get(k).copied().unwrap_or(0)))
        .collect()
}

/// `plan: 3 changes (2 create, 1 update)`: `n` changes, and those of the
/// `kinds` there are.
pub(super) fn changes_text(n: usize, kinds: &[(&str, usize)]) -> String {
    let mut out = format!("plan: {}", count(n, "change"));
    let ks: Vec<String> = kinds
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{n} {k}"))
        .collect();
    if !ks.is_empty() {
        out.push_str(&format!(" ({})", ks.join(", ")));
    }
    out
}

/// `n thing`, `n things`.
pub(super) fn count(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}
