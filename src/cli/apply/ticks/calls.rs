//! A tick's Apply calls, of one deployment: prepared (its in-flight
//! record written, what needs the master or waits taken out), made as one
//! part of the executor's walk (`executor::run_tick`: a deployment's own
//! apply walks one part, a tree's one per deployment, together), each told
//! to the deployment's audit log, its fence and its write-ahead record as
//! it goes, and what they did taken back: the tick's checkpoint, its
//! retries and its world in the audit log.

use super::{Ran, TickPlan, Ticks, forgotten, log_retries, needing_master};
use crate::cli::evaluated::Context;
use crate::deployment::Planned;
use crate::progress::Progress;
use crate::provider::ActionKind;
use crate::report::progress::{Block, Change};
use crate::report::waits_on;
use crate::said::Said;
use crate::{address, executor, query, report, state, store};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// A tick's calls, prepared ([`Ticks::calls`]): the world as the tick's
/// plan saw it, what of it waits or needs the master, and whether it
/// changes anything.
pub(in crate::cli::apply) struct Calls {
    seen: executor::Seen,
    pending: BTreeSet<address::Address>,
    changed: bool,
    needing: BTreeMap<address::Address, Vec<String>>,
}

impl Calls {
    /// Whether tick `tick` makes calls: every apply is at least one tick
    /// of the fake world, also when there is nothing to do; a later tick,
    /// when it changes anything.
    pub(in crate::cli::apply) fn made(&self, tick: usize) -> bool {
        tick == 1 || self.changed
    }

    /// What a tick that made no call did.
    pub(in crate::cli::apply) fn ran(self) -> Ran {
        Ran {
            seen: self.seen,
            pending: self.pending,
            changed: self.changed,
            needing: self.needing,
        }
    }
}

/// The changes of `plan` the tick holds whose plan read the world: what
/// the boundary's guard compares with the world then. One held on its
/// provider's settings was planned without a read (R-177): it is planned
/// again at the boundary, against the world as it is then, and nothing
/// changed under it.
fn guarded(
    plan: &crate::provider::Plan,
    sections: &crate::stuck::Sections,
) -> BTreeSet<address::Address> {
    plan.actions
        .iter()
        .filter(|a| waits_on(a, sections).is_some() && sections.read_world(&a.addr))
        .map(|a| a.addr.clone())
        .collect()
}

/// One deployment's part of a tick's calls while they are made
/// ([`executor::Hooks`]): each action to its audit log, each answer to
/// its write-ahead record, its lease's fence before each call, and each
/// change of state to the tick's block `progress`, as part `part` of it.
pub(in crate::cli::apply) struct Calling<'a, 'p> {
    log: ActionLog<'a>,
    dep: &'a store::Deployment,
    /// Chaos `interrupt`: the stop a Ctrl-C asks for, once the call is
    /// sent.
    interrupt: &'a BTreeSet<address::Address>,
    progress: Option<&'p Progress>,
    part: usize,
}

impl<'a> Calling<'a, '_> {
    /// The calls made: their audit log, the block let go.
    pub(in crate::cli::apply) fn into_log(self) -> ActionLog<'a> {
        self.log
    }
}

/// Each Apply call of a tick to the audit log: its result, its remote id,
/// and a digest of its redacted diff; a forget (R-154) by the remote id it
/// drops. The first entry that could not be written fails the tick.
pub(in crate::cli::apply) struct ActionLog<'a> {
    tick: usize,
    audit: &'a crate::audit::Log,
    redact: query::Redactor,
    diffs: BTreeMap<address::Address, String>,
    /// What a forget drops from state: its remote id, which the log keeps.
    forgotten: BTreeMap<address::Address, String>,
    failed: std::cell::RefCell<Option<anyhow::Error>>,
}

impl executor::Hooks for Calling<'_, '_> {
    fn persist(&self, st: &state::State) -> Result<()> {
        self.dep.record(st)
    }

