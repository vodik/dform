//! The batch executor: one tick of a plan against the provider (E §2.7,
//! §2.8).
//!
//! The executor decides the order of the provider's per-resource Apply calls
//! and when dform's own state is written (`persist`: the caller's; the
//! command line logs each change of state to the audit log, its write-ahead
//! log, and checkpoints the state per tick, `wal`). Persistence points:
//!
//! * after every Apply call that returns, answered or not: `identity` for a
//!   create or adopt, its removal for a delete, in the order the calls
//!   answer. A failure leaves every identity answered before it in state;
//!   a crash loses at most the calls in flight, `--parallel` of them (with
//!   1, a failure at action N leaves the N-1 before it).
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
//! after the replacement. An object a create-first replacement deposed is
//! deleted only once nothing that depends on it is still held
//! (`hold_deposed`), in a later tick than the replacement's. Where the
//! type's name is its identity, the replacement is given the next
//! generation of it (R-189, `state::next_name`), so the two exist at once,
//! and what reads the name moves to the new one before the old goes.

use crate::address::Address;
use crate::ast::{Atom, Term};
use crate::lattice::nulls_in;
use crate::plugin::providers::{Providers, Tick};
use crate::provider::{Action, ActionKind, Change, Plan, fmt_value};
use crate::report::waits_on;
use crate::resources::{Adopt, Resource};
use crate::state::{self, InFlight, State, Uncertain, UncertainOp};
use crate::stuck::Sections;
use crate::value::{Value, null_owner};
use crate::zset::Lifecycle;
use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

mod together;

pub use together::Together;

/// What the world holds per address as the executor last saw it: the
/// refresh a tick was planned from, overlaid with what its Apply calls
/// returned (`None`: deleted).
pub type Seen = BTreeMap<Address, Option<Json>>;

/// How a tick runs.
pub struct Options<'a> {
    /// At most this many Apply calls in flight (`--parallel`, at least 1):
    /// of a tree's tick, of every part (`beside`).
    pub parallel: usize,
    /// Chaos `stop-after=N`: how many more Apply calls may return before
    /// dform stops as if killed (`None`: no limit). Counted across the
    /// run's ticks, a tree's across its parts.
    pub stop_after: Option<&'a AtomicUsize>,
    /// Of a tree's tick: the walks of its other parts, each on a thread
    /// of its own.
    pub beside: Option<Beside<'a>>,
}

/// A part's walk among a tree's tick's ([`Together`]): which it is.
#[derive(Clone, Copy)]
pub struct Beside<'a> {
    pub tick: &'a Together,
    pub part: usize,
}

/// One deployment's part of a tick ([`run_tick`]): its plan's definite
/// actions, made by its providers, the state their answers are written
/// to, and how it is told.
pub struct Part<'a> {
    /// The deployment, as a reader of it names it (`provider::Held`'s
    /// `deployment`).
    pub name: &'a str,
    pub cloud: &'a Providers,
    pub desired: &'a [Resource],
    pub adopts: &'a [Adopt],
    pub lifecycle: &'a Lifecycle,
    pub state: &'a mut State,
    pub plan: &'a Plan,
    pub hooks: &'a dyn Hooks,
}

/// What a part's calls tell as they go: where its state is written, its
/// audit log, its fence, its progress.
pub trait Hooks {
    /// Where state goes after every Apply call: durable when it returns.
    fn persist(&self, state: &State) -> Result<()>;

    /// How action `a` ended, its error if it failed, once state has been
    /// written for it: the audit log.
    fn action(&self, _a: &Action, _err: Option<&anyhow::Error>, _state: &State) {}

    /// Asked before each Apply call is submitted: an error stops the tick
    /// as the call's failure, and no call is made (the lease fence,
    /// `store::Deployment::check_fence`).
    fn before_submit(&self) -> Result<()> {
        Ok(())
    }

    /// Each change of state of an action (R-127): apply's progress.
    fn event(&self, _e: Event) {}
}

