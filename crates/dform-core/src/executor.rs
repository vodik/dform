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
//!
//! Uncertainty. A call whose answer was lost (a timeout, a crash, in flight
//! when dform stopped) may have taken effect: it stays in
//! `State::uncertain`, and the next run resolves it before it plans
//! (`resolve_uncertain`). Every Create and Replace carries an idempotency
//! key, written to state with the in-flight record (`mark_creates`), so
//! the provider can say what it made, and a retry never makes it twice.
//!
//! Replacement. A replace makes a new object: every null that named the old
//! one (its id, every computed path) is retracted, so the program is
//! evaluated again without the replaced identities (`withhold`), and what
//! reads them is held on the replacement's nulls (`hold_dependents`) until
//! the boundary after the tick that creates it; the next tick updates it to
//! the new values. A create that reads them fills them in the same tick,
//! after the replacement. An object a `create_before_destroy` replacement
//! deposed is deleted only once nothing that depends on it is still held
//! (`hold_deposed`).

use crate::ast::{Atom, Term};
use crate::ir::{Address, Adopt, Resource};
use crate::lattice::nulls_in;
use crate::plan_print::waits_on;
use crate::plugin::Providers;
use crate::provider::{Action, ActionKind, Change, Plan, fmt_value};
use crate::state::{self, InFlight, State, Uncertain, UncertainOp};
use crate::stuck::Sections;
use crate::value::{Value, null_owner};
use crate::zset::Lifecycle;
use anyhow::{Result, bail};
use serde_json::Value as Json;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
    /// Chaos `stop-after=N`: how many more Apply calls may return before
    /// dform stops as if killed (`None`: no limit). Counted across the
    /// run's ticks.
    pub stop_after: Option<&'a Cell<usize>>,
    /// The audit log's hook: told how each action ended, once state has
    /// been written for it (`report_action`).
    pub on_action: Option<ActionHook<'a>>,
}

/// How an action ended: its error, if it failed; state as written after it.
pub type ActionHook<'a> = &'a dyn Fn(&Action, Option<&anyhow::Error>, &State);

/// Apply every definite action of `plan` as one tick of the world, walking
/// the tick's dependency DAG with at most `parallel` calls in flight. The
/// first failure stops new calls; those in flight finish, and every action
/// that answered keeps its identity. Returns what the answered Apply calls
/// returned.
///
/// The walk is single-threaded: submit every ready action while fewer than
/// `parallel` calls are in flight (in plan order), then take the next
/// answer, persist, and release what waited on it. Which call answers next
/// is the backend's: the process backend's calls run at once; the direct
/// and wire backends answer in simulated time (`plugin::queue`). Each call
/// is put on the executor's clock from its answer's `elapsed_ms` (the
/// mock's chaos `latency`): it starts when it is submitted and ends that
/// much later, and the clock moves to the end of each call as it is
/// taken. The world records the spans. With `parallel` 1 the calls run in
/// plan order, one after the other.
pub fn run_tick(
    cloud: &Providers,
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
    let mut start_ms = vec![0; actions.len()];
    // The order the calls were submitted in.
    let mut seq = vec![0; actions.len()];
    // Answered at submit: no call was needed.
    let mut at_once: VecDeque<usize> = VecDeque::new();
    // (action, start, end) of every call taken, for the world's timeline.
    let mut spans: Vec<(usize, u64, u64)> = Vec::new();
    let mut in_flight = 0;
    let mut now = 0;
    let mut failed = None;
    loop {
        while failed.is_none() && in_flight + at_once.len() < opts.parallel.max(1) {
            let Some(i) =
                (0..actions.len()).find(|&i| !started[i] && waits[i].iter().all(|&j| done[j]))
            else {
                break;
            };
            started[i] = true;
            start_ms[i] = now;
            seq[i] = started.iter().filter(|s| **s).count();
            match tick.submit(i, actions[i], state) {
                Ok(true) => in_flight += 1,
                Ok(false) => at_once.push_back(i),
                Err(e) => {
                    (opts.persist)(state)?;
                    report_action(opts, actions[i], Some(&e), state);
                    spans.push((i, now, now));
                    failed = Some(e);
                }
            }
        }
        let (i, r, called) = match at_once.pop_front() {
            Some(i) => (i, Ok(()), false),
            None if tick.busy() => {
                in_flight -= 1;
                let (i, r) = tick.next_completed(state);
                (i, r, true)
            }
            None => break,
        };
        let a = actions[i];
        if r.is_ok()
            && let Some(f) = &mut state.in_flight
        {
            f.remaining.remove(&state::key(&a.addr));
        }
        persist_answered(opts, state)?;
        report_action(opts, a, r.as_ref().err(), state);
        let end = start_ms[i] + tick.latency(&a.addr);
        spans.push((i, start_ms[i], end));
        match r {
            Ok(()) => {
                done[i] = true;
                now = now.max(end);
            }
            Err(e) => {
                failed.get_or_insert(e);
            }
        }
        if let Some(left) = opts.stop_after.filter(|_| called) {
            left.set(left.get().saturating_sub(1));
            if left.get() == 0 {
                // As if dform were killed here: nothing in flight is
                // waited for, and the tick never ends.
                bail!(
                    "apply {}/{}: dform stopped after this Apply call returned \
                     (chaos stop-after); the next apply resumes",
                    a.addr.typ,
                    a.addr.name
                );
            }
        }
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
    // The timeline in the order the calls started.
    spans.sort_by_key(|&(i, _, _)| seq[i]);
    for (i, start, end) in spans {
        tick.record(&actions[i].addr, start, end);
    }
    let returned = tick.end(state)?;
    (opts.persist)(state)?;
    match failed {
        Some(e) => Err(e),
        None => Ok(returned),
    }
}

/// State after an Apply call returned. A `test-hooks` build (never the
/// `dform` binary's: only tests turn the feature on, as a dev-dependency)
/// can revert it to one write per tick, the end's, for the model test to
/// catch (`tests/model.rs`).
fn persist_answered(opts: &Options, state: &State) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    if hooks::PERSIST_PER_TICK.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    (opts.persist)(state)
}

