//! `dform apply` and `dform destroy`: a deployment's plan applied, tick by
//! tick (E §2.7), asked for, approved and logged.

mod approvals;
mod prompt;
mod ticks;
mod tree;

pub(super) use self::tree::{Seat, apply as tree};

use self::ticks::Ticks;
use super::evaluated::Evaluated;
use super::planning::Reporter;
use super::{Outcome, Session};
use crate::{deployment, engine, report, resources, zset};
use anyhow::Result;
use std::path::PathBuf;

/// `--parallel`'s default: the host's ceiling on a provider's window of
/// requests in flight. Below it the executor submits every call whose
/// dependencies are met, and what paces a provider is that window, kept
/// by what the server answers (`dform_host::ratelimit`).
pub(super) const PARALLEL: u64 = dform_host::ratelimit::CEILING as u64;

/// `dform apply TARGET`, `dform apply PLAN.json` and `dform destroy
/// TARGET`.
#[derive(Debug, Clone)]
pub(super) struct Apply {
    /// `apply PLAN.json`: the plan file applied.
    pub(super) plan_file: Option<PathBuf>,
    /// `dev --chaos`: failures injected into the fake provider.
    pub(super) chaos: Vec<String>,
    pub(super) max_ticks: usize,
    pub(super) parallel: u64,
    pub(super) approval: Option<PathBuf>,
    /// `--yes`: no confirmation.
    pub(super) yes: bool,
    /// `--allow-empty`: what the plan may empty without asking (R-80).
    pub(super) allow_empty: Vec<String>,
    /// `--wait-timeout`: how long a tick waits on a value not reached
    /// yet, over dform.toml's (R-201).
    pub(super) wait_timeout: Option<std::time::Duration>,
    /// What each change of the printed plan says of why.
    pub(super) why: report::Why,
    /// `destroy`: the deployment is removed (R-149).
    pub(super) destroy: bool,
    /// `--new-master` (R-163).
    pub(super) new_master: bool,
}

/// What a tick plans from: the program's evaluation over the world as the
/// last boundary left it, its violations, and what it compiles to.
struct Wanted {
    res: engine::EvalResult,
    violations: Vec<String>,
    resources: Vec<resources::Resource>,
    adopts: Vec<resources::Adopt>,
    lifecycle: zset::Lifecycle,
}

/// Tick 1's plan: the deployment's evaluation's own, the one `plan`
/// prints, and the adoptions and the lifecycle its calls are made with.
struct First {
    planned: deployment::Planned,
    adopts: Vec<resources::Adopt>,
    lifecycle: zset::Lifecycle,
}

impl First {
    /// The plan of the evaluation (`policy`, of the resources `compiled`).
    fn of(
        policy: Option<Result<deployment::Planned>>,
        compiled: deployment::Compiled,
    ) -> Result<First> {
        let planned =
            policy.ok_or_else(|| anyhow::anyhow!("internal: an apply without its plan"))??;
        Ok(First {
            planned,
            adopts: compiled.adopts,
            lifecycle: compiled.lifecycle,
        })
    }
}

/// What a tick is planned from: tick 1's plan, else what the last
/// boundary derived.
enum Step {
    First(Box<First>),
    Wanted(Box<Wanted>),
}

/// What a tick ends in: the next tick, from what the boundary derived, or
/// the apply's end.
enum Next {
    Tick(Box<Wanted>),
    Done(Outcome),
}

impl Apply {
    /// The apply controller mode runs on each event: unattended, it never
    /// asks.
    pub(super) fn unattended(max_ticks: usize) -> Apply {
        Apply {
            plan_file: None,
            chaos: Vec::new(),
            max_ticks,
            parallel: PARALLEL,
            approval: None,
            yes: true,
            allow_empty: Vec::new(),
            wait_timeout: None,
            why: report::Why::None,
            destroy: false,
            new_master: false,
        }
    }

    /// The apply: its plan the evaluation's (`plan`'s); in a tree's run,
    /// handed to the tree ([`tree::Handover`]), else applied: one apply at
    /// a time per deployment, the lock held until its end is in the audit
    /// log (`session`); what an interrupted apply left resumed; then ticks
    /// until the plan holds nothing.
    pub(super) fn run(
        &self,
        run: Evaluated,
        compiled: deployment::Compiled,
        session: &mut Option<Session>,
    ) -> Result<Outcome> {
        let Evaluated { mut cx, ev, hook } = run;
        let deployment::Evaluation {
            located,
            st,
            moves,
            violations,
            evaluator,
            policy,
            ..
        } = ev;
        let first = First::of(policy, compiled)?;
        let start = Start {
            moves,
            violations,
            first,
        };
        if let Some(seat) = cx.cli.seat.clone() {
            self.planned(&mut cx, &located, &evaluator, &st, &start)?;
            return tree::serve(self, &seat, &cx, &located, &evaluator, st, start, session);
        }
        let r = Reporter::new(&cx, &evaluator, &located, &st);
        let mut ticks = Ticks::new(self, &cx, &r, &evaluator, &located, hook, session, st)?;
        let mut step = ticks.start(start)?;
        loop {
            match ticks.tick(step)? {
                Next::Tick(w) => step = Step::Wanted(w),
                Next::Done(outcome) => return Ok(outcome),
            }
        }
    }
}

impl Apply {
    /// A deployment of a tree's apply, planned (`start`): what those it
    /// reads know only after a tick, as their plans have it, is what its
    /// report says (`board`); its outputs as its plan has them are told
    /// to those that read it.
    fn planned(
        &self,
        cx: &mut crate::cli::evaluated::Context,
        located: &deployment::Located,
        evaluator: &deployment::Evaluator,
        st: &crate::state::State,
        start: &Start,
    ) -> Result<()> {
        let Some(up) = cx.cli.upstreams.clone() else {
            return Ok(());
        };
        cx.cli.planned = up.planned()?;
        let p = &start.first.planned;
        let r = Reporter::new(cx, evaluator, located, st);
        let report = r.report(&p.plan, &p.res, &p.sections, 1, &[], &p.denies);
        let outputs = r.outputs_of(&p.res, st, &report.outputs_after)?;
        cx.cli.held.outputs(outputs.clone());
        up.tell_planned(outputs);
        Ok(())
    }
}

/// What a deployment's ticks start from ([`Ticks::start`]): the moves
/// `moved/3` made, its violations, and tick 1's plan.
struct Start {
    moves: Vec<(crate::address::Address, crate::address::Address)>,
    violations: Vec<String>,
    first: First,
}
