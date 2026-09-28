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

use crate::fakecloud::FakeCloud;
use crate::ir::{Adopt, Resource};
use crate::provider::{ActionKind, Plan};
use crate::state::State;
use anyhow::Result;

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