/// What a part's walk came to ([`run_tick`]).
pub enum Walked {
    /// Its tick ended: what its answered calls returned, or why its tick
    /// stopped.
    Ended(Result<Seen>),
    /// The walk stopped with its tick not ended: its state could not be
    /// written, or chaos `stop-after` stopped dform as if killed (in a
    /// tree's tick, maybe another part's walk: [`Together::stopped`]
    /// says which).
    Stopped(anyhow::Error),
}

impl Walked {
    /// What the part's answered calls returned, or why its tick or its
    /// walk stopped.
    pub fn applied(self) -> Result<Seen> {
        match self {
            Walked::Ended(r) => r,
            Walked::Stopped(e) => Err(e),
        }
    }
}

/// Whether chaos `stop-after` (`left`) has stopped dform as if killed.
pub fn killed(left: Option<&AtomicUsize>) -> bool {
    left.is_some_and(|l| l.load(Ordering::SeqCst) == 0)
}

/// A change of state of one of the tick's actions (R-127). A heartbeat
/// is the progress driver's own: the walk blocks on the call it waits
/// for, so its time ticks where it is printed.
#[derive(Debug)]
pub enum Event<'a> {
    /// Its Apply call was submitted.
    Started(&'a Address),
    /// It answered.
    Finished(&'a Address),
    /// It failed: no new call starts after it.
    Failed(&'a Address, &'a anyhow::Error),
    /// Its provider said how it goes (R-130): a status word to show beside
    /// it as it is, and a message for the log.
    Progress(&'a Address, &'a crate::plugin::pb::Event),
}

/// Apply every definite action of `part`'s plan as one tick of the world,
/// walking its dependency DAG with at most `parallel` calls in flight: a
/// deployment's apply walks its one part; a tree's walks one part per
/// deployment whose tick it is, each on its own thread, together
/// (`beside`: one window, one stop, and a reader's call that reads a
/// secret another part holds waits for the call that changes it). The
/// first failure stops new calls; those in flight finish, and every
/// action that answered keeps its identity in the part's state. Returns
/// what the part's answered Apply calls returned.
///
/// The walk is single-threaded: submit every ready call, in plan order,
/// while the window has room, then take the next answer, persist the
/// state, and release what waited on it. Which call answers next is the
/// backend's: the process backend's calls run at once; the direct and
/// wire backends answer in simulated time (`plugin::queue`). Each call is
/// put on the executor's clock from its answer's `elapsed_ms` (the mock's
/// chaos `latency`): it starts when it is submitted and ends that much
/// later, and the clock moves to the end of each call as it is taken. The
/// world records the spans. With `parallel` 1 the calls run one after
/// the other, in plan order.
pub fn run_tick(part: Part, opts: &Options) -> Walked {
    let beside = opts.beside;
    let walked = Walk::new(part, opts).run();
    if let Some(b) = beside {
        if let Walked::Stopped(_) = walked {
            b.tick.stop(b.part);
        }
        b.tick.end(b.part);
    }
    walked
}

/// A tick's walk of its part's dependency DAG ([`run_tick`]): its calls,
/// where each is, the calls in flight, the executor's clock, and the
/// part's first failure.
struct Walk<'a, 'o> {
    part: Part<'a>,
    calls: Vec<&'a Action>,
    /// What each call waits on, by index into `calls`.
    waits: Vec<Vec<usize>>,
    /// What each call waits on in the tree's other parts: their calls,
    /// by part and address.
    beside: Vec<Vec<(usize, Address)>>,
    tick: Tick<'a>,
    opts: &'o Options<'o>,
    started: Vec<bool>,
    done: Vec<bool>,
    start_ms: Vec<u64>,
    /// The order the calls were submitted in.
    seq: Vec<usize>,
    /// Answered at submit: no call was needed.
    at_once: VecDeque<usize>,
    /// (call, start, end) of every call taken, for the world's timeline.
    spans: Vec<(usize, u64, u64)>,
    in_flight: usize,
    now: u64,
    /// The first failure: after it, no new call starts.
    failed: Option<anyhow::Error>,
}

impl<'a, 'o> Walk<'a, 'o> {
    fn new(part: Part<'a>, opts: &'o Options<'o>) -> Walk<'a, 'o> {
        let calls: Vec<&'a Action> = part
            .plan
            .actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop | ActionKind::Pending))
            .collect();
        let waits = dag(&calls, part.desired, part.state);
        let beside = match opts.beside {
            Some(b) => cross(&part, &calls, b),
            None => calls.iter().map(|_| Vec::new()).collect(),
        };
        let tick = Tick::new(vec![part.cloud.tick_part(
            part.desired,
            part.adopts,
            part.lifecycle,
        )]);
        let n = calls.len();
        Walk {
            part,
            calls,
            waits,
            beside,
            tick,
            opts,
            started: vec![false; n],
            done: vec![false; n],
            start_ms: vec![0; n],
            seq: vec![0; n],
            at_once: VecDeque::new(),
            spans: Vec::new(),
            in_flight: 0,
            now: 0,
            failed: None,
        }
    }

    /// The walk: ready calls submitted, answers taken, until none is left
    /// that can start; in a tree's tick, waiting on the other parts' walks
    /// for room in the window or a call of theirs.
    fn run(mut self) -> Walked {
        loop {
            let seen = self.opts.beside.map(|b| b.tick.generation());
            if let Some(stopped) = self.stopped_beside() {
                return stopped;
            }
            if let Err(stopped) = self.submit_ready() {
                return stopped;
            }
            match self.take() {
                Some((c, r, called)) => {
                    if let Err(stopped) = self.answered(c, r, called) {
                        return stopped;
                    }
                }
                None => match (self.opts.beside, seen) {
                    (Some(b), Some(seen)) if self.waits_beside() => b.tick.wait(seen),
                    _ => break,
                },
            }
        }
        self.end()
    }

    /// Whether a call failed, of this part or (a tree's tick) another's:
    /// no new call starts.
    fn stopping(&self) -> bool {
        self.failed.is_some() || self.opts.beside.is_some_and(|b| b.tick.failed())
    }

    /// In a tree's tick, another part's walk stopped dform as if killed:
    /// this one stops so too, its calls in flight left to its write-ahead
    /// record.
    fn stopped_beside(&self) -> Option<Walked> {
        let b = self.opts.beside?;
        let part = b.tick.stopped()?;
        Some(Walked::Stopped(anyhow::anyhow!(
            "internal: stopped with part {part} of the tree's tick"
        )))
    }

    /// Submit every ready call, in order, while the window has room and
    /// no call has failed.
    fn submit_ready(&mut self) -> Result<(), Walked> {
        while !self.stopping() {
            let Some(c) = self.ready() else {
                break;
            };
            if !self.take_slot() {
                break;
            }
            self.submit(c)?;
        }
        Ok(())
    }

    /// Room in the window for a call: this walk's, or (a tree's tick) the
    /// tick's, taken.
    fn take_slot(&self) -> bool {
        match self.opts.beside {
            Some(b) => b.tick.take_slot(),
            None => self.in_flight + self.at_once.len() < self.opts.parallel.max(1),
        }
    }

    /// A call taken, or not sent: in a tree's tick, its room in the
    /// window free.
    fn free_slot(&self) {
        if let Some(b) = self.opts.beside {
            b.tick.free_slot();
        }
    }

    /// The first call not started whose dependencies are done, this
    /// part's and the other parts'.
    fn ready(&self) -> Option<usize> {
        (0..self.calls.len()).find(|&c| {
            !self.started[c]
                && self.waits[c].iter().all(|&j| self.done[j])
                && self.answered_beside(c) == Some(true)
        })
    }

    /// Whether what call `c` waits on in the other parts answered; `None`
    /// once it never will.
    fn answered_beside(&self, c: usize) -> Option<bool> {
        match (self.opts.beside, self.beside[c].is_empty()) {
            (Some(b), false) => b.tick.answered(&self.beside[c]),
            _ => Some(true),
        }
    }

    /// In a tree's tick, with nothing of this walk's in flight: whether a
    /// call not started waits only on the other parts' walks (room in the
    /// window, a call of theirs that will answer).
    fn waits_beside(&self) -> bool {
        !self.stopping()
            && (0..self.calls.len()).any(|c| {
                !self.started[c]
                    && self.waits[c].iter().all(|&j| self.done[j])
                    && self.answered_beside(c).is_some()
            })
    }

    /// Submit call `c`: in flight, or answered at once; a failure to send
    /// it is its failure.
    fn submit(&mut self, c: usize) -> Result<(), Walked> {
        let a = self.calls[c];
        self.started[c] = true;
        self.start_ms[c] = self.now;
        self.seq[c] = self.started.iter().filter(|s| **s).count();
        let part = &mut self.part;
        // No new call once a signal asked to stop (`interrupt`): the
        // calls in flight are awaited, and their answers kept.
        let check = crate::interrupt::check().and_then(|()| part.hooks.before_submit());
        if let Err(e) = check {
            self.started[c] = false;
            self.free_slot();
            self.fail(e);
            return Ok(());
        }
        part.hooks.event(Event::Started(&a.addr));
        // A forget (`lifecycle(r, "retain")`, R-154) calls no provider:
        // state drops the object, and the world keeps it.
        if matches!(a.kind, ActionKind::Forget) {
            part.state.remove(&a.addr);
            self.at_once.push_back(c);
            return Ok(());
        }
        match self.tick.submit(0, c, a, part.state) {
            Ok(true) => self.in_flight += 1,
            Ok(false) => self.at_once.push_back(c),
            Err(e) => {
                self.free_slot();
                let part = &mut self.part;
                part.hooks.persist(part.state).map_err(Walked::Stopped)?;
                part.hooks.action(a, Some(&e), part.state);
                part.hooks.event(Event::Failed(&a.addr, &e));
                self.spans.push((c, self.now, self.now));
                self.fail(e);
            }
        }
        Ok(())
    }

    /// The part's first failure, `e`: no new call starts, of any part.
    fn fail(&mut self, e: anyhow::Error) {
        if let Some(b) = self.opts.beside {
            b.tick.fail();
        }
        self.failed.get_or_insert(e);
    }

    /// The next call answered: one answered at submit, else the next
    /// call's answer, recorded in the part's state; whether a call was
    /// made. `None` when nothing is left in flight.
    fn take(&mut self) -> Option<(usize, Result<()>, bool)> {
        if let Some(c) = self.at_once.pop_front() {
            self.free_slot();
            return Some((c, Ok(()), false));
        }
        if !self.tick.busy() {
            return None;
        }
        self.in_flight -= 1;
        let (calls, hooks) = (&self.calls, self.part.hooks);
        let answer = self.tick.next_answer(&mut |_, c, e| {
            hooks.event(Event::Progress(&calls[c].addr, &e));
        });
        self.free_slot();
        let (c, r) = self.tick.answered(answer, self.part.state);
        Some((c, r, true))
    }

    /// Call `c` answered `r` (`called`: a call was made): the state
    /// persisted and reported, its span on the clock, what waited on it
    /// released, or its failure kept. The walk stops when the state
    /// cannot be written, or chaos `stop-after` stops dform here.
    fn answered(&mut self, c: usize, r: Result<()>, called: bool) -> Result<(), Walked> {
        let a = self.calls[c];
        let part = &mut self.part;
        if r.is_ok()
            && let Some(f) = &mut part.state.in_flight
        {
            f.remaining.remove(&state::key(&a.addr));
        }
        persist_answered(part.hooks, part.state).map_err(Walked::Stopped)?;
        part.hooks.action(a, r.as_ref().err(), part.state);
        let end = self.start_ms[c] + self.tick.latency(0, &a.addr);
        self.spans.push((c, self.start_ms[c], end));
        match r {
            Ok(()) => {
                self.part.hooks.event(Event::Finished(&a.addr));
                self.done[c] = true;
                self.now = self.now.max(end);
                if let Some(b) = self.opts.beside {
                    b.tick.done(b.part, &a.addr);
                }
            }
            Err(e) => {
                self.part.hooks.event(Event::Failed(&a.addr, &e));
                self.fail(e);
            }
        }
        match self.opts.stop_after.filter(|_| called) {
            Some(left) => stop_after(left, a).map_err(Walked::Stopped),
            None => Ok(()),
        }
    }

    /// The walk's end: a call never sent, when nothing failed, is a
    /// cycle's; the timeline recorded, the tick ended and the state
    /// persisted. What the answered calls returned, or why the tick
    /// stopped.
    fn end(mut self) -> Walked {
        if !self.stopping()
            && let Some(c) = self.started.iter().position(|s| !s)
        {
            let a = self.calls[c];
            self.failed = Some(
                crate::report::Failure::of(
                    "apply",
                    &a.addr,
                    "not sent",
                    "its dependencies never finished (a cycle)",
                )
                .into(),
            );
        }
        // The timeline in the order the calls started.
        let seq = &self.seq;
        self.spans.sort_by_key(|&(c, _, _)| seq[c]);
        for &(c, start, end) in &self.spans {
            self.tick.span(0, &self.calls[c].addr, start, end);
        }
        let tick = self.tick.into_parts().remove(0);
        Walked::Ended(self.part.end(tick, self.failed))
    }
}

