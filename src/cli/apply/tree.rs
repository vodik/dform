//! One apply over a tree of deployments (one plan over the dependency
//! tree): the deployments of `apply X` (X and those it reads) or of the
//! project, each a namespace of its own (its state, its lease, its audit
//! log, its outputs) and nothing about ordering: the ticks are the
//! tree's.
//!
//! The tree is one run: each provider started once for it, each
//! deployment's calls in a session of its own (`matrix::OneRun`,
//! `plugin::pool`). Its deployments are planned as the tree's plan plans
//! them (`matrix::Tree`): each on a thread of its own, all at once, each
//! waiting only on those it reads (`board`); each one's plan, the one
//! `plan` prints, is its tick 1. A deployment's evaluation and providers
//! never leave its thread: once planned it takes its seat ([`hub`]) and
//! does there what the tree's thread asks of it ([`member`]).
//!
//! Every lease is taken before the first call. Each tick plans every
//! deployment still changing, in apply order, a reader's wait on an
//! output its upstream knows only after a tick scheduled the tick after;
//! tick 1's plan of the tree is shown and confirmed once. A tick's calls
//! are the tree's: every deployment's part is walked at once, each on its
//! thread (`executor::run_tick`), under one window (`executor::Together`),
//! each deployment's state written, its fence checked and its audit log
//! told as its own calls go; their block is one, each deployment's
//! changes under its name. Then the boundaries in apply order, each
//! reader evaluated over what its upstreams' boundaries made known. A
//! deployment whose plan holds nothing more is done: its outputs
//! published, its apply's end logged and its lease released. A tick waits
//! on what it cannot reach yet only when no deployment changed anything
//! in it.

mod hub;
mod member;

pub(in crate::cli) use self::hub::Seat;
pub(in crate::cli) use self::member::serve;

use self::hub::{Hub, Seated};
use self::member::{After, Job, Side, Walking};
use super::super::matrix;
use super::super::order::Dependency;
use super::super::{Cli, Cmd, Outcome};
use super::Apply;
use super::prompt::{confirm, declined};
use crate::report::progress::Block;
use crate::said::Teller;
use crate::{executor, report};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

/// The tree's apply of the deployments of `order` (apply order), each
/// run as `of` makes it: planned as `plan` plans them, then ticked here.
pub(in crate::cli) fn apply(
    cli: &Cli,
    order: &[Dependency],
    of: &dyn Fn(&Dependency) -> Cli,
) -> Result<Outcome> {
    let Cmd::Apply(args) = &cli.cmd else {
        bail!("internal: an apply");
    };
    let _run = matrix::OneRun::begin(order);
    let hub = Hub::new(order.len());
    let seated = |d: &Dependency| {
        let mut run = of(d);
        let i = order
            .iter()
            .position(|o| o.full == d.full)
            .unwrap_or_default();
        run.seat = Some(hub.seat(i));
        run
    };
    let tree = matrix::Tree {
        cli,
        label: None,
        sites: BTreeMap::new(),
    };
    let teller = Teller {
        quiet: args.why == report::Why::None,
        style: cli.style,
    };
    let (planned, ticked) = tree.applied(order, &seated, || {
        let _closed = Closing(&hub);
        let members = hub.seated()?;
        Some(ticked(args, &teller, order, members))
    });
    match ticked {
        Some(r) => r,
        // One was not planned: the first whose plan failed says why.
        None => Err(planned
            .into_iter()
            .find_map(|e| e)
            .unwrap_or_else(|| anyhow::anyhow!("internal: a tree's apply with none planned"))),
    }
}

/// The hub closed once the tree's thread is done with it: every member's
/// thread let go, whatever ended the apply.
struct Closing<'h>(&'h Hub);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The tree's ticks over its members (`seated`, in apply order): each
/// member's lease taken, then tick after tick until each is done or one
/// stops the tree; each member's apply's end logged as its own.
fn ticked(
    args: &Apply,
    teller: &Teller,
    order: &[Dependency],
    seated: Vec<Seated>,
) -> Result<Outcome> {
    let stop_after = seated
        .first()
        .and_then(|m| m.stop_after)
        .map(|n| Arc::new(AtomicUsize::new(n)));
    let members: Vec<Member> = order
        .iter()
        .zip(seated)
        .map(|(d, s)| Member {
            d: d.clone(),
            seated: s,
        })
        .collect();
    let mut tree = Tree {
        args,
        teller,
        active: members.iter().map(|_| true).collect(),
        ended: members.iter().map(|_| None).collect(),
        members,
        at: None,
        tick: 1,
        stop_after,
    };
    let r = tree.start().and_then(|()| tree.drive());
    let r = super::super::interrupted(r);
    tree.end(&r);
    r
}

/// A deployment of the tree, at its seat.
struct Member {
    d: Dependency,
    seated: Seated,
}

