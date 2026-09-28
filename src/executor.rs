//! The batch executor: one tick of a plan against the provider (E §2.7,
//! §2.8).
//!
//! The executor decides the order of the provider's per-resource Apply calls
//! and when dform's own state is written. Persistence points:
//!
//! * after every Apply call that returns, answered or not: `identity` for a
//!   create or adopt, its removal for a delete. A failure at action N leaves
//!   the N-1 identities before it in state; a crash loses at most the call in
//!   flight.
//! * at the end of the tick, once the world's clock has advanced.
//!
//! Resume. Before a tick's first call, its deformations are written to
//! state as in flight (`State::in_flight`), each with the world document it
//! was planned against; an answered call takes its action out. An apply that
//! fails or is killed leaves the rest there. The next `apply` refreshes and
//! compares: if the world moved under a remaining action it reports the
//! change and stops (`changed_under`); otherwise it finishes the remaining
//! actions from a fresh evaluation, where the finished ones cancel. The check
//! takes only the remaining documents, so a saved plan file can feed it the
//! same way.

use crate::fakecloud::FakeCloud;
use crate::ir::{Address, Adopt, Resource};
use crate::provider::{ActionKind, Change, Plan, fmt_value};
use crate::state::{self, InFlight, State};
use anyhow::{Result, bail};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// Where state goes after every Apply call.
pub type Persist<'a> = &'a dyn Fn(&State) -> Result<()>;

/// What the world holds per address as the executor last saw it: the
/// refresh a tick was planned from, overlaid with what its Apply calls
/// returned (`None`: deleted).
pub type Seen = BTreeMap<Address, Option<Json>>;

/// Apply every definite action of `plan` as one tick of the world. The
/// first failure stops the tick; the actions before it keep their identity.
/// Returns what the answered Apply calls returned.
pub fn run_tick(
    cloud: &FakeCloud,
    desired: &[Resource],
    adopts: &[Adopt],
    state: &mut State,
    plan: &Plan,
    persist: Persist,
) -> Result<Seen> {
    let mut tick = cloud.begin_tick(desired, adopts)?;
    let mut failed = None;
    for a in &plan.actions {
        if matches!(a.kind, ActionKind::Noop | ActionKind::Pending) {
            continue;
        }
        let r = tick.apply(a, state);
        if r.is_ok()
            && let Some(f) = &mut state.in_flight
        {
            f.remaining.remove(&state::key(&a.addr));
        }
        persist(state)?;
        if let Err(e) = r {
            failed = Some(e);
            break;
        }
    }
    let returned = tick.end(state)?;
    persist(state)?;
    match failed {
        Some(e) => Err(e),
        None => Ok(returned),
    }
}

/// Mark the tick's deformations in flight: every action of `plan` that is
/// not a no-op, held ones included, with the world document `observed` has
/// for it.
pub fn begin(state: &mut State, tick: usize, plan: &Plan, observed: &BTreeMap<Address, Json>) {
    let remaining = plan
        .actions
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop))
        .map(|a| (state::key(&a.addr), observed.get(&a.addr).cloned()))
        .collect();
    state.in_flight = Some(InFlight { tick, remaining });
}

/// The addresses whose world document is no longer the one they were
/// planned against, with what changed (planned -> now). For resume the
/// documents are the in-flight record's; at a phase boundary, what the
/// executor last saw (`Seen`).
pub fn changed_under(
    cloud: &FakeCloud,
    expected: &Seen,
    observed: &BTreeMap<Address, Json>,
) -> Vec<(Address, Vec<Change>)> {
    let mut out = Vec::new();
    for (addr, planned) in expected {
        let now = observed.get(addr);
        let changes = cloud.diff(&addr.typ, planned.as_ref(), now);
        if !changes.is_empty() || planned.is_some() != now.is_some() {
            out.push((addr.clone(), changes));
        }
    }
    out
}

/// The in-flight record's remaining deformations as `Seen`.
pub fn remaining(f: &InFlight) -> Seen {
    f.remaining
        .iter()
        .filter_map(|(k, d)| Some((state::parse_key(k)?, d.clone())))
        .collect()
}

/// At a phase boundary: refresh and compare with what the executor last
/// saw. The world changing under an address whose deformation is pending
/// (held for this boundary) stops the run before the next tick, with the
/// change printed: the pending diff was computed against a document that
/// no longer exists. A change anywhere else is drift: it is reported and
/// the run goes on; the next tick's plan deforms it back.
pub fn check_boundary(
    cloud: &FakeCloud,
    seen: &Seen,
    pending: &BTreeSet<Address>,
    state: &State,
    tick: usize,
) -> Result<()> {
    let changed = changed_under(cloud, seen, &cloud.observe(state)?);
    let (under, drift): (Vec<_>, Vec<_>) =
        changed.into_iter().partition(|(a, _)| pending.contains(a));
    if !drift.is_empty() {
        print!("drift after tick {tick}:\n{}", format_changes(&drift));
    }
    if !under.is_empty() {
        eprint!(
            "the world changed under a pending deformation after tick {tick}:\n{}",
            format_changes(&under)
        );
        bail!("apply stopped after tick {tick}: the world changed under a pending deformation");
    }
    Ok(())
}

/// `~ T.N` and a `path: before -> after` line per change, as plan prints an
/// update; a resource that appeared or vanished says so.
pub fn format_changes(changed: &[(Address, Vec<Change>)]) -> String {
    let mut out = String::new();
    for (addr, changes) in changed {
        out.push_str(&format!("~ {}.{}\n", addr.typ, addr.name));
        for ch in changes {
            let side = |v: Option<&Json>| match (ch.sensitive, v) {
                (true, Some(_)) => "(sensitive)".to_string(),
                _ => fmt_value(v),
            };
            out.push_str(&format!(
                "  {}: {} -> {}\n",
                ch.path,
                side(ch.before.as_ref()),
                side(ch.after.as_ref())
            ));
        }
    }
    out
}