/// Mutants the model test must catch; off unless a test sets one.
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use std::sync::atomic::AtomicBool;

    /// Persist once per tick, at its end, instead of after every Apply
    /// call that returns.
    pub static PERSIST_PER_TICK: AtomicBool = AtomicBool::new(false);
}

/// `approver_allowed(Who, D)`: may `Who` approve the deformation `D`?
pub type Allowed<'a> = &'a dyn Fn(&str, &str) -> bool;

/// Tell the audit hook how `a` ended.
fn report_action(opts: &Options, a: &Action, err: Option<&anyhow::Error>, state: &State) {
    if let Some(hook) = opts.on_action {
        hook(a, err, state);
    }
}

/// Apply's entry check, before any Apply call (README "Approvals"): verify
/// `token` against the stack's trust root `roots` and what this apply is
/// (`expect`: the plan digest, the stack and its key, the time), and, when
/// the program restricts approvers (`allowed`), that the approver may
/// approve every deformation `needs` names. The error names what failed.
pub fn approve(
    token: &str,
    needs: &[(String, String)],
    roots: &[(jsonwebtoken::jwk::JwkSet, Option<String>)],
    expect: &crate::approval::Expect,
    allowed: Option<Allowed>,
) -> Result<crate::approval::Verified> {
    let v = crate::approval::verify(token, roots, expect)?;
    if let Some(allowed) = allowed {
        let who = &v.statement.approver;
        let refused: Vec<&str> = needs
            .iter()
            .filter(|(d, _)| !allowed(who, d))
            .map(|(d, _)| d.as_str())
            .collect();
        if !refused.is_empty() {
            bail!(
                "approval by {who}: approver_allowed({who:?}, D) does not hold for {}",
                refused.join(", ")
            );
        }
    }
    Ok(v)
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

/// The addresses `plan` replaces.
pub fn replaced(plan: &Plan) -> BTreeSet<Address> {
    plan.actions
        .iter()
        .filter(|a| matches!(a.kind, ActionKind::Replace { .. }))
        .map(|a| a.addr.clone())
        .collect()
}

/// Refresh facts without the identities of `withheld`: round 0 resolves
/// none of their nulls, so what reads them sees the nulls again.
pub fn withhold(facts: Vec<Atom>, withheld: &BTreeSet<Address>) -> Vec<Atom> {
    facts
        .into_iter()
        .filter(|f| {
            let [Term::Val(Value::Str(typ)), Term::Val(Value::Str(name)), _] = f.args.as_slice()
            else {
                return true;
            };
            f.pred != "identity"
                || !withheld.contains(&Address {
                    typ: typ.clone(),
                    name: name.clone(),
                })
        })
        .collect()
}

/// Hold every update of an existing object whose document carries a null a
/// replaced address owns: it runs the tick after the replacement, with the
/// new value. A create is not held; the executor fills the null after the
/// replacement in the same tick.
pub fn hold_dependents(plan: &mut Plan, desired: &[Resource], replaced: &BTreeSet<Address>) {
    for a in plan.actions.iter_mut() {
        if replaced.contains(&a.addr)
            || !matches!(
                a.kind,
                ActionKind::Update
                    | ActionKind::Drift
                    | ActionKind::Pending
                    | ActionKind::Replace { .. }
            )
        {
            continue;
        }
        let Some(r) = desired.iter().find(|r| r.addr == a.addr) else {
            continue;
        };
        let on: BTreeSet<String> = nulls_in(&r.attrs)
            .into_iter()
            .filter(|l| {
                null_owner(l).is_some_and(|(typ, name)| replaced.contains(&Address { typ, name }))
            })
            .collect();
        if !on.is_empty() {
            a.kind = ActionKind::Pending;
            a.on.extend(on);
        }
    }
}

/// A deposed object is deleted only after what depended on it has moved to
/// the replacement: while a desired object that references its address is
/// held, the delete is held on the same nulls.
pub fn hold_deposed(plan: &mut Plan, desired: &[Resource], sections: &Sections) {
    let held: Vec<(Address, Vec<String>)> = plan
        .actions
        .iter()
        .filter_map(|a| Some((a.addr.clone(), waits_on(a, sections)?)))
        .collect();
    for a in plan
        .actions
        .iter_mut()
        .filter(|a| matches!(a.kind, ActionKind::DeleteDeposed))
    {
        for (addr, on) in &held {
            if desired
                .iter()
                .any(|r| &r.addr == addr && r.deps.contains(&a.addr))
            {
                a.on.extend(on.iter().cloned());
            }
        }
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

/// Give every Create and Replace of the tick (`actions`, the definite ones)
/// its idempotency key before the tick's first call, so that the state
/// written with the in-flight record already names it: a call in flight
/// when dform is killed is uncertain on disk (`State::uncertain`). One that
/// was uncertain already keeps its key: the retry must send the same one.
pub fn mark_creates<'a>(
    state: &mut State,
    stack: &str,
    actions: impl IntoIterator<Item = &'a Action>,
) {
    for a in actions {
        let op = match a.kind {
            ActionKind::Create => UncertainOp::Create,
            ActionKind::Replace { create_first } => UncertainOp::Replace { create_first },
            _ => continue,
        };
        let k = state::key(&a.addr);
        let key = state
            .uncertain
            .get(&k)
            .map(|u| u.key.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| state.new_idempotency_key(stack, &a.addr, a));
        let remote = state
            .get(&a.addr)
            .map(|e| e.remote.clone())
            .unwrap_or_default();
        state.uncertain.insert(k, Uncertain { op, remote, key });
    }
}

/// Resolve every uncertain Apply call (`State::uncertain`) before anything
/// is planned, so that a call whose answer was lost is neither repeated nor
/// forgotten. Returns what was resolved, a line each.
///
/// * A create or a replace: ask the provider for the object its idempotency
///   key made (`Providers::created`). Found, it is the address's: state maps
///   it (a create-first replacement's old object deposed), and the action
///   is no longer remaining. Not found, the call is retried with the same
///   key: the entry stays while an apply of it is outstanding (the
///   in-flight record lists the address), else it goes. A destroy-first
///   replacement whose old object is gone becomes a create.
/// * An update: its outcome is whatever the refresh Reads. The plan is
///   computed from it, so the update is no longer remaining: the world
///   moving under it is its own doing, not someone else's.
/// * A delete: a Read. Gone, state forgets it; there, the plan deletes it
///   again.
pub fn resolve_uncertain(cloud: &Providers, state: &mut State) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let outstanding = |state: &State, k: &str| {
        state
            .in_flight
            .as_ref()
            .is_some_and(|f| f.remaining.contains_key(k))
    };
    let done = |state: &mut State, k: &str| {
        state.uncertain.remove(k);
        if let Some(f) = &mut state.in_flight {
            f.remaining.remove(k);
        }
    };
    for (k, u) in state.uncertain.clone() {
        let deposed = matches!(u.op, UncertainOp::DeleteDeposed);
        let Some(addr) = state::parse_key(k.strip_suffix("#deposed").unwrap_or(&k)) else {
            state.uncertain.remove(&k);
            continue;
        };
        let at = format!("{}/{}", addr.typ, addr.name);
        match u.op {
            UncertainOp::Create | UncertainOp::Replace { .. } => {
                // A create's address may still map an object that is gone.
                let mapped = state.get(&addr).map(|e| e.remote.clone());
                if let Some(remote) = cloud.created(&addr, &u.key)? {
                    if let UncertainOp::Replace { create_first: true } = u.op
                        && mapped.as_ref().is_some_and(|m| *m != remote)
                    {
                        state.depose(&addr);
                    }
                    let provider = cloud.provider_of(&addr.typ).to_string();
                    state.set(addr.clone(), provider, remote.clone());
                    done(state, &k);
                    out.push(format!(
                        "{at}: the {} whose answer was lost made {remote}; state maps it",
                        op_name(&u.op)
                    ));
                    continue;
                }
                // A destroy-first replacement that deleted the old object
                // and made nothing: what is left is a create.
                if let UncertainOp::Replace {
                    create_first: false,
                } = u.op
                    && mapped.is_some_and(|m| m == u.remote)
                    && !cloud.exists(&addr, &u.remote)?
                {
                    state.remove(&addr);
                    if let Some(e) = state.uncertain.get_mut(&k) {
                        e.op = UncertainOp::Create;
                        e.remote.clear();
                    }
                    out.push(format!(
                        "{at}: the replace whose answer was lost deleted {} and made nothing;                          it is created again",
                        u.remote
                    ));
                    continue;
                }
                if !outstanding(state, &k) {
                    state.uncertain.remove(&k);
                }
            }
            UncertainOp::Update => done(state, &k),
            UncertainOp::Delete | UncertainOp::DeleteDeposed => {
                let gone = !cloud.exists(&addr, &u.remote)?;
                if gone {
                    match deposed {
                        true => {
                            state.deposed.remove(&state::key(&addr));
                        }
                        false => state.remove(&addr),
                    }
                    out.push(format!(
                        "{at}: the delete whose answer was lost took effect; {} is gone",
                        u.remote
                    ));
                }
                done(state, &k);
            }
        }
    }
    Ok(out)
}

fn op_name(op: &UncertainOp) -> &'static str {
    match op {
        UncertainOp::Create => "create",
        UncertainOp::Replace { .. } => "replace",
        UncertainOp::Update => "update",
        UncertainOp::Delete | UncertainOp::DeleteDeposed => "delete",
    }
}