impl Member {
    /// `f` done on the member's thread, under the span the tree's thread
    /// is in: once it has, its result.
    fn ask<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Side<'_>) -> R + Send + 'static,
    ) -> Result<R> {
        self.post(f)?
            .recv()
            .map_err(|_| anyhow::anyhow!("internal: {}'s thread ended", self.d.full))
    }

    /// `f` sent to the member's thread: what it will answer.
    fn post<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Side<'_>) -> R + Send + 'static,
    ) -> Result<mpsc::Receiver<R>> {
        let (tx, rx) = mpsc::channel();
        let span = tracing::Span::current();
        let job: Job = Box::new(move |side| {
            let _in = span.enter();
            let _ = tx.send(f(side));
        });
        self.seated
            .jobs
            .send(job)
            .map_err(|_| anyhow::anyhow!("internal: {}'s thread ended", self.d.full))?;
        Ok(rx)
    }
}

/// The tree's ticks.
struct Tree<'a> {
    args: &'a Apply,
    teller: &'a Teller,
    members: Vec<Member>,
    /// Whether each member is still changing.
    active: Vec<bool>,
    /// How each member ended, once it did.
    ended: Vec<Option<Outcome>>,
    /// The member a step of the tree was at when it failed.
    at: Option<usize>,
    tick: usize,
    /// Chaos `stop-after`: the tree's, counted across its ticks and its
    /// deployments' calls.
    stop_after: Option<Arc<AtomicUsize>>,
}

