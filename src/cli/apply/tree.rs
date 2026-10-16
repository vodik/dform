//! One apply over a tree of deployments (one plan over the dependency
//! tree): the deployments of `apply X` (X and those it reads) or of the
//! project, each a namespace of its own (its state, its lease, its audit
//! log, its outputs) and nothing about ordering: the ticks are the
//! tree's.
//!
//! The tree is one run: each provider started once for it, each
//! deployment's calls in a session of its own (`matrix::OneRun`,
//! `plugin::pool`). Every deployment is evaluated in apply order, each
//! over what those it reads plan to publish, and planned for tick 1;
//! every lease is taken before the first call. Each tick plans every
//! deployment still changing, in apply order, a reader's wait on an
//! output its upstream knows only after a tick scheduled the tick after;
//! tick 1's plan of the tree is shown and confirmed once. A tick's calls
//! are the tree's: every deployment's are walked together
//! (`executor::run_tick`, one part each), each whose dependencies are met
//! submitted, under one window, each deployment's state written, its
//! fence checked and its audit log told as its own calls go; their block
//! is one, each deployment's changes under its name. Then the boundaries
//! in apply order, each reader evaluated over what its upstreams'
//! boundaries made known. A deployment whose plan holds nothing more is
//! done: its outputs published, its apply's end logged and its lease
//! released. A tick waits on what it cannot reach yet only when no
//! deployment changed anything in it.

use super::super::evaluated::{Context, Evaluated, Objects};
use super::super::order::Dependency;
use super::super::planning::Reporter;
use super::super::{Cli, Cmd, Outcome, Session};
use super::prompt::{confirm, declined};
use super::ticks::{Calls, Ran, TickPlan, Ticks, failed_of};
use super::{Apply, Next, Wanted};
use crate::report::progress::Block;
use crate::said::Teller;
use crate::{deployment, executor, report, resources, stack, state, zset};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// A deployment of the tree, evaluated: what its ticks borrow.
struct Member {
    d: Dependency,
    cx: Context,
    located: deployment::Located,
    evaluator: deployment::Evaluator,
}

/// What a member's ticks start from: its state, the moves `moved/3`
/// made, its violations, and its tick 1's plan, made as the tree was
/// evaluated to feed what reads it.
struct Seed {
    st: state::State,
    moves: Vec<(crate::address::Address, crate::address::Address)>,
    violations: Vec<String>,
    first: First,
}

/// A tick's plan and what its calls are made with.
type First = (deployment::Planned, Vec<resources::Adopt>, zset::Lifecycle);

/// The tree's apply of the deployments of `order` (apply order), each
/// run as `of` makes it.
pub(in crate::cli) fn apply(
    cli: &Cli,
    order: &[Dependency],
    of: &dyn Fn(&Dependency) -> Cli,
) -> Result<Outcome> {
    let Cmd::Apply(args) = &cli.cmd else {
        bail!("internal: an apply");
    };
    let _run = super::super::matrix::OneRun::begin(order);
    let (members, seeds) = evaluate(order, of)?;
    let teller = Teller {
        quiet: args.why == report::Why::None,
        style: cli.style,
    };
    run(args, &teller, &members, seeds)
}

/// Each deployment of `order` evaluated in apply order, over what those
/// it reads plan to publish, and planned for tick 1.
fn evaluate(
    order: &[Dependency],
    of: &dyn Fn(&Dependency) -> Cli,
) -> Result<(Vec<Member>, Vec<Seed>)> {
    let mut members: Vec<Member> = Vec::new();
    let mut seeds: Vec<Seed> = Vec::new();
    let mut planned: BTreeMap<String, stack::Planned> = BTreeMap::new();
    for d in order {
        let mut cli = of(d);
        cli.planned = planned.clone();
        // Its `== NAME` line names it.
        cli.held = super::super::Held::new();
        let Evaluated { cx, ev, .. } = evaluated(cli)?;
        let deployment::Evaluation {
            located,
            st,
            moves,
            res,
            violations,
            compiled,
            evaluator,
            ..
        } = ev;
        let compiled = compiled?;
        let plan = evaluator.plan(
            res,
            &violations,
            compiled.resources,
            &compiled.adopts,
            &compiled.lifecycle,
            &st,
        )?;
        {
            let r = Reporter::new(&cx, &evaluator, &located, &st);
            let report = r.report(&plan.plan, &plan.res, &plan.sections, 1, &[], &plan.denies);
            let outputs = r.outputs_of(&plan.res, &st, &report.outputs_after)?;
            planned.insert(d.name.clone(), outputs);
        }
        seeds.push(Seed {
            st,
            moves,
            violations,
            first: (plan, compiled.adopts, compiled.lifecycle),
        });
        members.push(Member {
            d: d.clone(),
            cx,
            located,
            evaluator,
        });
    }
    Ok((members, seeds))
}