impl Part<'_> {
    /// Its tick `tick` ends, after its first failure `failed`: the
    /// world's tick ended, state persisted. What its answered calls
    /// returned, or why its tick stopped.
    fn end(
        self,
        tick: crate::plugin::providers::Part,
        failed: Option<anyhow::Error>,
    ) -> Result<Seen> {
        // The end of a tick that failed may fail in turn (its call waits
        // behind one that timed out): why the tick stopped comes first.
        let ended = tick.end(self.state);
        let persisted = self.hooks.persist(self.state);
        match (failed, ended.and_then(|r| persisted.map(|()| r))) {
            (Some(e), Ok(_)) => Err(e),
            // Why the tick stopped first, then why it did not end or its
            // state was not written.
            (Some(e), Err(p)) => Err(p.context(format!("{e:#}"))),
            (None, r) => r,
        }
    }
}

/// What each of `calls`, of `part`, waits on in the tree's other parts
/// (`b`): a secret output a deployment holds at an object is read where it
/// is held, inside the reader's Apply (`provider::Held`), so the reader's
/// call waits for the holder's call that changes that object. What else a
/// reader reads of its upstream is known a tick later.
fn cross(part: &Part, calls: &[&Action], b: Beside) -> Vec<Vec<(usize, Address)>> {
    let changes = calls
        .iter()
        .filter_map(|a| {
            let e = part.state.get(&a.addr)?;
            Some((a.addr.typ.clone(), e.remote.clone(), a.addr.clone()))
        })
        .collect();
    let made = together::Made {
        name: part.name.to_string(),
        objects: changes,
    };
    let others = b.tick.begin(b.part, made);
    calls
        .iter()
        .map(|a| {
            let Some(r) = part.desired.iter().find(|r| r.addr == a.addr) else {
                return Vec::new();
            };
            let mut on = Vec::new();
            for h in part.cloud.held_in(r) {
                for (q, made) in &others {
                    if made.name != h.deployment {
                        continue;
                    }
                    on.extend(
                        made.objects
                            .iter()
                            .filter(|(typ, remote, _)| *typ == h.typ && *remote == h.remote)
                            .map(|(_, _, addr)| (*q, addr.clone())),
                    );
                }
            }
            on
        })
        .collect()
}

