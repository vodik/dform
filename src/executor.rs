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
//! Order. The tick's actions form a DAG (`dag`): a create or update after
//! what its document references, deletes after everything else and each
//! before the deletes of what it depended on. `--parallel N` walks it with
//! at most N Apply calls in flight.
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
use crate::provider::{Action, ActionKind, Change, Plan, fmt_value};
use crate::state::{self, InFlight, State};
use crate::zset::Lifecycle;
use anyhow::{Result, bail};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// Where state goes after every Apply call.
pub type Persist<'a> = &'a dyn Fn(&State) -> Result<()>;

/// What the world holds per address as the executor last saw it: the
/// refresh a tick was planned from, overlaid with what its Apply calls
/// returned (`None`: deleted).
pub type Seen = BTreeMap<Address, Option<Json>>;

/// How a tick runs.
pub struct Options<'a> {
    /// At most this many Apply calls in flight (`--parallel`, at least 1).
    pub parallel: usize,
    /// Where state goes after every Apply call.
    pub persist: Persist<'a>,
}

/// Apply every definite action of `plan` as one tick of the world, walking
/// the tick's dependency DAG with at most `parallel` calls in flight. The
/// first failure stops new calls; those in flight finish, and every action
/// that answered keeps its identity. Returns what the answered Apply calls
/// returned.
///
/// Time is the mock's simulated clock (chaos `latency`), so the walk is a
/// deterministic simulation: a call starts, in plan order among the ready
/// ones, as soon as a slot is free and everything it waits on has finished;
/// it finishes `latency` later. The world records each call's span. With
/// `parallel` 1 the calls run in plan order, one after the other.
pub fn run_tick(
    cloud: &FakeCloud,
    desired: &[Resource],
    adopts: &[Adopt],
    lifecycle: &Lifecycle,
    state: &mut State,
    plan: &Plan,
    opts: &Options,
) -> Result<Seen> {
    let actions: Vec<&Action> = plan
        .actions
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop | ActionKind::Pending))
        .collect();
    let waits = dag(&actions, desired, state);
    let mut tick = cloud.begin_tick(desired, adopts, lifecycle)?;
    let mut started = vec![false; actions.len()];
    let mut done = vec![false; actions.len()];
    // (finishes at, action), in start order.
    let mut running: Vec<(u64, usize)> = Vec::new();
    let mut now = 0;
    let mut failed = None;
    loop {
        while failed.is_none() && running.len() < opts.parallel.max(1) {
            let Some(i) =
                (0..actions.len()).find(|&i| !started[i] && waits[i].iter().all(|&j| done[j]))
            else {
                break;
            };
            started[i] = true;
            let a = actions[i];
            let r = tick.apply(a, state);
            if r.is_ok()
                && let Some(f) = &mut state.in_flight
            {
                f.remaining.remove(&state::key(&a.addr));
            }
            (opts.persist)(state)?;
            let end = now + tick.latency(&a.addr);
            tick.record(&a.addr, now, end);
            match r {
                Ok(()) => running.push((end, i)),
                Err(e) => failed = Some(e),
            }
        }
        // The next call to finish; ties in start order.
        let Some(k) = (0..running.len()).min_by_key(|&k| running[k].0) else {
            break;
        };
        let (end, i) = running.remove(k);
        now = end;
        done[i] = true;
    }
    if failed.is_none()
        && let Some(i) = started.iter().position(|s| !s)
    {
        let a = &actions[i].addr;
        failed = Some(anyhow::anyhow!(
            "apply {}/{}: its dependencies never finished (a cycle)",
            a.typ,
            a.name
        ));
    }
    let returned = tick.end(state)?;
    (opts.persist)(state)?;
    match failed {
        Some(e) => Err(e),
        None => Ok(returned),
    }
}

/// For each action, the actions it waits for. A create, update or replace
/// waits for the actions at the addresses its document references. Deletes
/// wait for every other kind of action, and a delete waits for the deletes
/// of the objects that depended on it (state's recorded dependencies).
fn dag(actions: &[&Action], desired: &[Resource], state: &State) -> Vec<Vec<usize>> {
    let is_delete = |a: &Action| matches!(a.kind, ActionKind::Delete | ActionKind::DeleteDeposed);
    let deps: Vec<Vec<String>> = actions
        .iter()
        .map(|a| match a.kind {
            ActionKind::Delete => state.get(&a.addr).map(|e| e.deps.clone()),
            ActionKind::DeleteDeposed => state
                .deposed
                .get(&state::key(&a.addr))
                .map(|e| e.deps.clone()),
            _ => desired
                .iter()
                .find(|r| r.addr == a.addr)
                .map(|r| r.deps.iter().map(state::key).collect()),
        })
        .map(Option::unwrap_or_default)
        .collect();
    (0..actions.len())
        .map(|i| {
            let me = state::key(&actions[i].addr);
            (0..actions.len())
                .filter(|&j| j != i)
                .filter(|&j| {
                    if !is_delete(actions[i]) {
                        !is_delete(actions[j]) && deps[i].contains(&state::key(&actions[j].addr))
                    } else {
                        !is_delete(actions[j]) || deps[j].contains(&me)
                    }
                })
                .collect()
        })
        .collect()
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