/// The run `cli` evaluated, as an apply's run is: of a plan file's
/// section, with its inputs.
fn evaluated(mut cli: Cli) -> Result<Evaluated<'static>> {
    let file = match &cli.cmd {
        Cmd::Apply(a) => a.plan_file.clone(),
        _ => None,
    };
    let saved = match file {
        Some(p) => {
            let f = cli.with_plan_inputs(&p)?;
            Some((p, f))
        }
        None => None,
    };
    let loaded = cli.load(None)?;
    loaded.require_provider()?;
    let objects = Objects::locate(cli, loaded, None, None)?;
    Evaluated::new(objects, saved, None, None)
}

/// The tree's ticks over its members': each member's lease taken, then
/// tick after tick until each is done or one stops the tree; each member's
/// apply's end logged as its own.
fn run(args: &Apply, teller: &Teller, members: &[Member], seeds: Vec<Seed>) -> Result<Outcome> {
    let reporters: Vec<Reporter> = members
        .iter()
        .zip(&seeds)
        .map(|(m, s)| Reporter::new(&m.cx, &m.evaluator, &m.located, &s.st))
        .collect();
    let mut sessions: Vec<Option<Session>> = members.iter().map(|_| None).collect();
    let mut tree = Tree {
        args,
        teller,
        members,
        reporters: &reporters,
        ticks: Vec::new(),
        next: Vec::new(),
        ended: members.iter().map(|_| None).collect(),
        at: None,
        tick: 1,
        stop_after: members
            .first()
            .and_then(|m| m.cx.chaos.stop_after)
            .map(std::cell::Cell::new),
    };
    let r = tree.start(seeds, &mut sessions).and_then(|()| tree.drive());
    let r = super::super::interrupted(r);
    tree.end(&r);
    r
}

/// The tree's ticks.
struct Tree<'a> {
    args: &'a Apply,
    teller: &'a Teller,
    members: &'a [Member],
    reporters: &'a [Reporter<'a>],
    ticks: Vec<Ticks<'a, 'static>>,
    /// What each member's coming tick is planned from; `None` once it is
    /// done.
    next: Vec<Option<Step>>,
    /// How each member ended, once it did.
    ended: Vec<Option<Outcome>>,
    /// The member a step of the tree was at when it failed.
    at: Option<usize>,
    tick: usize,
    /// Chaos `stop-after`: the tree's, counted across its ticks and its
    /// deployments' calls.
    stop_after: Option<std::cell::Cell<usize>>,
}

/// What a tick's walk did ([`Tree::walk`]): its outcome, each part's
/// audit log, and the changes its block said failed.
struct Made<'a> {
    walked: executor::Walked,
    logs: Vec<super::ticks::ActionLog<'a>>,
    failed: Vec<crate::report::progress::Change>,
}

/// What a member's coming tick is planned from.
enum Step {
    /// Tick 1's plan, made as the tree was evaluated.
    First(Box<First>),
    /// What its last boundary derived.
    Wanted(Box<Wanted>),
}

impl<'a> Tree<'a> {
    /// Each member's ticks begun: its chaos checked, its lease taken, what
    /// `moved/3` did said, and what an interrupted apply of it left
    /// resumed.
    fn start(&mut self, seeds: Vec<Seed>, sessions: &'a mut [Option<Session>]) -> Result<()> {
        let mut sessions = sessions.iter_mut();
        for (i, seed) in seeds.into_iter().enumerate() {
            self.enter(i);
            let m = &self.members[i];
            let session = sessions.next().expect("a session per member");
            let mut t = Ticks::new(
                self.args,
                &m.cx,
                &self.reporters[i],
                &m.evaluator,
                &m.located,
                None,
                session,
                seed.st,
            )?;
            t.check_chaos(&seed.first.0.resources)?;
            t.in_tree();
            t.begin()?;
            if !seed.moves.is_empty() {
                t.say(crate::said::Said::Moved(report::moved_text(&seed.moves)));
            }
            t.resume(&seed.violations)?;
            // The providers the plan's own evaluation configured.
            m.evaluator.take_configured();
            self.ticks.push(t);
            self.next.push(Some(Step::First(Box::new(seed.first))));
        }
        self.at = None;
        Ok(())
    }