/// Chaos `stop-after=N`: one more Apply call (`a`) returned; at none
/// left, dform stops as if killed: nothing in flight is waited for, and
/// the tick never ends.
fn stop_after(left: &AtomicUsize, a: &Action) -> Result<()> {
    let was = left
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        })
        .unwrap_or_default();
    if was > 1 {
        return Ok(());
    }
    Err(crate::report::Failure::of(
        "apply",
        &a.addr,
        "dform stopped after this Apply call returned",
        "chaos stop-after; the next apply resumes",
    )
    .into())
}

/// State after an Apply call returned. A `test-hooks` build (never the
/// `dform` binary's: only tests turn the feature on, as a dev-dependency)
/// can revert it to one write per tick, the end's, for the model test to
/// catch (`tests/model.rs`).
fn persist_answered(part: &dyn Hooks, state: &State) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    if hooks::PERSIST_PER_TICK.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    part.persist(state)
}

/// Mutants the model test must catch; off unless a test sets one.
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use std::sync::atomic::AtomicBool;

    /// Persist once per tick, at its end, instead of after every Apply
    /// call that returns.
    pub static PERSIST_PER_TICK: AtomicBool = AtomicBool::new(false);

    /// Resolve an uncertain Create or Replace as if the provider could not
    /// say what its idempotency key made (the k8s provider today): the
    /// record is dropped, and an object it made is left unmapped.
    pub static CREATED_UNKNOWN: AtomicBool = AtomicBool::new(false);
}

