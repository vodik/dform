//! `dform apply` and `dform destroy`: a deployment's plan applied, tick by
//! tick (E §2.7), asked for, approved and logged.

mod approvals;
mod prompt;
mod ticks;

use self::ticks::Ticks;
use super::evaluated::Evaluated;
use super::planning::Reporter;
use super::{Outcome, Session};
use crate::{deployment, engine, report, resources, zset};
use anyhow::Result;
use std::path::PathBuf;

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
            parallel: 1,
            approval: None,
            yes: true,
            allow_empty: Vec::new(),
            wait_timeout: None,
            why: report::Why::None,
            destroy: false,
            new_master: false,
        }
    }

    /// The apply: one apply at a time per deployment, the lock held until
    /// its end is in the audit log (`session`); what an interrupted apply
    /// left resumed; then ticks until the plan holds nothing.
    pub(super) fn run(
        &self,
        run: Evaluated,
        compiled: deployment::Compiled,
        session: &mut Option<Session>,
    ) -> Result<Outcome> {
        let Evaluated { cx, ev, hook } = run;
        let deployment::Evaluation {
            located,
            st,
            moves,
            res,
            violations,
            evaluator,
            ..
        } = ev;
        let r = Reporter::new(&cx, &evaluator, &located, &st);
        let mut ticks = Ticks::new(self, &cx, &r, &evaluator, &located, hook, session, st)?;
        ticks.check_chaos(&compiled.resources)?;
        ticks.begin()?;
        if !moves.is_empty() {
            ticks.say(crate::said::Said::Moved(report::moved_text(&moves)));
        }
        ticks.resume(&violations)?;
        let mut wanted = Wanted {
            res,
            violations,
            resources: compiled.resources,
            adopts: compiled.adopts,
            lifecycle: compiled.lifecycle,
        };
        // The providers the plan's own evaluation configured.
        evaluator.take_configured();
        loop {
            match ticks.tick(wanted)? {
                Next::Tick(w) => wanted = *w,
                Next::Done(outcome) => return Ok(outcome),
            }
        }
    }
}
