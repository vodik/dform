//! `dform apply` and `dform destroy`: a deployment's plan applied, tick by
//! tick (E §2.7), asked for, approved and logged.

use super::evaluated::{Context, Evaluated};
use super::outputs::{readers_of, sealed_label};
use super::planning::{Reporter, unreachable_text};
use super::secrets::recipients_json;
use super::{Outcome, Refused, Session, open_s3};
use crate::ast::{Atom, Term};
use crate::deployment::{self, Planned};
use crate::provider::ActionKind;
use crate::report::waits_on;
use crate::value::Value;
use crate::{controller, engine, executor, ir, query, report, state, store, stuck, zset};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    /// What each change of the printed plan says of why.
    pub(super) why: report::Why,
    /// `destroy`: the deployment is removed (R-149).
    pub(super) destroy: bool,
    /// `--new-master` (R-163).
    pub(super) new_master: bool,
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
        for addr in cx.chaos.addresses() {
            if !compiled.resources.iter().any(|r| &r.addr == addr) && ticks.st.get(addr).is_none() {
                bail!(
                    "--chaos: {} is not a resource of this stack",
                    report::address(addr)
                );
            }
        }
        ticks.begin()?;
        if !moves.is_empty() {
            print!("{}", report::moved_text(&moves));
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

/// What a tick plans from: the program's evaluation over the world as the
/// last boundary left it, its violations, and what it compiles to.
struct Wanted {
    res: engine::EvalResult,
    violations: Vec<String>,
    resources: Vec<ir::Resource>,
    adopts: Vec<ir::Adopt>,
    lifecycle: zset::Lifecycle,
}

/// What a tick ends in: the next tick, from what the boundary derived, or
/// the apply's end.
enum Next {
    Tick(Box<Wanted>),
    Done(Outcome),
}

/// An apply's ticks (E §2.7): each applies every definite deformation in
/// dependency order; what waits on a null is held. At the boundary the
/// results come back as world facts, round 0 resolves them, everything is
/// re-derived and policy is checked again before the next tick.
struct Ticks<'a, 'h> {
    args: &'a Apply,
    cx: &'a Context,
    r: &'a Reporter<'a>,
    evaluator: &'a deployment::Evaluator,
    located: &'a deployment::Located,
    hook: Option<&'h mut controller::Hook>,
    session: &'a mut Option<Session>,
    st: state::State,
    /// The tick being applied, from 1.
    tick: usize,
    /// What the plan file records of this run's inputs.
    inputs: zset::file::Inputs,
    approvals: Approvals<'a>,
    /// The in-flight record of an apply this one resumes (R-122).
    resumed: Option<state::InFlight>,
    /// Chaos `stop-after`: the executor's, counted across the run's ticks.
    stop_after: Option<std::cell::Cell<usize>>,
    /// How long a tick waits on an open null (R-122), by provider.
    waits: BTreeMap<String, std::time::Duration>,
    /// A plan file or an approval applies only the ticks whose addresses
    /// the plan it approves named (R-30); `--yes` answers every question
    /// (R-122).
    shown: bool,
    /// Every address a tick's plan has listed so far, and the last one's
    /// pending groups: what it could not name.
    listed: BTreeSet<ir::Address>,
    unnamed: Vec<String>,
    /// What the last tick's plan held under `later` waiting on a
    /// provider's settings (R-110): listed, but planned only once the
    /// provider is configured, so asked for again (R-45).
    on_provider: BTreeSet<ir::Address>,
    /// What the first tick's plan scheduled in a later tick, its
    /// attributes as written (R-156): shown, so not asked again.
    scheduled: BTreeSet<String>,
    /// The delta of the plan this apply showed, or of the plan file it
    /// applies: a later tick's re-plan is compared with it, and what
    /// earlier ticks ran (After R-156).
    shown_delta: Vec<zset::file::Entry>,
    ran: BTreeSet<(String, String)>,
}

/// One tick's plan, and what the tick made of it.
struct Tick {
    planned: Planned,
    /// What the policy pass says needs an approval.
    needs: Vec<(String, String)>,
    /// The digest of this plan: the file's, else of the plan as a file
    /// would record it.
    digest: Option<String>,
    /// What the tick holds: the nulls its held changes wait on.
    held: Vec<String>,
    /// The tick ends at a boundary: another tick follows.
    boundary: bool,
    /// Controller mode: the plan changes nothing.
    undeformed: bool,
}

impl<'a, 'h> Ticks<'a, 'h> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        args: &'a Apply,
        cx: &'a Context,
        r: &'a Reporter<'a>,
        evaluator: &'a deployment::Evaluator,
        located: &'a deployment::Located,
        hook: Option<&'h mut controller::Hook>,
        session: &'a mut Option<Session>,
        st: state::State,
    ) -> Result<Ticks<'a, 'h>> {
        // How long a tick waits on an open null (R-122): the `wait` of the
        // provider that answers it, as dform.toml sets it (a built-in
        // extern's by its `[providers.NAME]` table), else 10m; not its
        // calls' `timeout`.
        let waits = located
            .loaded
            .manifest
            .as_ref()
            .map(|m| m.provider_waits())
            .unwrap_or_default();
        Ok(Ticks {
            args,
            cx,
            r,
            evaluator,
            located,
            hook,
            session,
            st,
            tick: 1,
            inputs: cx
                .inputs
                .clone()
                .ok_or_else(|| anyhow::anyhow!("internal: no plan inputs"))?,
            approvals: Approvals {
                cx,
                located,
                roots: std::cell::OnceCell::new(),
                restricts: crate::approval::restricts_approvers(&evaluator.program),
                approved: None,
            },
            resumed: None,
            stop_after: cx.chaos.stop_after.map(std::cell::Cell::new),
            waits,
            shown: cx.saved.is_some() || args.approval.is_some(),
            listed: BTreeSet::new(),
            unnamed: Vec::new(),
            on_provider: BTreeSet::new(),
            scheduled: BTreeSet::new(),
            shown_delta: Vec::new(),
            ran: BTreeSet::new(),
        })
    }

    fn backend(&self) -> &'a crate::plugin::Providers {
        &self.evaluator.backend
    }

    fn deployment(&self) -> &'a str {
        &self.cx.deployment
    }

    /// Without the master (R-164) the apply's digests are its plan file's
    /// form: derivation digests, labels.
    fn key(&self) -> Option<&'a zset::file::Key> {
        self.cx.file_key()
    }

    /// A checkpoint of the state (`wal`): before a tick's first call and
    /// after its last, and where the apply stops. Between, each call's
    /// change of state goes to the log alone, before the next call
    /// (`Store::record`).
    fn persist(&self) -> Result<()> {
        self.cx.dep.save_state(&self.st)
    }

    /// The apply's lock, checked still held before a question: a person
    /// never answers one while the lease is lost or no longer renewed.
    fn still_held(&self) -> Result<()> {
        match &*self.session {
            Some(s) => s.lock.check(),
            None => Ok(()),
        }
    }

    /// The apply's start: its lock taken, the controller's memos flushed
    /// under the lease again, the memos the plan read kept in state.
    /// Nothing is written, to state or the world, until the apply is
    /// confirmed: the moves, the resolution of uncertain calls and the
    /// in-flight record taken here are written with the tick's first.
    fn begin(&mut self) -> Result<()> {
        // One apply at a time per deployment; the lock is held until the
        // apply's end is in the audit log.
        *self.session = Some(Session {
            log: self.cx.audit.clone(),
            lock: self.cx.dep.lock()?,
        });
        // Under the lease again: a memo the last run kept in memory.
        if let Some(h) = self.hook.as_deref_mut() {
            h.flush()?;
        }
        self.keep_memos()?;
        self.evaluator.files.keep(&mut self.st);
        Ok(())
    }

    /// Keep in state each `memo.first` value the apply read that state does
    /// not keep yet (R-60), a secret one sealed with the stack's key.
    fn keep_memos(&mut self) -> Result<()> {
        crate::memo::keep(
            &mut self.st,
            self.evaluator.externs.memos(),
            &self.cx.master,
            &crate::memo::now(),
        )
    }

    /// An apply that resumes one interrupted: the remaining deformations
    /// come back as facts with the documents they were planned against, as
    /// the held ones do at a boundary: the evaluator derives the deny when
    /// the world moved under one (`zset::POLICY_RULES`).
    fn resume(&mut self, violations: &[String]) -> Result<()> {
        self.resumed = self.st.in_flight.take();
        self.r.resuming.set(self.resumed.is_some());
        let Some(f) = &self.resumed else {
            return Ok(());
        };
        let backend = self.backend();
        let remaining = executor::remaining(f);
        // As the record keeps it: a sensitive leaf by its digest.
        let observed = backend.stored_world(&backend.observe(&self.st)?);
        let changed = executor::changed_under(backend, &remaining, &observed);
        if !changed.is_empty() {
            eprint!(
                "the world changed under a remaining action:\n{}",
                executor::format_changes(&changed)
            );
        }
        let facts = zset::deformation_facts(
            remaining.keys().map(|a| ("remaining", a)),
            &remaining,
            &observed,
        );
        let (after, denies) =
            self.evaluator
                .evaluate_with(&self.st, &BTreeSet::new(), &facts, Some(f.tick))?;
        // The program's own violations did not refuse it (a destroy's):
        // only what the remaining actions add does.
        let denies: Vec<String> = denies
            .into_iter()
            .filter(|d| !violations.contains(d))
            .collect();
        if denies.is_empty() {
            return Ok(());
        }
        let redact = query::Redactor::new(&after.facts, backend.schema());
        eprintln!("constraint violations:");
        for d in &denies {
            eprintln!("- {}", report::violation_line(d, &redact));
        }
        self.persist()?;
        let verb = self.cx.cli.cmd.verb();
        Err(Refused::apply(
            verb,
            0,
            denies.len(),
            Some(format!(
                "on the remaining actions of the interrupted {verb}: review `dform plan`, \
                 then {verb} again"
            )),
        )
        .into())
    }

    /// One tick: planned, shown, asked for, approved, its calls made; then
    /// the apply's end, or the boundary the next tick plans from.
    fn tick(&mut self, wanted: Wanted) -> Result<Next> {
        // Between ticks, a signal stops the apply here.
        crate::interrupt::check()?;
        let Wanted {
            res,
            violations,
            resources,
            adopts,
            lifecycle,
        } = wanted;
        let planned =
            self.evaluator
                .plan(res, &violations, resources, &adopts, &lifecycle, &self.st)?;
        if self.tick == 1 {
            self.r
                .check_saved(&planned.plan, &planned.res, &planned.sections)?;
        }
        self.say_configured(&planned.res)?;
        let mut t = self.decide(planned)?;
        if let Some(o) = self.show(&t)? {
            return Ok(Next::Done(o));
        }
        self.approve(&t)?;
        if self.tick == 1 {
            self.log_start()?;
        }
        // A run that does not hold the master (R-164) makes no change that
        // needs it, nor one that depends on one: they wait for an apply
        // with it, and this one stops after the tick.
        let needing = needing_master(
            &t.planned.plan,
            &t.planned.resources,
            &self.st,
            self.backend(),
        );
        t.planned
            .plan
            .actions
            .retain(|a| !needing.contains_key(&a.addr));
        let (seen, pending, changed) = self.call(&mut t, &adopts, &lifecycle)?;
        if !needing.is_empty() {
            self.st.in_flight = None;
            self.persist()?;
            return Ok(Next::Done(Outcome::Stopped {
                tick: self.tick,
                why: needing_text(
                    self.deployment(),
                    &needing,
                    self.cx.master.without.as_deref(),
                ),
            }));
        }
        if !t.boundary && self.args.destroy {
            return self.finish_destroy(&t.planned.unreachable).map(Next::Done);
        }
        if !t.boundary {
            return self.finish(&t.planned.res, t.undeformed).map(Next::Done);
        }
        if !changed && let Some(h) = self.hook.as_deref_mut() {
            // Everything definite is held: wait for the next event.
            self.st.in_flight = None;
            self.cx.dep.save_state(&self.st)?;
            let backend = &*self.evaluator.backend;
            h.finish(
                &self.cx.deployment,
                false,
                &backend.stored_world(&backend.observe(&self.st)?),
            )?;
            return Ok(Next::Done(Outcome::Done));
        }
        if !changed {
            self.wait(&t)?;
        }
        if self.tick == self.args.max_ticks {
            bail!(
                "apply stopped after {} ticks (--max-ticks): the stack still has changes",
                self.args.max_ticks
            );
        }
        let wanted = self.boundary(&seen, &pending)?;
        self.tick += 1;
        Ok(Next::Tick(Box::new(wanted)))
    }

    /// A provider the last tick made the settings of known (a kubeconfig
    /// read from the server it created) was configured at the boundary:
    /// said, a secret setting as `(sensitive)`, and logged by its keys,
    /// never a value.
    fn say_configured(&self, res: &engine::EvalResult) -> Result<()> {
        let why = self.r.why();
        let tick = self.tick;
        for name in self.evaluator.take_configured() {
            let (keys, shown): (Vec<String>, Vec<String>) = self
                .evaluator
                .settings_shown(&name, &res.facts, why >= report::Why::How)
                .into_iter()
                .unzip();
            if why != report::Why::None {
                println!(
                    "provider {name}: configured after tick {}: {}",
                    tick - 1,
                    shown.join(", ")
                );
            }
            self.cx.audit.append(
                "configure",
                serde_json::json!({ "tick": tick - 1, "provider": name, "settings": keys }),
            )?;
        }
        Ok(())
    }

    /// What the tick's plan needs (approvals, its digest, at tick 1 the
    /// `plan` entry of the log), what the controller's gate lets through,
    /// what it holds and whether a boundary follows.
    fn decide(&mut self, planned: Planned) -> Result<Tick> {
        let tick = self.tick;
        let saved = &self.cx.saved;
        // What the policy pass says needs an approval, and the digest of
        // this plan: the file's, else of the plan as a file would record
        // it.
        let mut needs = crate::approval::needs(&planned.res.facts);
        if let (1, Some((_, f))) = (tick, saved) {
            needs.extend(
                f.needs_approval
                    .iter()
                    .map(|n| (n.deformation.clone(), n.reason.clone())),
            );
            needs.sort();
            needs.dedup();
        }
        let digest = match saved {
            _ if tick > 1 && (self.hook.is_none() || needs.is_empty()) => None,
            Some((path, f)) => {
                let d = f.digest();
                if f.digest.as_ref().is_some_and(|x| *x != d) {
                    bail!(
                        "plan file {}: its digest {} is not its content's ({d}): it was edited \
                         after the plan",
                        path.display(),
                        f.digest.as_deref().unwrap_or_default()
                    );
                }
                Some(d)
            }
            None => Some(
                self.r
                    .plan_file(
                        &planned.plan,
                        &planned.res,
                        &planned.sections,
                        &planned.resources,
                        &self.st,
                        self.key(),
                        self.inputs.clone(),
                    )?
                    .digest(),
            ),
        };
        if tick == 1 {
            self.cx.audit.append(
                "plan",
                serde_json::json!({
                    "digest": digest,
                    "documents": crate::diff::documents(&self.evaluator.tables.sources()),
                    "file": saved.as_ref().map(|(p, _)| p.display().to_string()),
                    "inputs": self.inputs,
                    "needs_approval": needs
                        .iter()
                        .map(|(d, r)| serde_json::json!({ "deformation": d, "reason": r }))
                        .collect::<Vec<_>>(),
                    "who": crate::audit::who(),
                }),
            )?;
        }
        let mut t = Tick {
            planned,
            needs,
            digest,
            held: Vec::new(),
            boundary: false,
            undeformed: false,
        };
        self.gate(&mut t)?;
        let (plan, sections) = (&t.planned.plan, &t.planned.sections);
        t.held = plan
            .actions
            .iter()
            .filter_map(|a| waits_on(a, sections))
            .flatten()
            .collect();
        // A create_before_destroy replacement deposes an object that is
        // deleted at the next tick, once what depends on it has moved to
        // the replacement.
        t.boundary = !t.held.is_empty()
            || !sections.pending_groups.is_empty()
            || !sections.undetermined.is_empty()
            || plan
                .actions
                .iter()
                .any(|a| matches!(a.kind, ActionKind::Replace { create_first: true }));
        // The controller applies ticks until a plan is undeformed: every
        // tick that changes something is followed by another.
        if self.hook.is_some() {
            t.boundary |= plan
                .actions
                .iter()
                .any(|a| !matches!(a.kind, ActionKind::Noop) && waits_on(a, sections).is_none());
        }
        Ok(t)
    }

    /// Controller mode: the report is one log line, and the policy pass
    /// gates what this tick may apply; a deformation that needs an approval
    /// is held until a token for the plan's digest arrives.
    fn gate(&mut self, t: &mut Tick) -> Result<()> {
        let Some(h) = self.hook.as_deref_mut() else {
            return Ok(());
        };
        let p = &mut t.planned;
        let text = self
            .r
            .report(&p.plan, &p.res, &p.sections, self.tick, &[], &p.denies)
            .text();
        t.undeformed = text
            .lines()
            .next()
            .is_some_and(|l| l.ends_with(" is up to date"));
        h.gate(self.tick, &mut p.plan, &p.res.facts, &text);
        let (false, Some(digest)) = (t.needs.is_empty(), &t.digest) else {
            return Ok(());
        };
        let mut tokens: Vec<String> = p
            .res
            .facts
            .iter()
            .filter(|a| a.pred == "approval")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(t))] => Some(t.clone()),
                _ => None,
            })
            .collect();
        tokens.extend(h.dropped_tokens());
        let (mut ok, mut refused) = (None, Vec::new());
        for token in &tokens {
            match self.approvals.verify(token, &t.needs, digest, &p.res.facts) {
                Ok(v) => {
                    ok = Some(v);
                    break;
                }
                Err(e) if e.is::<crate::approval::OtherDigest>() => {}
                Err(e) => refused.push(e.to_string()),
            }
        }
        h.approvals(
            self.tick,
            &mut p.plan,
            &t.needs,
            digest,
            ok.as_ref(),
            &refused,
        )
    }

    /// A batch apply's tick shown and asked for: the plan printed, a deny
    /// over it refused, tick 1 confirmed (and what it empties, R-80), a
    /// later tick that adds what no plan listed asked for again or, of a
    /// plan file or an approval, stopped before. `Some`: the apply ends.
    fn show(&mut self, t: &Tick) -> Result<Option<Outcome>> {
        let tick = self.tick;
        let p = &t.planned;
        let why = self.r.why();
        if self.hook.is_none() {
            if why == report::Why::None && (tick > 1 || t.boundary) {
                println!("tick {tick}:");
            } else if why != report::Why::None && tick > 1 {
                // A later tick that only waits has no section of its own
                // in the report: its header says which tick the report is
                // of.
                let report = self
                    .r
                    .report(&p.plan, &p.res, &p.sections, tick, &[], &p.denies);
                if !report.undeformed && report.changes() == 0 {
                    println!("tick {tick}  0 changes");
                }
            }
            self.r
                .show(&p.plan, &p.res, &p.sections, tick, &[], &p.denies);
            if tick == 1 {
                print!("{}", unreachable_text(&p.unreachable));
            }
        }
        if !p.denies.is_empty() {
            let redact = query::Redactor::new(&p.res.facts, self.backend().schema());
            eprintln!("constraint violations:");
            for d in &p.denies {
                eprintln!("- {}", report::violation_line(d, &redact));
            }
            let at = (tick > 1).then(|| {
                format!(
                    "stopped at tick {tick}; ticks 1 to {} were applied",
                    tick - 1
                )
            });
            return Err(Refused::apply(self.cx.cli.cmd.verb(), 0, p.denies.len(), at).into());
        }
        if tick == 1
            && let Some(o) = self.confirm_first(t)?
        {
            return Ok(Some(o));
        }
        // A later tick whose plan holds an address no earlier one listed (a
        // pending group's member, named only now) asks again, its plan
        // printed above, counting the new ones; with a plan file too, whose
        // groups bound them (`check_saved`).
        let addresses: BTreeSet<&ir::Address> = p
            .plan
            .actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop))
            .map(|a| &a.addr)
            .collect();
        if tick > 1
            && self.hook.is_none()
            && let Some(o) = self.confirm_later(t, &addresses)?
        {
            return Ok(Some(o));
        }
        if tick == 1 {
            self.scheduled = self
                .r
                .report(&p.plan, &p.res, &p.sections, tick, &[], &p.denies)
                .ticks
                .into_iter()
                .filter(|(t, _)| *t > 1)
                .flat_map(|(_, xs)| xs)
                .collect();
            // What the later ticks are compared with at their boundaries:
            // the plan file's delta, else this plan's.
            self.shown_delta = match &self.cx.saved {
                Some((_, f)) => f.deformations.clone(),
                None => self.r.delta(&p.plan, &p.res, &p.sections, tick, self.key()),
            };
        }
        self.listed.extend(addresses.into_iter().cloned());
        self.on_provider = p
            .resources
            .iter()
            .filter(|r| self.evaluator.provider_wait(&r.addr.typ).is_some())
            .map(|r| r.addr.clone())
            .collect();
        if self.hook.is_none() {
            self.unnamed = self
                .r
                .report(&p.plan, &p.res, &p.sections, tick, &[], &p.denies)
                .groups
                .iter()
                .map(|g| {
                    let on: Vec<String> = g.on.iter().map(|n| report::attribute_label(n)).collect();
                    format!("{} on {}", report::address_text(&g.pattern), on.join(", "))
                })
                .collect();
        }
        Ok(None)
    }

    /// A batch apply asks before it changes anything, unless `--yes` or it
    /// applies a reviewed plan file; what it carries over from an
    /// interrupted apply is marked. What the plan empties since the last
    /// apply is asked for on its own, also under `--yes` or of a plan
    /// file, unless `--allow-empty` names it (R-80).
    fn confirm_first(&self, t: &Tick) -> Result<Option<Outcome>> {
        if self.hook.is_some() {
            return Ok(None);
        }
        let (tick, p) = (self.tick, &t.planned);
        let style = self.cx.cli.style;
        print!(
            "{}",
            executor::carried_over(self.resumed.as_ref(), &self.st, &p.plan)
        );
        if !self.args.yes && self.cx.saved.is_none() {
            let report = self
                .r
                .report(&p.plan, &p.res, &p.sections, tick, &[], &p.denies);
            if !report.undeformed {
                let n = report.changes();
                self.still_held()?;
                if !confirm(n, false, self.args.destroy, self.deployment(), tick, style)? {
                    return Ok(Some(declined(self.deployment(), tick)));
                }
            }
        }
        for e in self.r.emptied(&p.plan, &p.res, &|_| None) {
            self.still_held()?;
            if !confirm_emptied(&e, self.deployment(), style)? {
                return Ok(Some(declined(self.deployment(), tick)));
            }
        }
        Ok(None)
    }

    /// A later tick: what no earlier tick listed, what `later` held for a
    /// provider's settings, and what differs from the plan shown (After
    /// R-156) are asked for again; of a plan file or an approval, the
    /// apply stops before the tick instead, the state consistent: the next
    /// apply plans them as its tick 1. `--yes` applies it.
    fn confirm_later(
        &mut self,
        t: &Tick,
        addresses: &BTreeSet<&ir::Address>,
    ) -> Result<Option<Outcome>> {
        let (tick, p) = (self.tick, &t.planned);
        let new = addresses
            .iter()
            .filter(|a| !self.listed.contains(**a))
            .count();
        if new > 0 && self.shown {
            let unnamed = std::mem::take(&mut self.unnamed);
            return self
                .stop(Stopped {
                    tick: tick - 1,
                    new,
                    unnamed,
                    on_provider: false,
                    differs: Vec::new(),
                })
                .map(Some);
        }
        // What `later` showed waiting on a provider, planned now against
        // it: asked for as tick 1 was, unless `--yes`; a plan file or an
        // approval did not see it.
        let planned = addresses
            .iter()
            .filter(|a| self.on_provider.contains(**a) && !self.scheduled.contains(&a.to_string()))
            .count();
        if planned > 0 && self.shown {
            return self
                .stop(Stopped {
                    tick: tick - 1,
                    new: planned,
                    unnamed: Vec::new(),
                    on_provider: true,
                    differs: Vec::new(),
                })
                .map(Some);
        }
        // The tick as its boundary re-plans it, against the tick as the
        // plan shown had it (After R-156): the same changes and values, a
        // value the plan did not know whatever it became. One that differs
        // is printed below the tick with what differs, and asked for
        // again; a plan file or an approval stops before it, naming what
        // differs.
        let now = self.r.delta(&p.plan, &p.res, &p.sections, tick, self.key());
        let differs =
            zset::file::tick_differences(&self.shown_delta, &now, tick, &self.ran, self.key());
        if !differs.is_empty() {
            println!("tick {tick} differs from the plan shown:");
            for d in &differs {
                println!("{}", d.line());
            }
        }
        // A change gone from the tick is said, not asked for: the tick does
        // less than was shown.
        let more = differs.iter().any(|d| d.mark != '-');
        if more && self.shown {
            let mut names: Vec<String> = Vec::new();
            for n in differs.iter().map(|d| d.name()) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
            return self
                .stop(Stopped {
                    tick: tick - 1,
                    new: differs.len(),
                    unnamed: Vec::new(),
                    on_provider: false,
                    differs: names,
                })
                .map(Some);
        }
        let asked = new + planned > 0 || more;
        if asked && !self.args.yes {
            self.still_held()?;
            if !confirm(
                new + planned,
                true,
                self.args.destroy,
                self.deployment(),
                tick,
                self.cx.cli.style,
            )? {
                return Ok(Some(declined(self.deployment(), tick)));
            }
        }
        // What was asked for (or `--yes` applied) is what the next boundary
        // compares with.
        if asked {
            let key = |e: &zset::file::Entry| (e.typ.clone(), e.name.clone());
            let mut by: BTreeMap<_, _> = self.shown_delta.drain(..).map(|e| (key(&e), e)).collect();
            by.extend(now.into_iter().map(|e| (key(&e), e)));
            self.shown_delta = by.into_values().collect();
        }
        Ok(None)
    }

    /// The apply stopped before a tick, the state consistent.
    fn stop(&mut self, stopped: Stopped) -> Result<Outcome> {
        self.st.in_flight = None;
        self.persist()?;
        Ok(stopped.into())
    }

    /// A batch apply's approval, before its Apply calls.
    fn approve(&mut self, t: &Tick) -> Result<()> {
        if self.hook.is_some() {
            return Ok(());
        }
        let asked = match (self.cx.saved.is_some(), self.args.destroy) {
            (true, _) => Asked::File,
            (false, true) => Asked::Destroy,
            (false, false) => Asked::Apply,
        };
        self.approvals
            .entry(self.tick, t, self.args.approval.as_deref(), asked)
    }

    /// Tick 1's entries in the audit log: the apply's start (who, the
    /// commit, the providers, a dirty tree's modified files), the secret
    /// outputs it opened, a new master and who could open it, and the
    /// master sealed again as dform.toml says (R-164).
    fn log_start(&mut self) -> Result<()> {
        let (cx, audit) = (self.cx, &self.cx.audit);
        let master = &cx.master;
        let dir = cx.cli.files[0]
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let commit = crate::project::git_head(dir);
        let mut e = serde_json::json!({
            "who": crate::audit::who(),
            "dform": env!("CARGO_PKG_VERSION"),
            "commit": commit,
            "providers": self.backend().names(),
            "protocol": crate::plugin::backend::VERSION,
        });
        // A dirty tree: the commit is not the program; which tracked files
        // it had modified.
        let modified = match commit {
            Some(_) => crate::project::git_modified(dir),
            None => Vec::new(),
        };
        if !modified.is_empty() {
            e["dirty"] = true.into();
            e["modified"] = modified.into();
        }
        if self.args.destroy {
            e["destroy"] = true.into();
        }
        audit.append("apply_start", e)?;
        if !cx.opened.is_empty() {
            audit.append(
                "opened",
                serde_json::json!({
                    "outputs": cx.opened,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        // The master this apply derives with, as state records it (R-163):
        // a new one is said in the log, and where it came from.
        if let Some(id) = master
            .id
            .as_ref()
            .filter(|id| self.st.master.as_ref() != Some(*id))
        {
            audit.append(
                "master",
                serde_json::json!({
                    "from": self.st.master,
                    "to": id,
                    "source": master.source,
                    "made": master.made,
                    "who": crate::audit::who(),
                }),
            )?;
            self.st.master = Some(id.clone());
        }
        // A new master sealed to recipients: who could open it from the
        // start (the offboarding list's first entry).
        if master.made && !cx.mixing.recipients.is_empty() {
            audit.append(
                "recipients",
                serde_json::json!({
                    "added": recipients_json(&cx.mixing.recipients),
                    "removed": [],
                    "id": master.id,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        // A plain key file now sealed (R-164); the master sealed again to
        // the recipients dform.toml names now.
        let Some(done) =
            crate::custody::reseal(cx.dep.store().as_ref(), &cx.deployment, master, &cx.mixing)?
        else {
            return Ok(());
        };
        if done.key_file {
            audit.append(
                "custody",
                serde_json::json!({
                    "sealed": store::KEY,
                    "into": store::MASTER,
                    "id": master.id,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        if !done.added.is_empty() || !done.removed.is_empty() || done.passphrase.is_some() {
            audit.append(
                "recipients",
                serde_json::json!({
                    "added": recipients_json(&done.added),
                    "removed": recipients_json(&done.removed),
                    "passphrase": done.passphrase.map(|p| match p {
                        true => "added",
                        false => "removed",
                    }),
                    "id": master.id,
                    "epoch": master.epoch,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        Ok(())
    }

    /// The tick's calls: its in-flight record written, then every definite
    /// action applied (`executor::run_tick`), each to the audit log, the
    /// tick's block on stderr filling in (R-127). What the executor saw
    /// of the world, what the tick held, and whether it changed anything.
    fn call(
        &mut self,
        t: &mut Tick,
        adopts: &[ir::Adopt],
        lifecycle: &zset::Lifecycle,
    ) -> Result<(executor::Seen, BTreeSet<ir::Address>, bool)> {
        let tick = self.tick;
        let backend = self.backend();
        let p = &mut t.planned;
        let sections = &p.sections;
        self.ran.extend(
            p.plan
                .actions
                .iter()
                .filter(|a| !matches!(a.kind, ActionKind::Noop) && waits_on(a, sections).is_none())
                .map(|a| (a.addr.typ.clone(), a.addr.name.clone())),
        );
        // Kept in state: a sensitive leaf by its digest.
        let observed = backend.stored_world(&backend.observe(&self.st)?);
        executor::begin(&mut self.st, tick, &p.plan, &observed);
        if let Some(f) = self.st.in_flight.as_mut() {
            f.destroy = self.args.destroy;
        }
        let deployment = self.deployment();
        executor::mark_creates(
            &mut self.st,
            deployment,
            p.plan
                .actions
                .iter()
                .filter(|a| waits_on(a, sections).is_none()),
        );
        self.persist()?;
        let pending: BTreeSet<ir::Address> = p
            .plan
            .actions
            .iter()
            .filter(|a| waits_on(a, sections).is_some())
            .map(|a| a.addr.clone())
            .collect();
        let mut seen: executor::Seen = observed.into_iter().map(|(a, d)| (a, Some(d))).collect();
        p.plan.actions.retain(|a| waits_on(a, sections).is_none());
        let changed = p
            .plan
            .actions
            .iter()
            .any(|a| !matches!(a.kind, ActionKind::Noop));
        // Every apply is at least one tick of the fake world, also when
        // there is nothing to do. Each Apply call's change of state is
        // logged before the next call (`executor`, `wal`).
        if tick == 1 || changed {
            seen.extend(self.run_calls(p, adopts, lifecycle)?);
            // The world as the executor saw it, keyed like a secret: a
            // document may hold one.
            let world: serde_json::Map<String, serde_json::Value> = seen
                .iter()
                .map(|(a, d)| (state::key(a), d.clone().unwrap_or_default()))
                .collect();
            let canonical = crate::approval::canonical_json(&world.into());
            // Without the master there is no digest of it (R-164).
            self.cx.audit.append(
                "tick",
                serde_json::json!({
                    "tick": tick,
                    "world": self.key().map(|k| format!("hmac-sha256:{}", k.digest(canonical.as_bytes()))),
                }),
            )?;
        }
        Ok((seen, pending, changed))
    }

    /// The tick's Apply calls, each logged: its result, its remote id, and
    /// a digest of its redacted diff; then the tick's checkpoint and its
    /// `tick` entry, a digest of the world as the executor saw it.
    fn run_calls(
        &mut self,
        p: &Planned,
        adopts: &[ir::Adopt],
        lifecycle: &zset::Lifecycle,
    ) -> Result<executor::Seen> {
        let tick = self.tick;
        let (backend, audit, key) = (self.backend(), &self.cx.audit, self.key());
        let (plan, res, sections) = (&p.plan, &p.res, &p.sections);
        let redact = query::Redactor::new(&res.facts, backend.schema());
        let diffs: BTreeMap<ir::Address, String> = self
            .r
            .delta(plan, res, sections, tick, key)
            .into_iter()
            .map(|e| {
                let digest =
                    crate::approval::digest_of(&serde_json::to_value(&e).unwrap_or_default());
                (
                    ir::Address {
                        typ: e.typ,
                        name: e.name,
                    },
                    digest,
                )
            })
            .collect();
        let audit_failed = std::cell::RefCell::new(None);
        // What a forget drops from state (R-154): its remote id, which the
        // log keeps.
        let forgotten: BTreeMap<ir::Address, String> = plan
            .actions
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::Forget))
            .filter_map(|a| Some((a.addr.clone(), self.st.get(&a.addr)?.remote.clone())))
            .collect();
        let on_action =
            |a: &crate::provider::Action, err: Option<&anyhow::Error>, st: &state::State| {
                if let Some(remote) = forgotten.get(&a.addr) {
                    let e = serde_json::json!({
                        "tick": tick,
                        "address": a.addr.to_string(),
                        "remote": remote,
                        "why": "lifecycle retain",
                    });
                    if let Err(x) = audit.append("forgot", e) {
                        audit_failed.borrow_mut().get_or_insert(x);
                    }
                    return;
                }
                let mut e = serde_json::json!({
                    "tick": tick,
                    "action": zset::deformation_kind(&a.kind, false).unwrap_or("no-op"),
                    "address": a.addr.to_string(),
                    "result": if err.is_some() { "failed" } else { "ok" },
                    "remote": st.get(&a.addr).map(|e| e.remote.clone()),
                    "diff": diffs.get(&a.addr),
                });
                if let Some(err) = err {
                    crate::audit::error(&mut e, &redact.text(&crate::diag::shape(err)));
                }
                if let Err(x) = audit.append("action", e) {
                    audit_failed.borrow_mut().get_or_insert(x);
                }
            };
        let dep = &self.cx.dep;
        let fence = || dep.check_fence();
        let record = |st: &state::State| dep.record(st);
        // The tick's block on stderr, filling in (R-127); a controller's
        // log line is its report.
        let mode = crate::progress::Mode::of_stderr(self.r.why() == report::Why::None);
        let progress = self.hook.is_none().then(|| {
            let actions: Vec<&crate::provider::Action> = plan.actions.iter().collect();
            let mut block = report::progress::Block::new(tick, &actions);
            // A failure's site, where its change is derived (R-109).
            block.sites =
                report::sites(res, actions.iter().map(|a| &a.addr), self.r.top.as_deref());
            crate::progress::Progress::tick(
                block,
                mode,
                match mode {
                    crate::progress::Mode::Terminal => self.cx.cli.style,
                    _ => report::Style::default(),
                },
            )
        });
        let on_event = |e: executor::Event| {
            if let Some(p) = &progress {
                p.event(&e, &|t: &str| redact.text(t));
            }
        };
        let opts = executor::Options {
            parallel: self.args.parallel as usize,
            persist: &record,
            stop_after: self.stop_after.as_ref(),
            on_action: Some(&on_action),
            before_submit: Some(&fence),
            on_event: Some(&on_event),
        };
        let applied = executor::run_tick(
            backend,
            &p.resources,
            adopts,
            lifecycle,
            &mut self.st,
            plan,
            &opts,
        );
        let failed = progress.map(|p| p.finish()).unwrap_or_default();
        // A failure the block said in full below it is not said again: the
        // run ends naming what failed (R-109).
        let applied = match applied {
            Err(e) if !failed.is_empty() && e.is::<report::Failure>() => Err(anyhow::anyhow!(
                "apply {}: tick {tick} failed: {}",
                self.cx.deployment,
                failed
                    .iter()
                    .map(report::address)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            applied => applied,
        };
        for note in backend.take_notes() {
            println!("chaos: {note}");
        }
        if let Some(e) = audit_failed.into_inner() {
            return Err(e);
        }
        // The tick's checkpoint, also of a tick that failed; not of one
        // chaos `stop-after` stopped as if dform were killed: its calls'
        // answers are in the log alone, and the next run replays them.
        let killed = self.stop_after.as_ref().is_some_and(|left| left.get() == 0);
        let checkpoint = match killed {
            true => Ok(()),
            false => self.persist(),
        };
        log_retries(audit, &redact, backend, tick)?;
        let seen = applied?;
        checkpoint?;
        Ok(seen)
    }

    /// A destroy's last tick: what no Delete could reach stays in state,
    /// and the destroy stops there (exit 5); else the deployment is gone.
    fn finish_destroy(&mut self, unreachable: &[(ir::Address, String)]) -> Result<Outcome> {
        let (tick, deployment) = (self.tick, self.deployment());
        self.st.in_flight = None;
        // Run again once their provider can be configured, or retain them.
        let kept: BTreeSet<String> = unreachable.iter().map(|(a, _)| state::key(a)).collect();
        let left: Vec<&String> = self
            .st
            .resources
            .keys()
            .chain(self.st.deposed.keys())
            .filter(|k| !kept.contains(*k))
            .collect();
        if !left.is_empty() {
            bail!(
                "destroy {deployment}: state still holds {} after the last tick",
                left.iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !unreachable.is_empty() {
            self.persist()?;
            let n = match unreachable.len() {
                1 => "1 object".to_string(),
                n => format!("{n} objects"),
            };
            return Ok(Outcome::Stopped {
                tick,
                why: format!(
                    "destroy {deployment}: stopped; {n} no Delete could reach stay in state \
                     (listed under `unreachable`): destroy again once their provider can be \
                     configured, or forget them with `lifecycle(r, \"retain\")`"
                ),
            });
        }
        // The deployment is gone: its checkpoint is empty but for the count
        // of idempotency keys given out, so a later apply never reuses one;
        // the audit log keeps its history (R-146) and says it was
        // destroyed, which `stack list` reads.
        let st = std::mem::take(&mut self.st);
        self.st = state::State {
            version: st.version,
            keys: st.keys,
            ..Default::default()
        };
        self.persist()?;
        self.cx.audit.append(
            "destroyed",
            serde_json::json!({ "deployment": deployment, "who": crate::audit::who() }),
        )?;
        // What other stacks read of it goes with it: a reader waits on it as
        // on one never applied (R-121).
        let store = self.cx.dep.store();
        if self.cx.cli.world.is_none() && store.get(store::OUTPUTS)?.is_some() {
            store.delete(store::OUTPUTS)?;
        }
        // No closing line: the block showed every change finish, and the
        // exit status says how it ended.
        Ok(Outcome::Done)
    }

    /// The apply's last tick: the rotations it carried made (R-161), an
    /// epoch no secret derives from any more retired (R-165), what it
    /// derived recorded for the next plan (R-80), the state's memos and
    /// copies kept, and the stack's outputs published.
    fn finish(&mut self, res: &engine::EvalResult, undeformed: bool) -> Result<Outcome> {
        let (cx, audit) = (self.cx, &self.cx.audit);
        self.st.in_flight = None;
        for r in self.st.secrets.values_mut() {
            r.pending = false;
        }
        let mut keep = self.st.epochs_in_use();
        keep.insert(cx.master.epoch.max(1));
        for (epoch, id) in crate::custody::retire(cx.dep.store().as_ref(), &keep)? {
            audit.append(
                "retired",
                serde_json::json!({
                    "epoch": epoch,
                    "id": id,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        // What this apply derived, for the next plan's guardrail and policy
        // pass (R-80).
        let relations: BTreeSet<String> = self
            .located
            .loaded
            .lowered
            .as_ref()
            .map(|l| l.signatures.keys().map(|(p, _)| p.clone()).collect())
            .unwrap_or_default();
        let redact = query::Redactor::new(&res.facts, self.backend().schema());
        let record = zset::Derived::of(res, &redact, &relations, self.r.top.as_deref());
        if record != zset::Derived::default() {
            audit.append("derived", serde_json::json!({ "record": record }))?;
        }
        // The stack's outputs, as the world now is, for other stacks to
        // read (evaluated again only when it has any: the evaluation
        // refreshes). A --world fixture is not registered: everything stays
        // beside the world file.
        self.keep_memos()?;
        self.evaluator.files.keep(&mut self.st);
        crate::tables::record(&mut self.st.externs, &self.evaluator.externs.recorded());
        // The copies state still holds resources of (R-67).
        let held: Vec<ir::Address> = self
            .st
            .resources
            .keys()
            .filter_map(|k| state::parse_key(k))
            .collect();
        self.st.instances = crate::zset::Instances::from_facts(&res.facts)
            .with(&self.st.instances)
            .kept(&held);
        if let Some(stopped) = self.publish(res)? {
            return Ok(stopped);
        }
        // No closing line: the block showed every change finish, and the
        // exit status says how it ended.
        if let Some(h) = self.hook.as_deref_mut() {
            let backend = &*self.evaluator.backend;
            h.finish(
                &self.cx.deployment,
                undeformed,
                &backend.stored_world(&backend.observe(&self.st)?),
            )?;
        }
        Ok(Outcome::Done)
    }

    /// The stack's outputs, a secret one by its label and digest, never its
    /// value (E DR-19), a ref resolved, as the world is; a secret output no
    /// provider holds sealed to each deployment of the project that reads
    /// it (R-166). Published beside the state, apart from it, and the
    /// deployment registered. `Some`: a secret output this run cannot
    /// digest (R-164) stops the apply, its changes made.
    fn publish(&mut self, res: &engine::EvalResult) -> Result<Option<Outcome>> {
        let (cx, audit, backend) = (self.cx, &self.cx.audit, self.backend());
        let (deployment, master, root) = (self.deployment(), &cx.master, &cx.root);
        let secret_types = crate::stack::secret_output_types(&self.evaluator.program);
        let readers = match secret_types.is_empty() || cx.cli.world.is_some() {
            true => Vec::new(),
            false => readers_of(root, deployment, &open_s3(root, false))?,
        };
        let sealing = std::cell::RefCell::new(None);
        // The same value to the same reader seals the same: the published
        // outputs move only when a value or a reader does.
        let seed = master
            .key
            .as_ref()
            .map(|k| crate::secrets::derived(k, "dform sealed output seed"));
        let seal = |path: &str, v: &serde_json::Value| {
            let mut out = BTreeMap::new();
            let plain = crate::approval::canonical_json(v);
            for (r, public) in &readers {
                let Some(public) = public else { continue };
                let label = sealed_label(deployment, path, r);
                match crate::custody::seal_to(public, &label, plain.as_bytes(), seed.as_ref()) {
                    Ok(b) => {
                        out.insert(r.clone(), b);
                    }
                    Err(e) => {
                        sealing.borrow_mut().get_or_insert(e);
                    }
                }
            }
            out
        };
        let key = self.key();
        let outputs = if crate::stack::has_outputs(&res.facts) {
            crate::stack::outputs(
                &self.evaluator.evaluate(&self.st)?.0.facts,
                &secret_types,
                &backend.observe(&self.st)?,
                &self.st,
                deployment,
                &|b| key.map(|k| k.digest(b)),
                &seal,
            )
        } else {
            Default::default()
        };
        if let Some(e) = sealing.into_inner() {
            return Err(e);
        }
        // A grant that changed is said in the log: who may now open which
        // output.
        let grants = |o: &BTreeMap<String, crate::stack::SecretOutput>| {
            o.iter()
                .filter(|(_, x)| !x.sealed.is_empty())
                .map(|(k, x)| (k.clone(), x.sealed.keys().cloned().collect::<Vec<_>>()))
                .collect::<BTreeMap<_, _>>()
        };
        if grants(&outputs.secret) != grants(&self.st.secret_outputs) {
            audit.append(
                "sealed",
                serde_json::json!({
                    "outputs": grants(&outputs.secret),
                    "who": crate::audit::who(),
                }),
            )?;
        }
        // A secret output this run cannot digest (R-164): not published;
        // the apply stops, its changes made.
        if !outputs.unproven.is_empty() {
            self.persist()?;
            return Ok(Some(Outcome::Stopped {
                tick: self.tick,
                why: format!(
                    "apply {deployment}: stopped; the secret outputs {} changed, and only the \
                     deployment's master ({}) can publish them: apply again with it",
                    outputs.unproven.join(", "),
                    master.without.as_deref().unwrap_or("not held")
                ),
            }));
        }
        self.st.outputs = outputs.known.clone();
        self.st.secret_outputs = outputs.secret.clone();
        self.persist()?;
        // Published beside the state, apart from it: what other stacks
        // read.
        let world = cx.cli.world.is_some();
        if !world
            && (!outputs.is_empty()
                || !secret_types.is_empty()
                || cx.dep.store().get(store::OUTPUTS)?.is_some())
        {
            let published = crate::stack::Published::new(deployment, &outputs);
            cx.dep.publish(&published.bytes())?;
        }
        // Every deployment of a keyed stack is registered.
        let stack_cfg = &self.located.loaded.cfg;
        let keyed = self.located.instance.segment().is_some();
        if (!outputs.is_empty() || !secret_types.is_empty() || stack_cfg.bootstrap || keyed)
            && !world
        {
            crate::stack::register(
                root,
                deployment,
                &self.located.location,
                stack_cfg.bootstrap,
            )?;
        }
        Ok(None)
    }

    /// A tick with nothing definite to apply waits for what waiting can
    /// resolve (R-81): the world reaching a value (a Job's status), an
    /// extern's "not yet", up to its providers' `wait` (R-122); what it
    /// cannot is an error.
    fn wait(&mut self, t: &Tick) -> Result<()> {
        let (tick, backend, externs) = (self.tick, self.backend(), &self.evaluator.externs);
        let sections = &t.planned.sections;
        let mut waits: Vec<String> = sections.blocking.iter().cloned().collect();
        waits.extend(t.held.iter().cloned());
        waits.sort();
        waits.dedup();
        let on = waiting_on(sections, &self.st, externs);
        if on.is_empty() {
            // A null by its label; a provider's settings as `later` names
            // them (R-110).
            let waits: Vec<String> = waits
                .iter()
                .map(|n| match n.starts_with("provider ") {
                    true => n.clone(),
                    false => report::attribute_label(n),
                })
                .collect();
            bail!(
                "apply stopped at tick {tick}: nothing definite to apply, still waiting on {}",
                waits.join(", ")
            );
        }
        // Said as printed (R-111); the audit log keeps the labels.
        let names: Vec<String> = on.iter().map(|l| report::attribute_label(l)).collect();
        let labels: Vec<String> = on.iter().map(|l| ir::label(l)).collect();
        let budget = self.wait_budget(&on);
        let mut w = crate::progress::Wait::new();
        // On a terminal, the wait is one line counting up (R-127);
        // elsewhere its lines say it every 10s.
        let mode = crate::progress::Mode::of_stderr(self.r.why() == report::Why::None);
        let live = (mode == crate::progress::Mode::Terminal)
            .then(|| crate::progress::Progress::wait(tick, names.clone(), mode, self.cx.cli.style));
        let resolved = loop {
            if live.is_none() {
                w.tick(&names);
            }
            if w.elapsed() >= budget {
                break false;
            }
            if crate::interrupt::sleep(w.next_poll(budget)) {
                break false;
            }
            backend.reread();
            externs.forget_not_yet();
            let (next, _) =
                self.evaluator
                    .evaluate_with(&self.st, &BTreeSet::new(), &[], Some(tick))?;
            let docs = ir::compile_resources(next.facts.iter().cloned(), backend.schema())?;
            let now = deployment::sections(&next, &docs, backend.schema());
            if waiting_on(&now, &self.st, externs) != on {
                break true;
            }
        };
        if let Some(l) = live {
            l.done();
        }
        let redact = query::Redactor::new(&t.planned.res.facts, backend.schema());
        log_retries(&self.cx.audit, &redact, backend, tick)?;
        let result = match (resolved, crate::interrupt::requested()) {
            (true, _) => "resolved",
            (false, Some(_)) => "interrupted",
            (false, None) => "expired",
        };
        self.cx.audit.append(
            "wait",
            crate::audit::wait(tick, &labels, w.since(), w.elapsed(), result),
        )?;
        if resolved {
            return Ok(());
        }
        // Nothing of the tick was applied: the next apply plans it again, as
        // an unattended stop does.
        self.st.in_flight = None;
        self.evaluator.files.keep(&mut self.st);
        self.persist()?;
        crate::interrupt::check()?;
        bail!(
            "apply stopped at tick {tick}: waited {} on {}, still unknown (the provider's \
             `wait` in dform.toml); state is consistent: run apply again to wait again",
            crate::plugin::policy::show(budget),
            names.join(", ")
        );
    }

    /// How long a tick waits on the nulls `on` (R-122): the longest `wait`
    /// of the providers that answer them, as dform.toml sets it (a
    /// built-in extern's by its `[providers.NAME]` table), else 10m; not
    /// its calls' `timeout`.
    fn wait_budget(&self, on: &[String]) -> std::time::Duration {
        let backend = self.backend();
        on.iter()
            .map(|l| {
                let set = || {
                    let (t, _) = crate::value::null_owner(l)?;
                    let serving = self.waits.iter().find(|(n, _)| backend.serves(n, &t));
                    match serving {
                        Some((_, d)) => Some(*d),
                        None => self.waits.get(t.split_once('.')?.0).copied(),
                    }
                };
                set().unwrap_or(crate::project::WAIT)
            })
            .max()
            .unwrap_or_default()
    }

    /// The boundary. The held deformations come back as facts with the
    /// documents they were planned against: the evaluator derives the deny
    /// when the world moved under one. A destroy is refused by what its
    /// held changes derive, not by the program's own violations.
    fn boundary(&self, seen: &executor::Seen, pending: &BTreeSet<ir::Address>) -> Result<Wanted> {
        let (tick, backend) = (self.tick, self.backend());
        let held = executor::check_boundary(backend, seen, pending, &self.st, tick)?;
        let (next, violations) =
            self.evaluator
                .evaluate_with(&self.st, &BTreeSet::new(), &held, Some(tick))?;
        let redact = query::Redactor::new(&next.facts, backend.schema());
        for w in &next.warnings {
            eprintln!("warning: {}", redact.text(w));
        }
        let refusing: Vec<&String> = match self.args.destroy {
            false => violations.iter().collect(),
            true => {
                let (_, own) = self.evaluator.evaluate(&self.st)?;
                violations.iter().filter(|v| !own.contains(v)).collect()
            }
        };
        if !refusing.is_empty() {
            eprintln!("constraint violations after tick {tick}:");
            for v in &refusing {
                eprintln!("- {}", report::violation_line(v, &redact));
            }
            let conflicts = refusing.iter().filter(|v| report::is_conflict(v)).count();
            return Err(Refused::apply(
                self.cx.cli.cmd.verb(),
                conflicts,
                refusing.len() - conflicts,
                Some(format!(
                    "stopped after tick {tick}; ticks 1 to {tick} were applied"
                )),
            )
            .into());
        }
        Ok(Wanted {
            resources: ir::compile_resources(next.facts.iter().cloned(), backend.schema())?,
            adopts: ir::compile_adopts(next.facts.iter())?,
            lifecycle: zset::Lifecycle::from_facts(&next.facts, backend.schema())?,
            res: next,
            violations,
        })
    }
}

/// What an apply's approvals are verified against (docs/reference.md,
/// "Approvals"): a token verifies against the stack's trust root (loaded
/// once), for this plan's digest and this deployment, by an approver
/// `approver_allowed` admits when the program restricts them.
struct Approvals<'a> {
    cx: &'a Context,
    located: &'a deployment::Located,
    roots: std::cell::OnceCell<crate::approval::Roots>,
    /// The program restricts who may approve (`approver_allowed`).
    restricts: bool,
    /// The approval this apply was given, verified.
    approved: Option<crate::approval::Verified>,
}

impl Approvals<'_> {
    /// `token` verified for the plan of digest `digest`, which needs
    /// `needs` approved.
    fn verify(
        &self,
        token: &str,
        needs: &[(String, String)],
        digest: &str,
        facts: &BTreeSet<Atom>,
    ) -> Result<crate::approval::Verified> {
        let stack_cfg = &self.located.loaded.cfg;
        if stack_cfg.approvals.is_empty() {
            bail!(
                "stack {} has no approvals trust root (dform.toml: `[stacks.{}] approvals = \
                 'jwks(\"https://...\")'`)",
                self.cx.deployment,
                self.located.loaded.stack
            );
        }
        let roots = match self.roots.get() {
            Some(r) => r,
            None => {
                let r = crate::approval::load_roots(&stack_cfg.approvals, &self.cx.cache)?;
                self.roots.get_or_init(|| r)
            }
        };
        let instance = &self.located.instance;
        let expect = crate::approval::Expect {
            stack: &instance.stack,
            key: &instance.key,
            digest,
            now: crate::approval::now(),
        };
        let allowed = |w: &str, d: &str| crate::approval::approver_allowed(facts, w, d);
        let allowed: Option<executor::Allowed> = if self.restricts { Some(&allowed) } else { None };
        executor::approve(token, needs, roots, &expect, allowed)
    }

    /// A batch apply's approval, before its Apply calls: at tick 1 the
    /// token given (`--approval FILE`), verified, or, with none, a refusal
    /// if anything needs one; at a later tick, a new deformation that needs
    /// one must be one the approver may approve. Each verdict at tick 1
    /// goes to the audit log.
    fn entry(&mut self, tick: usize, t: &Tick, token: Option<&Path>, asked: Asked) -> Result<()> {
        let (needs, facts, audit) = (&t.needs, &t.planned.res.facts, &self.cx.audit);
        let allowed =
            |w: &str, d: &str| !self.restricts || crate::approval::approver_allowed(facts, w, d);
        let list = |needs: &[(String, String)]| {
            let names: Vec<String> = needs.iter().map(|(d, r)| format!("{d} ({r})")).collect();
            match names.len() {
                1 => format!("{} needs", names[0]),
                _ => format!("{} need", names.join(", ")),
            }
        };
        if tick > 1 {
            if needs.is_empty() {
                return Ok(());
            }
            let Some(v) = &self.approved else {
                bail!(
                    "apply stopped at tick {tick}: {} an approval, and the apply has none",
                    list(needs)
                );
            };
            let who = &v.statement.approver;
            let refused: Vec<(String, String)> = needs
                .iter()
                .filter(|(d, _)| !allowed(who, d))
                .cloned()
                .collect();
            if !refused.is_empty() {
                bail!(
                    "apply stopped at tick {tick}: approver_allowed({who:?}, D) does not hold \
                     for {}",
                    refused
                        .iter()
                        .map(|(d, _)| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            return Ok(());
        }
        let digest = t.digest.as_deref().unwrap_or_default();
        let Some(path) = token else {
            if needs.is_empty() {
                return audit
                    .append("approval", serde_json::json!({ "result": "not required" }))
                    .map(drop);
            }
            let error = format!("{} an approval, and no --approval was given", list(needs));
            let mut entry = serde_json::json!({ "result": "refused", "digest": digest });
            crate::audit::error(&mut entry, &error);
            audit.append("approval", entry)?;
            let (verb, how) = match asked {
                Asked::File => (
                    "apply",
                    "apply it with --approval FILE, a signed approval of that digest",
                ),
                Asked::Apply => (
                    "apply",
                    "write the plan with `plan --out PLAN`, have its digest approved, and \
                     `apply PLAN --approval FILE`",
                ),
                Asked::Destroy => (
                    "destroy",
                    "have that digest approved (`plan --destroy` prints it) and \
                     `destroy --approval FILE`",
                ),
            };
            bail!("{verb} refused: {error}; the plan's digest is {digest}: {how}");
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read --approval {}: {e}", path.display()))?;
        match self.verify(&text, needs, digest, facts) {
            Ok(v) => {
                audit.append(
                    "approval",
                    serde_json::json!({ "result": "approved", "digest": digest, "attestation": v }),
                )?;
                println!("approved by {}: plan digest {digest}", v.statement.approver);
                self.approved = Some(v);
                Ok(())
            }
            Err(e) => {
                let mut entry = serde_json::json!({ "result": "refused", "digest": digest });
                crate::audit::error(&mut entry, &e.to_string());
                audit.append("approval", entry)?;
                let verb = match asked {
                    Asked::Destroy => "destroy",
                    _ => "apply",
                };
                bail!("{verb} refused: {e}")
            }
        }
    }
}

/// What an approval is of: a plan file, an apply's plan, a destroy's.
#[derive(Clone, Copy, PartialEq)]
enum Asked {
    File,
    Apply,
    Destroy,
}

/// Ask on the terminal whether to apply `n` changes to `deployment`
/// (`destroy`: to delete its `n` objects), or (`new`) tick `tick`, which
/// adds what no plan listed: only `y` or `yes` proceeds.
/// With no terminal to ask on, a refusal naming `--yes`, never a wait.
/// Answered no, `false`: a decline, not an error.
fn confirm(
    n: usize,
    new: bool,
    destroy: bool,
    deployment: &str,
    tick: usize,
    style: report::Style,
) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    let stdin = std::io::stdin();
    let verb = match destroy {
        true => "destroy",
        false => "apply",
    };
    if !stdin.is_terminal() {
        bail!(
            "{verb} {deployment}: nothing to ask on at tick {tick} (stdin is not a terminal); \
             pass --yes to {verb} without asking"
        );
    }
    let ask = match (new, destroy, n) {
        (true, _, _) => format!("Apply tick {tick} to {deployment}?"),
        (false, true, 1) => format!("Destroy this object of {deployment}?"),
        (false, true, _) => format!("Destroy these {n} objects of {deployment}?"),
        (false, false, 1) => format!("Apply this change to {deployment}?"),
        (false, false, _) => format!("Apply these {n} changes to {deployment}?"),
    };
    print!("{} [y/N] ", style.paint(report::Paint::Bold, &ask));
    std::io::stdout().flush()?;
    let answer = answer()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// A line from the terminal, the answer to a question. A signal while it
/// waits (Ctrl-C at the prompt) is the stop it asks for (`interrupt`):
/// nothing was applied for the question. The line is read on a thread of
/// its own, which a stop leaves blocked on stdin until the process exits.
fn answer() -> Result<String> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("dform-prompt".into())
        .spawn(move || {
            let mut line = String::new();
            let r = std::io::stdin().lock().read_line(&mut line).map(|_| line);
            let _ = tx.send(r);
        })?;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(line) => return Ok(line?),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::interrupt::requested().is_some() {
                    // The prompt's line ends here, not the shell's.
                    println!();
                    crate::interrupt::check()?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("internal: the prompt's reader is gone")
            }
        }
    }
}

/// Ask whether to apply a plan that empties `e` (R-80), on a terminal
/// only: there is no `--yes` for it, only `--allow-empty`. Answered no,
/// `false`.
fn confirm_emptied(e: &zset::Emptied, deployment: &str, style: report::Style) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!(
            "apply {deployment}: {}; nothing to ask on (stdin is not a terminal): confirm it \
             on a terminal, or pass --allow-empty {} if it is meant",
            e.what(),
            e.flag()
        );
    }
    let what = e.what();
    let ask = format!("T{}. Apply it anyway?", &what[1..]);
    print!("{} [y/N] ", style.paint(report::Paint::Warn, &ask));
    std::io::stdout().flush()?;
    let answer = answer()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// An apply whose confirmation of `tick` was answered no: at tick 1
/// nothing was applied, and nothing is said; later, what the earlier ticks
/// did. The audit log's `apply_end` says `declined`, and at which tick.
fn declined(deployment: &str, tick: usize) -> Outcome {
    let why = (tick > 1).then(|| {
        format!(
            "apply {deployment}: not confirmed at tick {tick}; ticks 1 to {} were applied, \
             and the next apply resumes from there",
            tick - 1
        )
    });
    Outcome::Declined { tick, why }
}

/// An apply of a plan file or an approval that stopped after `tick`: the
/// next tick adds `new` changes no printed plan named (`unnamed`, the
/// groups the last plan held them as). The audit log's `apply_end` says `stopped`.
#[derive(Debug)]
struct Stopped {
    tick: usize,
    new: usize,
    unnamed: Vec<String>,
    /// The changes are what `later` held waiting on a provider's settings,
    /// named but planned only now (R-45): the plan file or approval did
    /// not see their diff.
    on_provider: bool,
    /// The tick re-planned at its boundary differs from the tick the plan
    /// file or approval showed (After R-156): what it differs in, each an
    /// address or an attribute as the plan prints it.
    differs: Vec<String>,
}

impl From<Stopped> for Outcome {
    fn from(s: Stopped) -> Outcome {
        Outcome::Stopped {
            tick: s.tick,
            why: s.to_string(),
        }
    }
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = if self.new == 1 { "" } else { "s" };
        let groups = match self.unnamed.as_slice() {
            [] => String::new(),
            gs => format!(" ({})", gs.join("; ")),
        };
        let what = match self.on_provider {
            _ if !self.differs.is_empty() => format!(
                "differs from the plan it applies: {}",
                self.differs.join(", ")
            ),
            true => format!(
                "plans {} change{s} `later` held for a provider's settings, which the \
                 approved plan did not show",
                self.new
            ),
            false => format!(
                "adds {} change{s} the plan could not name{groups}",
                self.new
            ),
        };
        write!(
            f,
            "apply stopped after tick {}: tick {} {what}; run apply again to plan them \
             against the world as it now is",
            self.tick,
            self.tick + 1,
        )
    }
}

/// The nulls a tick waits on that waiting can resolve (R-81,
/// `deployment::waitable`): of the stuck rules' and the held resources'.
fn waiting_on(
    sections: &stuck::Sections,
    st: &state::State,
    externs: &crate::externs::Externs,
) -> Vec<String> {
    let on: BTreeSet<String> = sections
        .blocking
        .iter()
        .chain(sections.pending.values().flatten())
        .cloned()
        .collect();
    deployment::waitable(&on, st, &externs.not_yet())
}

/// Every provider call sent again since the last time (R-81) to the audit
/// log, a `retry` entry each, its error redacted.
fn log_retries(
    audit: &crate::audit::Log,
    redact: &query::Redactor,
    backend: &crate::plugin::Providers,
    tick: usize,
) -> Result<()> {
    for r in backend.take_retries() {
        audit.append(
            "retry",
            crate::audit::retry(tick, &r, redact.text(&r.error)),
        )?;
    }
    Ok(())
}

/// The actions of `plan` a run that does not hold the master cannot make
/// (R-164), each with the paths that need it (`Providers::needs_master`),
/// and what depends on one (no paths): what references it, and the
/// delete of what it references.
fn needing_master(
    plan: &crate::provider::Plan,
    desired: &[ir::Resource],
    st: &state::State,
    backend: &crate::plugin::Providers,
) -> std::collections::BTreeMap<ir::Address, Vec<String>> {
    let mut out = std::collections::BTreeMap::new();
    if !crate::secrets::standin::active() {
        return out;
    }
    let doc = |a: &ir::Address| {
        desired
            .iter()
            .find(|r| r.addr == *a)
            .map(|r| crate::engine::value_to_json(&r.attrs))
    };
    for a in &plan.actions {
        let paths = backend.needs_master(a, doc(&a.addr).as_ref());
        if !paths.is_empty() {
            out.insert(a.addr.clone(), paths);
        }
    }
    let deps = |a: &ir::Address| -> BTreeSet<ir::Address> {
        let mut d: BTreeSet<ir::Address> = desired
            .iter()
            .find(|r| r.addr == *a)
            .map(|r| r.deps.clone())
            .unwrap_or_default();
        d.extend(
            st.get(a)
                .into_iter()
                .flat_map(|e| e.deps.iter().filter_map(|k| state::parse_key(k))),
        );
        d
    };
    loop {
        let more: Vec<ir::Address> = plan
            .actions
            .iter()
            .filter(|a| !out.contains_key(&a.addr))
            .filter(|a| match a.kind {
                ActionKind::Noop | ActionKind::Pending => false,
                ActionKind::Delete | ActionKind::DeleteDeposed | ActionKind::Forget => {
                    out.keys().any(|n| deps(n).contains(&a.addr))
                }
                _ => deps(&a.addr).iter().any(|d| out.contains_key(d)),
            })
            .map(|a| a.addr.clone())
            .collect();
        if more.is_empty() {
            return out;
        }
        for m in more {
            out.insert(m, Vec::new());
        }
    }
}

/// Why a run that does not hold the master stopped: what it did not make.
fn needing_text(
    deployment: &str,
    needing: &std::collections::BTreeMap<ir::Address, Vec<String>>,
    without: Option<&str>,
) -> String {
    let n = match needing.len() {
        1 => "1 change".to_string(),
        n => format!("{n} changes"),
    };
    let mut out = format!(
        "apply {deployment}: stopped; {n} need its master ({}), and were not made:",
        without.unwrap_or("not held")
    );
    for (a, paths) in needing {
        let what = match paths.as_slice() {
            [] => "depends on one of these".to_string(),
            [p] if p.is_empty() => "holds a secret only the master derives".to_string(),
            ps => format!(
                "{} only the master derives",
                ps.iter()
                    .filter(|p| !p.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        out.push_str(&format!("\n  {}: {what}", report::address(a)));
    }
    out.push_str("\nevery other change was made; apply again with the master to make these");
    out
}