/// What `cloud` says the Create or Replace with idempotency key `key` at
/// `addr` made. A `test-hooks` build can make it say nothing, for the model
/// test to catch (`hooks::CREATED_UNKNOWN`).
fn created(cloud: &Providers, addr: &Address, key: &str) -> Result<Option<String>> {
    #[cfg(feature = "test-hooks")]
    if hooks::CREATED_UNKNOWN.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(None);
    }
    cloud.created(addr, key)
}

/// `approver_allowed(Who, D)`: may `Who` approve the deformation of `D`, an
/// address as plan prints it?
pub type Allowed<'a> = &'a dyn Fn(&str, &str) -> bool;

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
    let is_delete = |a: &Action| {
        matches!(
            a.kind,
            ActionKind::Delete | ActionKind::DeleteDeposed | ActionKind::Forget
        )
    };
    let deps: Vec<Vec<String>> = actions
        .iter()
        .map(|a| match a.kind {
            ActionKind::Delete | ActionKind::Forget => state.get(&a.addr).map(|e| e.deps.clone()),
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
    state.in_flight = Some(InFlight {
        tick,
        remaining,
        destroy: false,
    });
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
        state.uncertain.insert(
            k,
            Uncertain {
                op,
                remote,
                key,
                name: None,
            },
        );
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
///
/// Only the entries `which` keys name are looked at. One whose provider
/// waits on the program's settings (R-177: a kubeconfig the evaluation
/// reads) is left for a call once the evaluation has configured it
/// ([`uncertain_waiting`]); one still waiting then is sent again with its
/// key at the tick that plans it, once its provider is configured.
pub fn resolve_uncertain(
    cloud: &Providers,
    state: &mut State,
    which: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut r = Resolving {
        cloud,
        state,
        said: Vec::new(),
    };
    for (k, u) in r.state.uncertain.clone() {
        if which(&k) {
            r.resolve(&k, &u)?;
        }
    }
    Ok(r.said)
}

/// Uncertain calls being resolved ([`resolve_uncertain`]): the run's
/// providers, the state they are resolved in, and what each resolution
/// says.
struct Resolving<'a> {
    cloud: &'a Providers,
    state: &'a mut State,
    said: Vec<String>,
}

impl Resolving<'_> {
    /// The uncertain call `k`, `u`: resolved, or left for the call that
    /// sends it again; one whose provider waits on the program's settings
    /// is left alone.
    fn resolve(&mut self, k: &str, u: &Uncertain) -> Result<()> {
        let Some(addr) = state::parse_key(k.strip_suffix("#deposed").unwrap_or(k)) else {
            self.state.uncertain.remove(k);
            return Ok(());
        };
        if self.cloud.waits(&addr.typ).is_some() {
            return Ok(());
        }
        match u.op {
            UncertainOp::Create | UncertainOp::Replace { .. } => self.made(k, u, &addr),
            UncertainOp::Update => {
                self.done(k);
                Ok(())
            }
            UncertainOp::Delete | UncertainOp::DeleteDeposed => self.deleted(k, u, &addr),
        }
    }

    /// A create or a replace: what its idempotency key made, if it made
    /// anything, mapped; a destroy-first replace that made nothing, a
    /// create; else left for the call that sends it again, if one will.
    fn made(&mut self, k: &str, u: &Uncertain, addr: &Address) -> Result<()> {
        let at = crate::report::address(addr);
        // A create's address may still map an object that is gone.
        let mapped = self.state.get(addr).map(|e| e.remote.clone());
        let made = created(self.cloud, addr, &u.key).with_context(|| {
            format!(
                "{at}: looking up what the {} whose answer was lost made",
                op_name(&u.op)
            )
        })?;
        if let Some(remote) = made {
            self.adopt(k, u, addr, mapped, remote);
            return Ok(());
        }
        // A destroy-first replacement that deleted the old object and
        // made nothing: what is left is a create.
        if let UncertainOp::Replace {
            create_first: false,
        } = u.op
            && mapped.is_some_and(|m| m == u.remote)
            && !self.cloud.exists(addr, &u.remote)?
        {
            self.create_again(k, u, addr);
            return Ok(());
        }
        if !self.outstanding(k) {
            self.state.uncertain.remove(k);
        }
        Ok(())
    }

    /// The object `remote` a create or a replace made, mapped at `addr`
    /// (which mapped `mapped`): a create-first replacement's old object
    /// deposed.
    fn adopt(
        &mut self,
        k: &str,
        u: &Uncertain,
        addr: &Address,
        mapped: Option<String>,
        remote: String,
    ) {
        let state = &mut *self.state;
        if let UncertainOp::Replace { create_first: true } = u.op
            && mapped.is_some_and(|m| m != remote)
        {
            state.depose(addr);
        }
        let provider = self.cloud.provider_of(&addr.typ).to_string();
        state.set(addr, provider, remote.clone());
        state.set_name(addr, u.name.clone());
        self.done(k);
        self.said.push(format!(
            "{}: the {} whose answer was lost made {remote}; state maps it",
            crate::report::address(addr),
            op_name(&u.op)
        ));
    }

    /// A destroy-first replace that deleted the old object and made
    /// nothing: state forgets the old one, and the call is a create.
    fn create_again(&mut self, k: &str, u: &Uncertain, addr: &Address) {
        self.state.remove(addr);
        if let Some(e) = self.state.uncertain.get_mut(k) {
            e.op = UncertainOp::Create;
            e.remote.clear();
        }
        self.said.push(format!(
            "{}: the replace whose answer was lost deleted {} and made nothing; it is \
             created again",
            crate::report::address(addr),
            u.remote
        ));
    }

    /// A delete: a Read. Gone, state forgets it; there, the plan deletes
    /// it again.
    fn deleted(&mut self, k: &str, u: &Uncertain, addr: &Address) -> Result<()> {
        if !self.cloud.exists(addr, &u.remote)? {
            match u.op {
                UncertainOp::DeleteDeposed => {
                    self.state.deposed.remove(&state::key(addr));
                }
                _ => self.state.remove(addr),
            }
            self.said.push(format!(
                "{}: the delete whose answer was lost took effect; {} is gone",
                crate::report::address(addr),
                u.remote
            ));
        }
        self.done(k);
        Ok(())
    }

    /// The call `k` is resolved: neither uncertain nor remaining.
    fn done(&mut self, k: &str) {
        self.state.uncertain.remove(k);
        if let Some(f) = &mut self.state.in_flight {
            f.remaining.remove(k);
        }
    }

    /// Whether the interrupted tick's record still has the call `k` to
    /// make.
    fn outstanding(&self, k: &str) -> bool {
        self.state
            .in_flight
            .as_ref()
            .is_some_and(|f| f.remaining.contains_key(k))
    }
}