/// The addresses whose world document is no longer the one they were
/// planned against, with what changed (planned -> now). For resume the
/// documents are the in-flight record's; at a phase boundary, what the
/// executor last saw (`Seen`).
pub fn changed_under(
    cloud: &Providers,
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
/// saw. A change anywhere but under a held deformation is drift: it is
/// reported and the run goes on; the next tick's plan deforms it back. The
/// held deformations come back as facts for the boundary's evaluation,
/// `deformation(pending, T, A, Before)` with the document each was planned
/// against and `world_digest(T, A, Now)`, and the evaluator derives the deny
/// when the world moved under one (`zset::POLICY_RULES`): the pending diff
/// was computed against a document that no longer exists. That change is
/// printed here, the deny stops the run.
pub fn check_boundary(
    cloud: &Providers,
    seen: &Seen,
    pending: &BTreeSet<Address>,
    state: &State,
    tick: usize,
) -> Result<Vec<Atom>> {
    let observed = cloud.observe(state)?;
    let changed = changed_under(cloud, seen, &observed);
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
    }
    let before: BTreeMap<Address, Option<Json>> = pending
        .iter()
        .map(|a| (a.clone(), seen.get(a).cloned().flatten()))
        .collect();
    Ok(crate::zset::deformation_facts(
        pending.iter().map(|a| ("pending", a)),
        &before,
        &observed,
    ))
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