    /// Tick after tick until every member is done, or one stops the tree.
    fn drive(&mut self) -> Result<Outcome> {
        loop {
            crate::interrupt::check()?;
            if let Some(o) = self.tick()? {
                return Ok(o);
            }
            self.tick += 1;
        }
    }

    /// One tick of the tree: each member still changing planned, the tick
    /// shown, every member's calls made together, then each boundary in
    /// apply order, a reader's over what its upstreams' made known.
    /// `Some`: the tree's apply ends.
    #[tracing::instrument(
        target = "dform",
        name = "tick",
        skip_all,
        fields(otel.name = %format!("tick {}", self.tick), tick = self.tick)
    )]
    fn tick(&mut self) -> Result<Option<Outcome>> {
        let active: Vec<usize> = (0..self.members.len())
            .filter(|i| self.next[*i].is_some())
            .collect();
        if active.is_empty() {
            return Ok(Some(self.outcome()));
        }
        let mut plans = self.plan(&active)?;
        if let Some(o) = self.show(&active, &plans)? {
            return Ok(Some(o));
        }
        for (&i, p) in active.iter().zip(&plans) {
            self.enter(i);
            self.ticks[i].approve(&p.tick)?;
            if self.tick == 1 {
                self.ticks[i].log_start()?;
            }
        }
        let ran = self.calls(&active, &mut plans)?;
        let changed = ran.iter().any(|r| r.changed);
        self.check_progress(&active, &plans, changed)?;
        for ((i, p), r) in active.iter().zip(plans).zip(ran) {
            self.enter(*i);
            let waits = !changed && self.ticks[*i].can_wait(&p.tick);
            match self.ticks[*i].after(p, r, waits)? {
                Next::Tick(w) => {
                    self.feed(*i, Some(&w.res))?;
                    self.next[*i] = Some(Step::Wanted(w));
                }
                Next::Done(o) => {
                    self.next[*i] = None;
                    self.ticks[*i].end(&Ok(o.clone()));
                    self.ended[*i] = Some(o.clone());
                    if o != Outcome::Done {
                        return Ok(Some(self.outcome()));
                    }
                    self.feed(*i, None)?;
                }
            }
        }
        self.at = None;
        Ok(None)
    }

    /// The tick's calls, every active member's together (their `plans`):
    /// each prepared, then one walk of the parts of those whose tick makes
    /// calls ([`Tree::walk`]), then what each did taken back
    /// ([`Tree::taken_back`]).
    fn calls(&mut self, active: &[usize], plans: &mut [TickPlan]) -> Result<Vec<Ran>> {
        let mut prepared = Vec::new();
        for (&i, p) in active.iter().zip(plans.iter_mut()) {
            self.enter(i);
            prepared.push(self.ticks[i].calls(p)?);
        }
        let making: Vec<usize> = (0..active.len())
            .filter(|&k| prepared[k].made(self.tick))
            .collect();
        let made = self.walk(active, plans, &making);
        self.taken_back(active, prepared, &making, made)
    }

    /// What each active member's calls (`prepared`) did, taken back in
    /// apply order ([`Ticks::called`]); `making` made calls, the walk's
    /// parts in order. A member whose calls failed stops the tree there,
    /// once every member's are taken back. A walk that stopped as a whole
    /// (chaos `stop-after`, a state that could not be written) stops it at
    /// once, the part it stopped at taken back, the others' answers in
    /// their write-ahead records alone.
    fn taken_back(
        &mut self,
        active: &[usize],
        mut prepared: Vec<Calls>,
        making: &[usize],
        made: Made,
    ) -> Result<Vec<Ran>> {
        let killed = self.stop_after.as_ref().is_some_and(|l| l.get() == 0);
        let Made {
            walked,
            mut logs,
            failed,
        } = made;
        let results = match walked {
            executor::Walked::Ended(results) => results,
            executor::Walked::Stopped { part, error } => {
                let k = making[part];
                self.enter(active[k]);
                let (calls, log) = (prepared.swap_remove(k), logs.swap_remove(part));
                let failed = failed_of(&failed, part);
                self.ticks[active[k]].called(calls, log, Err(error), &failed, killed)?;
                bail!("internal: a stopped walk's part took its calls back");
            }
        };
        let mut taken: Vec<Option<_>> = active.iter().map(|_| None).collect();
        for (part, ((&k, applied), log)) in making.iter().zip(results).zip(logs).enumerate() {
            taken[k] = Some((part, applied, log));
        }
        let mut ran = Vec::new();
        let mut failure = None;
        for ((&i, calls), taken) in active.iter().zip(prepared).zip(taken) {
            self.enter(i);
            let r = match taken {
                Some((part, applied, log)) => {
                    let failed = failed_of(&failed, part);
                    self.ticks[i].called(calls, log, applied, &failed, killed)
                }
                None => Ok(calls.ran()),
            };
            match r {
                Ok(r) => ran.push(r),
                Err(e) => {
                    failure.get_or_insert((i, e));
                }
            }
        }
        match failure {
            Some((i, e)) => {
                self.at = Some(i);
                Err(e)
            }
            None => Ok(ran),
        }
    }

    /// The walk of the members `making` (indices into `active` and
    /// `plans`), those whose tick makes calls: one part each, in apply
    /// order, under one window (`--parallel`); their block one, each
    /// member's changes under its name.
    fn walk(&mut self, active: &[usize], plans: &[TickPlan], making: &[usize]) -> Made<'a> {
        let Some(&first) = making.first() else {
            return Made {
                walked: executor::Walked::Ended(Vec::new()),
                logs: Vec::new(),
                failed: Vec::new(),
            };
        };
        let blocks = making
            .iter()
            .map(|&k| {
                let (i, p) = (active[k], &plans[k].tick.planned);
                (self.members[i].d.full.clone(), self.ticks[i].block(p))
            })
            .collect();
        let block = Block::of_parts(self.tick, blocks, std::time::Instant::now());
        let progress = self.ticks[active[first]].progress(block);
        let callings: Vec<_> = making
            .iter()
            .enumerate()
            .map(|(part, &k)| {
                let p = &plans[k].tick.planned;
                self.ticks[active[k]].calling(p, Some(&progress), part)
            })
            .collect();
        let opts = executor::Options {
            parallel: self.args.parallel as usize,
            stop_after: self.stop_after.as_ref(),
        };
        let mut ticks: Vec<Option<&mut Ticks<'a, 'static>>> =
            self.ticks.iter_mut().map(Some).collect();
        let parts = making
            .iter()
            .zip(&callings)
            .filter_map(|(&k, c)| Some(ticks[active[k]].take()?.part(&plans[k], c)))
            .collect();
        let walked = executor::run_tick(parts, &opts);
        let logs = callings.into_iter().map(|c| c.into_log()).collect();
        Made {
            walked,
            logs,
            failed: progress.finish(),
        }
    }

    /// Each active member's plan of this tick, in apply order: a reader's
    /// waits on an output its upstream knows only after a tick are
    /// scheduled by the upstream's plan of it.
    fn plan(&mut self, active: &[usize]) -> Result<Vec<TickPlan>> {
        let mut plans = Vec::new();
        for &i in active {
            self.enter(i);
            let p = match self.next[i].take() {
                Some(Step::First(f)) => {
                    let (plan, adopts, lifecycle) = *f;
                    self.ticks[i].planned(plan, adopts, lifecycle)?
                }
                Some(Step::Wanted(w)) => self.ticks[i].plan(*w)?,
                None => bail!("internal: a member done is planned"),
            };
            let after = self.ticks[i].outputs_after(&p.tick);
            for j in self.readers(i) {
                self.reporters[j].outside_of(&self.members[i].cx.stored, &after);
            }
            plans.push(p);
        }
        Ok(plans)
    }

    /// The tick shown: at tick 1, each member's plan under its name and
    /// one question for the tree; at a later tick, each member's as its
    /// own apply shows it. `Some`: the tree's apply ends.
    fn show(&mut self, active: &[usize], plans: &[TickPlan]) -> Result<Option<Outcome>> {
        let mut n = 0;
        for (&i, p) in active.iter().zip(plans) {
            self.enter(i);
            {
                let name = &self.members[i].d.full;
                let line = format!("== {name}");
                println!(
                    "{}",
                    self.members[i]
                        .cx
                        .cli
                        .style
                        .paint(report::Paint::Bold, &line)
                );
            }
            if let Some(o) = self.ticks[i].show(&p.tick)? {
                self.ticks[i].end(&Ok(o.clone()));
                self.ended[i] = Some(o.clone());
                return Ok(Some(o));
            }
            n += self.ticks[i].changes(&p.tick);
        }
        if self.tick > 1 || self.args.yes || n == 0 {
            return Ok(None);
        }
        let names: Vec<&str> = active
            .iter()
            .map(|&i| self.members[i].d.full.as_str())
            .collect();
        let named = names.join(", ");
        if !confirm(self.teller, n, false, self.args.destroy, &named, 1)? {
            return Ok(Some(declined(&named, 1)));
        }
        Ok(None)
    }

    /// A tick in which no member changed anything and none has anything
    /// of its own to wait on cannot move: the tree's apply stops there.
    fn check_progress(&self, active: &[usize], plans: &[TickPlan], changed: bool) -> Result<()> {
        if changed {
            return Ok(());
        }
        let stuck = active
            .iter()
            .zip(plans)
            .all(|(&i, p)| !p.tick.boundary || !self.ticks[i].can_wait(&p.tick));
        let held: Vec<(&str, String)> = active
            .iter()
            .zip(plans)
            .filter(|(_, p)| p.tick.boundary)
            .map(|(&i, p)| {
                let on = self.ticks[i].waited_on(&p.tick).join(", ");
                (self.members[i].d.full.as_str(), on)
            })
            .collect();
        let waits: Vec<String> = match held.as_slice() {
            [(_, on)] => vec![on.clone()],
            held => held.iter().map(|(d, on)| format!("{d}: {on}")).collect(),
        };
        if stuck && !held.is_empty() {
            bail!(
                "apply stopped at tick {}: nothing definite to apply, still waiting on {}",
                self.tick,
                waits.join("; ")
            );
        }
        Ok(())
    }

    /// What member `i` makes known feeds the members that read it: its
    /// outputs as its boundary's evaluation (`res`) has them, else as its
    /// final state does.
    fn feed(&mut self, i: usize, res: Option<&crate::engine::EvalResult>) -> Result<()> {
        let readers = self.readers(i);
        if readers.is_empty() {
            return Ok(());
        }
        self.enter(i);
        let outputs = self.ticks[i].outputs_now(res)?;
        let name = &self.members[i].d.name;
        for j in readers {
            let m = &self.members[j];
            let read = outputs.over(name, None);
            let facts =
                stack::output_facts(std::slice::from_ref(&read), &m.located.loaded.deployed);
            m.evaluator.read_again(name, facts);
            m.evaluator
                .backend
                .hold(stack::held(std::slice::from_ref(&read)));
        }
        Ok(())
    }

    /// The steps that follow are member `i`'s: a failure is its.
    fn enter(&mut self, i: usize) {
        self.at = Some(i);
    }

    /// The members not done that read member `i`.
    fn readers(&self, i: usize) -> Vec<usize> {
        let name = &self.members[i].d.name;
        (0..self.members.len())
            .filter(|&j| self.ended[j].is_none() && self.members[j].d.reads.contains(name))
            .collect()
    }

    /// The tree's outcome: the worst of its members'.
    fn outcome(&self) -> Outcome {
        let mut out = Outcome::Done;
        for o in self.ended.iter().flatten() {
            out = match (out, o) {
                (Outcome::Done, o) => o.clone(),
                (o, _) => o,
            };
        }
        out
    }

    /// Each member's apply that has not ended, ended as the tree's did:
    /// the member it failed at with its error, the others stopped there.
    fn end(&mut self, r: &Result<Outcome>) {
        let tick = self.tick;
        for (i, t) in self.ticks.iter_mut().enumerate() {
            if self.ended[i].is_some() {
                continue;
            }
            let own = match r {
                Err(e) if self.at == Some(i) => Err(anyhow::anyhow!("{e:#}")),
                // A stop for the tree: each of its applies stopped so.
                Ok(o @ (Outcome::Interrupted { .. } | Outcome::Declined { .. })) => Ok(o.clone()),
                _ => Ok(Outcome::Stopped {
                    tick,
                    why: format!("the tree's apply stopped at tick {tick}"),
                }),
            };
            t.end(&own);
        }
    }
}