/// The uncertain entries (`State::uncertain`) whose provider waits on the
/// program's settings: [`resolve_uncertain`] leaves them until the
/// evaluation has configured it.
pub fn uncertain_waiting(cloud: &Providers, state: &State) -> BTreeSet<String> {
    state
        .uncertain
        .keys()
        .filter(|k| {
            state::parse_key(k.strip_suffix("#deposed").unwrap_or(k))
                .is_some_and(|a| cloud.waits(&a.typ).is_some())
        })
        .cloned()
        .collect()
}

/// What `plan` carries over from an interrupted apply, for the plan shown
/// before apply asks: each Create or Replace that `resolve_uncertain`
/// found made nothing, to be sent again with its idempotency key (what
/// `resumed` still lists is the plan's tick itself, R-122). Empty when
/// there is none; else a header and a line per address, in plan order.
pub fn carried_over(resumed: Option<&InFlight>, state: &State, plan: &Plan) -> String {
    let mut out = String::new();
    for a in plan
        .actions
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop))
    {
        let k = state::key(&a.addr);
        let retried = matches!(a.kind, ActionKind::Create | ActionKind::Replace { .. })
            && state.uncertain.get(&k).is_some_and(|u| {
                matches!(u.op, UncertainOp::Create | UncertainOp::Replace { .. })
                    && !u.key.is_empty()
            });
        // What remained is the plan's tick, headed `resumed` (R-122); a
        // create sent again under its key says so.
        if !retried {
            continue;
        }
        if out.is_empty() {
            out = match resumed {
                Some(f) => format!("resumed from the apply interrupted at tick {}:\n", f.tick),
                None => "carried over from an interrupted apply:\n".to_string(),
            };
        }
        out.push_str(&format!("  {}", crate::report::address(&a.addr)));
        // One whose provider waits on its settings was not looked up
        // (`resolve_uncertain`): it is sent with its key once configured.
        match a.on.is_empty() {
            true => out.push_str("  (retried with its idempotency key: nothing it made was found)"),
            false => out.push_str(
                "  (sent again with its idempotency key once its provider is configured)",
            ),
        }
        out.push('\n');
    }
    out
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
        // Compared as kept: a resumed record's sensitive leaves are digests.
        let planned = planned.as_ref().map(|d| cloud.stored(&addr.typ, d));
        let now = observed.get(addr).map(|d| cloud.stored(&addr.typ, d));
        let (planned, now) = (planned.as_ref(), now.as_ref());
        let changes = cloud.diff(&addr.typ, planned, now);
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
/// `deformation(pending, r, Before)` with the document each was planned
/// against and `world_digest(r, Now)`, and the evaluator derives the deny
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
            "the world changed under a pending change after tick {tick}:\n{}",
            format_changes(&under)
        );
    }
    let before: BTreeMap<Address, Option<Json>> = pending
        .iter()
        .map(|a| {
            let d = seen.get(a).cloned().flatten();
            (a.clone(), d.map(|d| cloud.stored(&a.typ, &d)))
        })
        .collect();
    Ok(crate::zset::deformation_facts(
        pending.iter().map(|a| ("pending", a)),
        &before,
        &cloud.stored_world(&observed),
    ))
}

/// `~ T["N"]` and a `path: before -> after` line per change, as plan prints an
/// update; a resource that appeared or vanished says so.
pub fn format_changes(changed: &[(Address, Vec<Change>)]) -> String {
    let mut out = String::new();
    for (addr, changes) in changed {
        out.push_str(&format!("~ {}\n", crate::report::address(addr)));
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