    fn action(&self, a: &crate::provider::Action, err: Option<&anyhow::Error>, st: &state::State) {
        self.log.action(a, err, st);
    }

    fn before_submit(&self) -> Result<()> {
        self.dep.check_fence()
    }

    fn event(&self, e: executor::Event) {
        if let executor::Event::Started(a) = e
            && self.interrupt.contains(a)
        {
            crate::interrupt::request(libc::SIGINT);
        }
        if let Some(p) = self.progress {
            p.event(self.part, &e, &|t: &str| self.log.redact.text(t));
        }
    }
}

impl<'a> Ticks<'a, '_> {
    /// The tick's calls prepared: what needs the master this run does not
    /// hold (R-164) and what depends on it left for an apply with it,
    /// what waits held; the in-flight record written with the world as
    /// the plan saw it, each create's idempotency key in it.
    pub(in crate::cli::apply) fn calls(&mut self, p: &mut TickPlan) -> Result<Calls> {
        let t = &mut p.tick;
        let needing = needing_master(
            &t.planned.plan,
            &t.planned.resources,
            &self.st,
            self.backend(),
        );
        let p = &mut t.planned;
        p.plan.actions.retain(|a| !needing.contains_key(&a.addr));
        let sections = &p.sections;
        let waits = |a: &crate::provider::Action| waits_on(a, sections).is_some();
        self.ran.extend(
            p.plan
                .actions
                .iter()
                .filter(|a| !matches!(a.kind, ActionKind::Noop) && !waits(a))
                .map(|a| (a.addr.typ.clone(), a.addr.name.clone())),
        );
        // Kept in state: a sensitive leaf by its digest.
        let backend = self.backend();
        let observed = backend.stored_world(&backend.observe(&self.st)?);
        executor::begin(&mut self.st, self.tick, &p.plan, &observed);
        if let Some(f) = self.st.in_flight.as_mut() {
            f.destroy = self.args.destroy;
        }
        let deployment = self.deployment();
        executor::mark_creates(
            &mut self.st,
            deployment,
            p.plan.actions.iter().filter(|a| !waits(a)),
        );
        self.persist()?;
        let pending = guarded(&p.plan, sections);
        p.plan.actions.retain(|a| !waits(a));
        let changed = p
            .plan
            .actions
            .iter()
            .any(|a| !matches!(a.kind, ActionKind::Noop));
        Ok(Calls {
            seen: observed.into_iter().map(|(a, d)| (a, Some(d))).collect(),
            pending,
            changed,
            needing,
        })
    }

    /// The tick's block on stderr (R-127, R-206): its actions as the
    /// plan's tree nests them, each failure's site where its change is
    /// derived (R-109).
    pub(in crate::cli::apply) fn block(&self, p: &Planned) -> Block {
        let actions: Vec<&crate::provider::Action> = p.plan.actions.iter().collect();
        let report = self
            .r
            .report(&p.plan, &p.res, &p.sections, self.tick, &[], &p.denies);
        let sites = report::sites(
            &p.res,
            actions.iter().map(|a| &a.addr),
            self.r.top.as_deref(),
        );
        Block::new(self.tick, &actions, std::time::Instant::now())
            .nested(&report.outline())
            .with_sites(sites)
    }

    /// The tick's block printed as it goes (under `--yes`, a line per
    /// change of state).
    pub(in crate::cli::apply) fn progress(&self, block: Block) -> Progress {
        let (mode, style) = self.drawn();
        Progress::tick(block, mode, style, self.asked.take())
    }

    /// How the tick's block is printed: drawn in place where the apply
    /// asks on a terminal, else a line per change of state; painted on a
    /// terminal only.
    pub(in crate::cli::apply) fn drawn(&self) -> (crate::progress::Mode, report::Style) {
        let mode =
            crate::progress::Mode::of_block(self.r.why() == report::Why::None, self.args.yes);
        let style = match mode {
            crate::progress::Mode::Terminal => self.cx.cli.style,
            _ => report::Style::default(),
        };
        (mode, style)
    }

    /// What makes the tick's calls of this deployment, as part `part` of
    /// the tick's block `progress`.
    pub(in crate::cli::apply) fn calling<'p>(
        &self,
        p: &Planned,
        progress: Option<&'p Progress>,
        part: usize,
    ) -> Calling<'a, 'p> {
        let cx: &'a Context = self.cx;
        let backend = self.backend();
        Calling {
            log: ActionLog {
                tick: self.tick,
                audit: &cx.audit,
                redact: query::Redactor::new(&p.res.facts, backend.schema(), backend.secrets()),
                diffs: self.diffs(p),
                forgotten: forgotten(&p.plan, &self.st),
                failed: std::cell::RefCell::new(None),
            },
            dep: &cx.dep,
            interrupt: &cx.chaos.interrupt,
            progress,
            part,
        }
    }

    /// This deployment's part of the tick's walk: its plan `p`'s calls
    /// made by its providers, `calling` told, its state written.
    pub(in crate::cli::apply) fn part<'p>(
        &'p mut self,
        p: &'p TickPlan,
        calling: &'p Calling<'_, '_>,
    ) -> executor::Part<'p> {
        p.part(self.deployment(), self.backend(), &mut self.st, calling)
    }

    /// The tick's calls of this deployment, its own walk of one part: its
    /// block filling in on stderr.
    pub(in crate::cli::apply) fn apply(&mut self, p: &mut TickPlan) -> Result<Ran> {
        let calls = self.calls(p)?;
        if !calls.made(self.tick) {
            return Ok(calls.ran());
        }
        // None for a controller, whose log line is its report.
        let progress = self
            .hook
            .is_none()
            .then(|| self.progress(self.block(&p.tick.planned)));
        let calling = self.calling(&p.tick.planned, progress.as_ref(), 0);
        let opts = executor::Options {
            parallel: self.args.parallel as usize,
            stop_after: self.stop_after.as_ref(),
        };
        let part = p.part(self.deployment(), self.backend(), &mut self.st, &calling);
        let walked = executor::run_tick(vec![part], &opts);
        let log = calling.into_log();
        let failed = progress.map(Progress::finish).unwrap_or_default();
        let applied = match walked {
            executor::Walked::Ended(mut each) => each.remove(0),
            executor::Walked::Stopped { error, .. } => Err(error),
        };
        let killed = self.stop_after.as_ref().is_some_and(|left| left.get() == 0);
        self.called(calls, log, applied, &failed_of(&failed, 0), killed)
    }

    /// After the tick's calls, told to `log` (`applied`: what the
    /// executor saw of the world, or why the tick stopped): a failure the
    /// block said (`failed`, its changes of this deployment) not said
    /// again, the providers' notes said, the tick's checkpoint written
    /// (but where chaos `stop-after` stopped dform as if `killed`), its
    /// retries and its world in the audit log.
    pub(in crate::cli::apply) fn called(
        &mut self,
        calls: Calls,
        log: ActionLog,
        applied: Result<executor::Seen>,
        failed: &[address::Address],
        killed: bool,
    ) -> Result<Ran> {
        let applied = self.said_below(applied, failed);
        let backend = self.backend();
        for note in backend.take_notes() {
            self.say(Said::Chaos(note));
        }
        if let Some(e) = log.failed.into_inner() {
            return Err(e);
        }
        let checkpoint = match killed {
            true => Ok(()),
            false => self.persist(),
        };
        log_retries(log.audit, &log.redact, backend, self.tick)?;
        let applied = applied?;
        checkpoint?;
        // The world as the plan saw it, overlaid with what the calls
        // returned.
        let seen = calls.seen.into_iter().chain(applied).collect();
        self.log_world(&seen)?;
        Ok(Ran {
            seen,
            pending: calls.pending,
            changed: calls.changed,
            needing: calls.needing,
        })
    }

    /// The world as the tick's calls left it, in the audit log, keyed like
    /// a secret (a document may hold one); without the master, no digest
    /// of it (R-164).
    fn log_world(&self, seen: &executor::Seen) -> Result<()> {
        let world: serde_json::Map<String, serde_json::Value> = seen
            .iter()
            .map(|(a, d)| (state::key(a), d.clone().unwrap_or_default()))
            .collect();
        let canonical = crate::approval::canonical_json(&world.into());
        self.cx.audit.append(
            "tick",
            serde_json::json!({
                "tick": self.tick,
                "world": self.key().map(|k| format!("hmac-sha256:{}", k.digest(canonical.as_bytes()))),
            }),
        )?;
        Ok(())
    }

    /// The tick's calls' outcome, `applied`: a failure the block said in
    /// full below it (the calls that `failed`) is not said again, the run
    /// ends naming what failed (R-109).
    fn said_below(
        &self,
        applied: Result<executor::Seen>,
        failed: &[address::Address],
    ) -> Result<executor::Seen> {
        match applied {
            Err(e) if !failed.is_empty() && e.is::<report::Failure>() => Err(anyhow::anyhow!(
                "apply {}: tick {} failed: {}",
                self.cx.deployment,
                self.tick,
                failed
                    .iter()
                    .map(report::address)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            applied => applied,
        }
    }

    /// A digest of each action's redacted diff, as the plan file records
    /// it.
    fn diffs(&self, p: &Planned) -> BTreeMap<address::Address, String> {
        self.r
            .delta(&p.plan, &p.res, &p.sections, self.tick, self.key())
            .into_iter()
            .map(|e| {
                let digest =
                    crate::approval::digest_of(&serde_json::to_value(&e).unwrap_or_default());
                (
                    address::Address {
                        typ: e.typ,
                        name: e.name,
                    },
                    digest,
                )
            })
            .collect()
    }
}

impl ActionLog<'_> {
    fn action(&self, a: &crate::provider::Action, err: Option<&anyhow::Error>, st: &state::State) {
        let tick = self.tick;
        if let Some(remote) = self.forgotten.get(&a.addr) {
            let e = serde_json::json!({
                "tick": tick,
                "address": a.addr.to_string(),
                "remote": remote,
                "why": "lifecycle retain",
            });
            self.append("forgot", e);
            return;
        }
        let mut e = serde_json::json!({
            "tick": tick,
            "action": crate::zset::deformation_kind(&a.kind, false).unwrap_or("no-op"),
            "address": a.addr.to_string(),
            "result": if err.is_some() { "failed" } else { "ok" },
            "remote": st.get(&a.addr).map(|e| e.remote.clone()),
            "diff": self.diffs.get(&a.addr),
        });
        if let Some(err) = err {
            crate::audit::error(&mut e, &self.redact.text(&crate::diag::shape(err)));
        }
        self.append("action", e);
    }

    fn append(&self, kind: &str, e: serde_json::Value) {
        if let Err(x) = self.audit.append(kind, e) {
            self.failed.borrow_mut().get_or_insert(x);
        }
    }
}

impl TickPlan {
    /// The plan's part of the tick's walk, of the deployment `name`: its
    /// calls made by `cloud`, their answers written to `state`, `hooks`
    /// told.
    fn part<'p>(
        &'p self,
        name: &'p str,
        cloud: &'p crate::plugin::Providers,
        state: &'p mut state::State,
        hooks: &'p dyn executor::Hooks,
    ) -> executor::Part<'p> {
        executor::Part {
            name,
            cloud,
            desired: &self.tick.planned.resources,
            adopts: &self.adopts,
            lifecycle: &self.lifecycle,
            state,
            plan: &self.tick.planned.plan,
            hooks,
        }
    }
}

/// The changes of part `part` among a block's `failed`, by address.
pub(in crate::cli::apply) fn failed_of(failed: &[Change], part: usize) -> Vec<address::Address> {
    failed
        .iter()
        .filter(|c| c.part == part)
        .map(|c| c.addr.clone())
        .collect()
}
