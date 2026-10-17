//! A deployment of a tree's apply, on the thread that planned it: its
//! evaluation, its providers and its ticks never leave it. Once planned it
//! takes its seat at the tree's apply ([`serve`]) and does each step of
//! the tree's ticks the tree's thread asks of it ([`Job`]): its tick
//! planned, shown, approved, its calls prepared and walked (its part of
//! the tick, beside the other deployments' on theirs), taken back, its
//! boundary, and what it makes known to those that read it.

use super::super::ticks::{Calls, Ran, TickPlan, Ticks, failed_of};
use super::super::{Apply, Next, Start, Step};
use super::hub::{Seat, Seated};
use crate::cli::evaluated::Context;
use crate::cli::planning::Reporter;
use crate::cli::{Outcome, Session};
use crate::progress::Progress;
use crate::report::progress::{Block, Change};
use crate::{deployment, executor, stack, state};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

/// A step the tree's thread asks of a deployment's ([`Side`]).
pub(super) type Job = Box<dyn FnOnce(&mut Side<'_>) + Send>;

/// A deployment of the tree's apply as its thread holds it: its ticks,
/// and where its coming tick is.
pub(super) struct Side<'a> {
    ticks: Ticks<'a, 'static>,
    r: &'a Reporter<'a>,
    located: &'a deployment::Located,
    evaluator: &'a deployment::Evaluator,
    /// What its ticks start from, until they do.
    start: Option<Start>,
    /// What its coming tick is planned from; `None` once it is done.
    next: Option<Step>,
    /// The tick planned.
    plan: Option<TickPlan>,
    /// Its calls prepared.
    calls: Option<Calls>,
    /// Its walk of them: what it told its audit log, and what it did.
    walked: Option<(super::super::ticks::ActionLog<'a>, Result<executor::Seen>)>,
    /// What its calls did.
    ran: Option<Ran>,
}

/// The deployment `cx` of a tree's apply, planned (`start`), at its seat
/// (`seat`): it does what the tree's thread asks of it until the tree's
/// apply lets it go. Its lease (`session`) is its run's.
#[allow(clippy::too_many_arguments)]
pub(in crate::cli) fn serve(
    args: &Apply,
    seat: &Seat,
    cx: &Context,
    located: &deployment::Located,
    evaluator: &deployment::Evaluator,
    st: state::State,
    start: Start,
    session: &mut Option<Session>,
) -> Result<Outcome> {
    let r = Reporter::new(cx, evaluator, located, &st);
    let mut ticks = Ticks::new(args, cx, &r, evaluator, located, None, session, st)?;
    ticks.in_tree();
    let (jobs, asked) = mpsc::channel::<Job>();
    let seated = Seated {
        jobs,
        stored: cx.stored.clone(),
        style: cx.cli.style,
        stop_after: cx.chaos.stop_after,
    };
    if !seat.take(seated) {
        return Ok(Outcome::Done);
    }
    let mut side = Side {
        ticks,
        r: &r,
        located,
        evaluator,
        start: Some(start),
        next: None,
        plan: None,
        calls: None,
        walked: None,
        ran: None,
    };
    for job in asked {
        job(&mut side);
    }
    Ok(Outcome::Done)
}

/// What a deployment's tick came to after its boundary ([`Side::after`]).
pub(super) enum After {
    /// Another tick follows.
    Tick,
    /// Its apply is done, so.
    Done(Outcome),
}

/// What says whether a tick of the tree can move ([`Side::waiting`]).
pub(super) struct Waiting {
    /// The tick ends at a boundary.
    pub(super) boundary: bool,
    /// It holds what waiting can resolve.
    pub(super) can_wait: bool,
    /// What it waits on.
    pub(super) on: Vec<String>,
}

/// No tick of a member is planned where one is used.
fn unplanned() -> anyhow::Error {
    anyhow::anyhow!("internal: no tick planned")
}

impl Side<'_> {
    /// Its ticks begun ([`Ticks::start`]).
    pub(super) fn start(&mut self) -> Result<()> {
        let Some(start) = self.start.take() else {
            bail!("internal: a member's ticks begun twice");
        };
        self.next = Some(self.ticks.start(start)?);
        Ok(())
    }

    /// Its coming tick planned: the outputs its plan knows only after a
    /// tick, by key (what its readers wait on).
    pub(super) fn plan_tick(&mut self) -> Result<BTreeMap<String, usize>> {
        let Some(step) = self.next.take() else {
            bail!("internal: a member done is planned");
        };
        let p = self.ticks.plan(step)?;
        let after = self.ticks.outputs_after(&p.tick);
        self.plan = Some(p);
        Ok(after)
    }

    /// What its report says of the outputs of `full`, which it reads:
    /// those known only after a tick (`after`).
    pub(super) fn outside_of(&self, full: &str, after: &BTreeMap<String, usize>) {
        self.r.outside_of(full, after);
    }

    /// Its tick shown, and the changes it makes; `Some`: its apply ends
    /// there.
    pub(super) fn show(&mut self) -> Result<(Option<Outcome>, usize)> {
        let p = self.plan.as_ref().ok_or_else(unplanned)?;
        let shown = self.ticks.show(&p.tick)?;
        let changes = self.ticks.changes(&p.tick);
        if let Some(o) = &shown {
            self.ticks.end(&Ok(o.clone()));
        }
        Ok((shown, changes))
    }

    /// Its tick approved, and at tick 1 its apply's start logged.
    pub(super) fn approve(&mut self, first: bool) -> Result<()> {
        let p = self.plan.as_ref().ok_or_else(unplanned)?;
        self.ticks.approve(&p.tick)?;
        if first {
            self.ticks.log_start()?;
        }
        Ok(())
    }

    /// Its tick's calls prepared ([`Ticks::calls`]): whether it makes
    /// any.
    pub(super) fn prepare(&mut self, tick: usize) -> Result<bool> {
        let p = self.plan.as_mut().ok_or_else(unplanned)?;
        let calls = self.ticks.calls(p)?;
        let made = calls.made(tick);
        self.calls = Some(calls);
        Ok(made)
    }

    /// Its part of the tick's block.
    pub(super) fn block(&self) -> Result<Block> {
        let p = self.plan.as_ref().ok_or_else(unplanned)?;
        Ok(self.ticks.block(&p.tick.planned))
    }

    /// The tick's block printed as it goes, as its ticks print one.
    pub(super) fn progress(&self, block: Block) -> Progress {
        self.ticks.progress(block)
    }

    /// Its calls walked, as part `opts.beside` of the tick, told to
    /// `progress`.
    pub(super) fn walk(&mut self, progress: &Progress, opts: &executor::Options) -> Result<()> {
        let Some(b) = opts.beside else {
            bail!("internal: a tree's part walked alone");
        };
        let p = self.plan.as_ref().ok_or_else(unplanned)?;
        let calling = self.ticks.calling(&p.tick.planned, Some(progress), b.part);
        let applied = executor::run_tick(self.ticks.part(p, &calling), opts).applied();
        self.walked = Some((calling.into_log(), applied));
        Ok(())
    }

    /// What its walk did taken back ([`Ticks::called`]): `failed`, the
    /// tick's block's failures, of part `part`; whether its tick changed
    /// anything. A deployment that made no call takes back what it
    /// prepared.
    pub(super) fn called(
        &mut self,
        failed: &[Change],
        part: Option<usize>,
        killed: bool,
    ) -> Result<bool> {
        let calls = self
            .calls
            .take()
            .ok_or_else(|| anyhow::anyhow!("internal: no calls prepared"))?;
        let ran = match (part, self.walked.take()) {
            (Some(part), Some((log, applied))) => {
                self.ticks
                    .called(calls, log, applied, &failed_of(failed, part), killed)?
            }
            _ => calls.ran(),
        };
        let changed = ran.changed;
        self.ran = Some(ran);
        Ok(changed)
    }

    /// Whether its tick ends at a boundary, holds what waiting can
    /// resolve, and what it waits on.
    pub(super) fn waiting(&self) -> Result<Waiting> {
        let t = &self.plan.as_ref().ok_or_else(unplanned)?.tick;
        Ok(Waiting {
            boundary: t.boundary,
            can_wait: self.ticks.can_wait(t),
            on: self.ticks.waited_on(t),
        })
    }

    /// After its tick's calls (`waits`: nothing of the tree's changed in
    /// it, so it waits first for what it waits on): its next tick, or its
    /// apply's end, logged and its lease released.
    pub(super) fn after(&mut self, waits: bool) -> Result<After> {
        let (Some(p), Some(ran)) = (self.plan.take(), self.ran.take()) else {
            bail!("internal: a tick's boundary before its calls");
        };
        let waits = waits && self.ticks.can_wait(&p.tick);
        Ok(match self.ticks.after(p, ran, waits)? {
            Next::Tick(w) => {
                self.next = Some(Step::Wanted(w));
                After::Tick
            }
            Next::Done(o) => {
                self.ticks.end(&Ok(o.clone()));
                After::Done(o)
            }
        })
    }

    /// Its outputs as its last boundary's evaluation has them, else (done)
    /// as its state does: what those that read it read.
    pub(super) fn outputs(&self) -> Result<stack::Planned> {
        let res = match &self.next {
            Some(Step::Wanted(w)) => Some(&w.res),
            _ => None,
        };
        self.ticks.outputs_now(res)
    }

    /// What `name`, which it reads, made known (`outputs`): read again at
    /// its next evaluation.
    pub(super) fn read(&self, name: &str, outputs: &stack::Planned) {
        let read = outputs.over(name, None);
        let facts = stack::output_facts(std::slice::from_ref(&read), &self.located.loaded.deployed);
        self.evaluator.read_again(name, facts);
        self.evaluator
            .backend
            .hold(stack::held(std::slice::from_ref(&read)));
    }

    /// Its apply ended as `r` says, if it has not.
    pub(super) fn end(&mut self, r: &Result<Outcome>) {
        self.ticks.end(r);
    }
}

/// The walks' shared counters of a tick: its window and the tree's chaos
/// `stop-after`, as a member's part's walk is given them.
pub(super) struct Walking {
    pub(super) together: Arc<executor::Together>,
    pub(super) stop_after: Option<Arc<AtomicUsize>>,
    pub(super) parallel: usize,
    pub(super) part: usize,
}

impl Walking {
    /// The executor's options of this part's walk.
    pub(super) fn options(&self) -> executor::Options<'_> {
        executor::Options {
            parallel: self.parallel,
            stop_after: self.stop_after.as_deref(),
            beside: Some(executor::Beside {
                tick: &self.together,
                part: self.part,
            }),
        }
    }
}