impl Tree<'_> {
    /// Each member's ticks begun ([`super::ticks::Ticks::start`]), in
    /// apply order.
    fn start(&mut self) -> Result<()> {
        for i in 0..self.members.len() {
            self.enter(i);
            self.members[i].ask(|s| s.start())??;
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
            .filter(|i| self.active[*i])
            .collect();
        if active.is_empty() {
            return Ok(Some(self.outcome()));
        }
        self.plan(&active)?;
        if let Some(o) = self.show(&active)? {
            return Ok(Some(o));
        }
        let first = self.tick == 1;
        for &i in &active {
            self.enter(i);
            self.members[i].ask(move |s| s.approve(first))??;
        }
        let changed = self.calls(&active)?;
        self.check_progress(&active, changed)?;
        for &i in &active {
            self.enter(i);
            match self.members[i].ask(move |s| s.after(!changed))?? {
                After::Tick => self.feed(i)?,
                After::Done(o) => {
                    self.active[i] = false;
                    self.ended[i] = Some(o.clone());
                    if o != Outcome::Done {
                        return Ok(Some(self.outcome()));
                    }
                    self.feed(i)?;
                }
            }
        }
        self.at = None;
        Ok(None)
    }

    /// Each active member's plan of this tick, in apply order: a reader's
    /// waits on an output its upstream knows only after a tick are
    /// scheduled by the upstream's plan of it.
    fn plan(&mut self, active: &[usize]) -> Result<()> {
        for &i in active {
            self.enter(i);
            let after = Arc::new(self.members[i].ask(|s| s.plan_tick())??);
            for j in self.readers(i) {
                let (full, after) = (self.members[i].seated.stored.clone(), after.clone());
                self.members[j].ask(move |s| s.outside_of(&full, &after))?;
            }
        }
        Ok(())
    }

    /// The tick shown: at tick 1, each member's plan under its name and
    /// one question for the tree; at a later tick, each member's as its
    /// own apply shows it. `Some`: the tree's apply ends.
    fn show(&mut self, active: &[usize]) -> Result<Option<Outcome>> {
        let mut n = 0;
        for &i in active {
            self.enter(i);
            let m = &self.members[i];
            let line = format!("== {}", m.d.full);
            println!("{}", m.seated.style.paint(report::Paint::Bold, &line));
            let (shown, changes) = m.ask(|s| s.show())??;
            if let Some(o) = shown {
                self.active[i] = false;
                self.ended[i] = Some(o.clone());
                return Ok(Some(o));
            }
            n += changes;
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

    /// The tick's calls, every active member's together: each prepared,
    /// then the parts of those whose tick makes calls walked at once, each
    /// on its member's thread ([`Tree::walk`]), then what each did taken
    /// back ([`Tree::taken_back`]). Whether the tick changed anything.
    fn calls(&mut self, active: &[usize]) -> Result<bool> {
        let tick = self.tick;
        let mut making = Vec::new();
        for &i in active {
            self.enter(i);
            if self.members[i].ask(move |s| s.prepare(tick))?? {
                making.push(i);
            }
        }
        let failed = self.walk(&making)?;
        self.taken_back(active, &making, &failed)
    }

    /// The walk of the members `making`, those whose tick makes calls: one
    /// part each, in apply order, each on its member's thread, under one
    /// window (`--parallel`); their block one, each member's changes under
    /// its name. The changes the block said failed.
    fn walk(&mut self, making: &[usize]) -> Result<Vec<report::progress::Change>> {
        let Some(&first) = making.first() else {
            return Ok(Vec::new());
        };
        let mut blocks = Vec::new();
        for &i in making {
            self.enter(i);
            let block = self.members[i].ask(|s| s.block())??;
            blocks.push((self.members[i].d.full.clone(), block));
        }
        let block = Block::of_parts(self.tick, blocks, std::time::Instant::now());
        let progress = Arc::new(self.members[first].ask(|s| s.progress(block))?);
        let together = Arc::new(executor::Together::new(
            making.len(),
            self.args.parallel as usize,
        ));
        let mut walks = Vec::new();
        for (part, &i) in making.iter().enumerate() {
            let walking = Walking {
                together: together.clone(),
                stop_after: self.stop_after.clone(),
                parallel: self.args.parallel as usize,
                part,
            };
            let progress = progress.clone();
            walks.push((
                i,
                self.members[i].post(move |s| s.walk(&progress, &walking.options()))?,
            ));
        }
        let mut walked = Ok(());
        for (i, w) in walks {
            let r = w
                .recv()
                .map_err(|_| anyhow::anyhow!("internal: {}'s thread ended", self.members[i].d.full))
                .and_then(|r| r);
            if let (Err(e), Ok(())) = (r, &walked) {
                self.at = Some(i);
                walked = Err(e);
            }
        }
        walked?;
        let progress = Arc::try_unwrap(progress)
            .map_err(|_| anyhow::anyhow!("internal: the tick's block outlives its walks"))?;
        if let Some(part) = together.stopped() {
            // A walk stopped dform as if killed: its member takes back
            // what it did, the others' answers in their write-ahead
            // records alone.
            let i = making[part];
            self.enter(i);
            let failed = progress.finish();
            let killed = executor::killed(self.stop_after.as_deref());
            self.members[i].ask(move |s| s.called(&failed, Some(part), killed))??;
            bail!("internal: a stopped walk's part took its calls back");
        }
        Ok(progress.finish())
    }

    /// What each active member's calls did, taken back in apply order
    /// ([`super::ticks::Ticks::called`]); `making` made calls, the walk's
    /// parts in order, and `failed` are the changes the block said
    /// failed. A member whose calls failed stops the tree there, once
    /// every member's are taken back. Whether any changed anything.
    fn taken_back(
        &mut self,
        active: &[usize],
        making: &[usize],
        failed: &[report::progress::Change],
    ) -> Result<bool> {
        let killed = executor::killed(self.stop_after.as_deref());
        let failed = Arc::new(failed.to_vec());
        let mut changed = false;
        let mut failure = None;
        for &i in active {
            self.enter(i);
            let part = making.iter().position(|&k| k == i);
            let failed = failed.clone();
            match self.members[i].ask(move |s| s.called(&failed, part, killed))? {
                Ok(c) => changed |= c,
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
            None => Ok(changed),
        }
    }

    /// A tick in which no member changed anything and none has anything
    /// of its own to wait on cannot move: the tree's apply stops there.
    fn check_progress(&mut self, active: &[usize], changed: bool) -> Result<()> {
        if changed {
            return Ok(());
        }
        let mut waiting = Vec::new();
        for &i in active {
            self.enter(i);
            waiting.push((i, self.members[i].ask(|s| s.waiting())??));
        }
        self.at = None;
        let stuck = waiting.iter().all(|(_, w)| !w.boundary || !w.can_wait);
        let held: Vec<(&str, String)> = waiting
            .iter()
            .filter(|(_, w)| w.boundary)
            .map(|(i, w)| (self.members[*i].d.full.as_str(), w.on.join(", ")))
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
    /// outputs as its boundary's evaluation has them, else as its final
    /// state does.
    fn feed(&mut self, i: usize) -> Result<()> {
        let readers = self.readers(i);
        if readers.is_empty() {
            return Ok(());
        }
        self.enter(i);
        let outputs = Arc::new(self.members[i].ask(|s| s.outputs())??);
        let name = self.members[i].d.name.clone();
        for j in readers {
            let (name, outputs) = (name.clone(), outputs.clone());
            self.members[j].ask(move |s| s.read(&name, &outputs))?;
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
        for (i, m) in self.members.iter().enumerate() {
            if self.ended[i].is_some() {
                continue;
            }
            let own = match r {
                Err(e) if self.at == Some(i) => Err(format!("{e:#}")),
                // A stop for the tree: each of its applies stopped so.
                Ok(o @ (Outcome::Interrupted { .. } | Outcome::Declined { .. })) => Ok(o.clone()),
                _ => Ok(Outcome::Stopped {
                    tick,
                    why: format!("the tree's apply stopped at tick {tick}"),
                }),
            };
            let ended = m.ask(move |s| s.end(&own.map_err(|e| anyhow::anyhow!(e))));
            if let Err(e) = ended {
                eprintln!("warning: {e:#}");
            }
        }
    }
}
