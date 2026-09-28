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
use anyhow::Result;
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// Where state goes after every Apply call.
pub type Persist<'a> = &'a dyn Fn(&State) -> Result<()>;

/// Apply every definite action of `plan` as one tick of the world. The
/// first failure stops the tick; the actions before it keep their identity.
pub fn run_tick(
    cloud: &FakeCloud,
    desired: &[Resource],
    adopts: &[Adopt],
    state: &mut State,
    plan: &Plan,
    persist: Persist,
) -> Result<()> {
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
    tick.end(state)?;
    persist(state)?;
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
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

/// The remaining deformations whose world document is no longer the one
/// they were planned against, with what changed (planned -> now).
pub fn changed_under(
    cloud: &FakeCloud,
    remaining: &BTreeMap<String, Option<Json>>,
    observed: &BTreeMap<Address, Json>,
) -> Vec<(Address, Vec<Change>)> {
    let mut out = Vec::new();
    for (k, planned) in remaining {
        let Some(addr) = state::parse_key(k) else {
            continue;
        };
        let now = observed.get(&addr);
        let changes = cloud.diff(&addr.typ, planned.as_ref(), now);
        if !changes.is_empty() || planned.is_some() != now.is_some() {
            out.push((addr, changes));
        }
    }
    out
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
