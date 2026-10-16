//! What a tree's plan knows of its deployments as their plans go
//! ([`Board`]), shared by the threads that plan them (`matrix::Tree`): a
//! deployment's outputs once its evaluation makes them known, before its
//! Plan calls and its policy; then its plan's, or why it was not planned.
//! A run of a deployment reads those it reads through it ([`Upstreams`])
//! and waits on nothing else: its evaluation on their evaluations, its
//! report (what it says of their outputs known only after a tick) on
//! their plans.
//!
//! An evaluation makes known what its plan makes known unless the plan's
//! Plan calls replace an object an output is made of, which is then known
//! only after the replace: a run that read otherwise than an upstream's
//! plan has it is planned again over the plan's, before it says anything
//! ([`Upstreams::planned`], [`Verdict::Again`]).

use crate::stack::Planned;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// How far a deployment of the tree's plan is.
#[derive(Debug)]
enum Known {
    /// Not evaluated yet.
    Pending,
    /// Evaluated: the outputs its evaluation makes known.
    Evaluated(Planned),
    /// Planned: its outputs as its plan has them, if it made one.
    Planned(Option<Planned>),
    /// Not planned, and why: its plan failed or was refused, or one it
    /// reads was not planned.
    Stopped(String),
}

impl Known {
    /// Whether its plan is over, made or not.
    fn settled(&self) -> bool {
        matches!(self, Known::Planned(_) | Known::Stopped(_))
    }
}

/// The tree's deployments, each as far as its plan is.
#[derive(Debug)]
pub(super) struct Board {
    /// Each deployment's name, as a reader names it, in the tree's order.
    names: Vec<String>,
    known: Mutex<Vec<Known>>,
    changed: Condvar,
}

impl Board {
    /// The deployments `names`, none evaluated yet.
    pub(super) fn new(names: Vec<String>) -> Arc<Board> {
        let known = names.iter().map(|_| Known::Pending).collect();
        Arc::new(Board {
            names,
            known: Mutex::new(known),
            changed: Condvar::new(),
        })
    }

    fn known(&self) -> MutexGuard<'_, Vec<Known>> {
        // A value set or read: a panic leaves the board whole.
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, i: usize, k: Known) {
        self.known()[i] = k;
        self.changed.notify_all();
    }

    /// Deployment `i` planned: its outputs as its plan has them.
    pub(super) fn planned(&self, i: usize, outputs: Option<Planned>) {
        self.post(i, Known::Planned(outputs));
    }

    /// Deployment `i` not planned, and why.
    pub(super) fn stopped(&self, i: usize, why: String) {
        self.post(i, Known::Stopped(why));
    }

    /// While this lives, a panic of the thread planning deployment `i`
    /// posts it not planned: those that read it wait on nothing that will
    /// not come.
    pub(super) fn unwinding(&self, i: usize) -> Unwinding<'_> {
        Unwinding { board: self, i }
    }

    /// The outputs of each deployment planned, as its plan has them, by
    /// name: what a run after the tree's reads.
    pub(super) fn outputs(&self) -> BTreeMap<String, Planned> {
        let known = self.known();
        self.names
            .iter()
            .zip(known.iter())
            .filter_map(|(n, k)| match k {
                Known::Planned(Some(p)) => Some((n.clone(), p.clone())),
                _ => None,
            })
            .collect()
    }

    /// The board once each deployment of `of` is as `ready` says.
    fn wait(&self, of: &[usize], ready: fn(&Known) -> bool) -> MutexGuard<'_, Vec<Known>> {
        let known = self.known();
        self.changed
            .wait_while(known, |k| !of.iter().all(|&j| ready(&k[j])))
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// A deployment's thread unwinding ([`Board::unwinding`]).
pub(super) struct Unwinding<'b> {
    board: &'b Board,
    i: usize,
}

impl Drop for Unwinding<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let name = &self.board.names[self.i];
            self.board
                .stopped(self.i, format!("{name}'s plan panicked"));
        }
    }
}

/// What a run of a deployment in the tree comes to once those it reads
/// are planned ([`Upstreams::verdict`]).
#[derive(Debug, PartialEq)]
pub(super) enum Verdict {
    /// What it read is what their plans make known: its plan stands.
    Stands,
    /// One it reads made known other than its evaluation did: plan it
    /// again, over the plans'.
    Again,
    /// One it reads was not planned (why): nor is it.
    NotPlanned(String),
}

/// A run of the tree's deployment `me`: those it reads on the board, and
/// what its evaluation read of them.
#[derive(Debug, Clone)]
pub(super) struct Upstreams {
    board: Arc<Board>,
    me: usize,
    of: Vec<usize>,
    read: Arc<Mutex<Option<BTreeMap<String, Planned>>>>,
}

impl Upstreams {
    /// A run of deployment `me` of `board`, which reads the deployments
    /// `of`.
    pub(super) fn new(board: Arc<Board>, me: usize, of: Vec<usize>) -> Upstreams {
        Upstreams {
            board,
            me,
            of,
            read: Arc::default(),
        }
    }

    /// What those it reads make known, once each has evaluated: what the
    /// run's evaluation reads, by name. An error once one is not planned.
    pub(super) fn evaluated(&self) -> Result<BTreeMap<String, Planned>> {
        let read = {
            let known = self.board.wait(&self.of, |k| !matches!(k, Known::Pending));
            self.outputs_of(&known)?
        };
        *self.read.lock().unwrap_or_else(|e| e.into_inner()) = Some(read.clone());
        Ok(read)
    }

    /// The outputs its evaluation makes known, for those that read it.
    pub(super) fn tell(&self, outputs: Planned) {
        self.board.post(self.me, Known::Evaluated(outputs));
    }

    /// What those it reads make known as their plans have it, once each
    /// has planned: what the run's report says of them. An error when one
    /// was not planned, or made known other than the run read (its
    /// [`Upstreams::verdict`] says which).
    pub(super) fn planned(&self) -> Result<BTreeMap<String, Planned>> {
        let (verdict, planned) = self.settled();
        match verdict {
            Verdict::Stands => Ok(planned),
            Verdict::Again => bail!("planned again: what it reads changed in their plans"),
            Verdict::NotPlanned(why) => bail!("not planned: {why}"),
        }
    }

    /// What the run comes to, once those it reads are planned.
    pub(super) fn verdict(&self) -> Verdict {
        self.settled().0
    }

    /// The verdict, and what their plans make known.
    fn settled(&self) -> (Verdict, BTreeMap<String, Planned>) {
        let planned = {
            let known = self.board.wait(&self.of, Known::settled);
            match self.outputs_of(&known) {
                Ok(p) => p,
                Err(e) => return (Verdict::NotPlanned(e.to_string()), BTreeMap::new()),
            }
        };
        let read = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let alike = |read: &BTreeMap<String, Planned>| {
            read.len() == planned.len()
                && read
                    .iter()
                    .zip(&planned)
                    .all(|((r, a), (p, b))| r == p && a.knows_alike(b))
        };
        // A run that never read them (it stopped before) stands as it is.
        match read.as_ref().is_none_or(alike) {
            true => (Verdict::Stands, planned),
            false => (Verdict::Again, planned),
        }
    }

    /// What those it reads make known on `known`, by name; the reason one
    /// is not planned.
    fn outputs_of(&self, known: &[Known]) -> Result<BTreeMap<String, Planned>> {
        let mut out = BTreeMap::new();
        for &j in &self.of {
            let p = match &known[j] {
                Known::Evaluated(p) | Known::Planned(Some(p)) => p,
                Known::Planned(None) | Known::Pending => continue,
                Known::Stopped(why) => bail!("{why}"),
            };
            out.insert(self.board.names[j].clone(), p.clone());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;
    use std::sync::mpsc;
    use std::time::Duration;

    /// `full`'s outputs: `cidr` known as `cidr`, `id` pending until
    /// `after` says.
    fn outputs(full: &str, cidr: &str, after: Option<usize>) -> Planned {
        let mut p = Planned {
            full: full.into(),
            outputs: Default::default(),
            values: BTreeMap::new(),
            after: after.map(|t| ("id".to_string(), t)).into_iter().collect(),
            world: Default::default(),
        };
        p.outputs
            .known
            .insert("cidr".into(), Value::Str(cidr.into()));
        p.outputs.pending.insert("id".into());
        p
    }

    /// Platform (0) and apps (1), which reads it.
    fn tree() -> (Upstreams, Upstreams) {
        let board = Board::new(vec!["platform".into(), "apps".into()]);
        (
            Upstreams::new(board.clone(), 0, Vec::new()),
            Upstreams::new(board, 1, vec![0]),
        )
    }

    /// Apps reads what platform's evaluation made known as soon as it
    /// has, and waits for platform's plan only to say it.
    #[test]
    fn a_reader_reads_an_evaluation_and_waits_for_its_plan() {
        let (platform, apps) = tree();
        let (tx, rx) = mpsc::channel();
        let reader = apps.clone();
        std::thread::spawn(move || {
            tx.send(reader.evaluated().unwrap()).unwrap();
            tx.send(reader.planned().unwrap()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        platform.tell(outputs("stacks.platform", "10.0.0.0/16", None));
        let read = rx.recv().unwrap();
        assert_eq!(
            read["platform"].outputs.known["cidr"],
            Value::Str("10.0.0.0/16".into())
        );
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        platform
            .board
            .planned(0, Some(outputs("stacks.platform", "10.0.0.0/16", Some(1))));
        assert_eq!(rx.recv().unwrap()["platform"].after["id"], 1);
        assert_eq!(apps.verdict(), Verdict::Stands);
    }

    /// A plan that makes known other than its evaluation did plans its
    /// reader again; one that only says when the rest is known does not.
    #[test]
    fn a_reader_of_what_a_plan_changed_is_planned_again() {
        let (platform, apps) = tree();
        platform.tell(outputs("stacks.platform", "10.0.0.0/16", None));
        apps.evaluated().unwrap();
        platform
            .board
            .planned(0, Some(outputs("stacks.platform", "10.9.0.0/16", Some(1))));
        assert_eq!(apps.verdict(), Verdict::Again);
        assert!(apps.planned().is_err());
        let again = Upstreams::new(platform.board.clone(), 1, vec![0]);
        assert_eq!(
            again.evaluated().unwrap()["platform"].outputs.known["cidr"],
            Value::Str("10.9.0.0/16".into())
        );
        assert_eq!(again.verdict(), Verdict::Stands);
    }

    /// A deployment whose thread panics is not planned: its reader does
    /// not wait for it.
    #[test]
    fn a_reader_of_one_whose_plan_panicked_is_not_planned() {
        let (platform, apps) = tree();
        let board = platform.board.clone();
        let panicked = std::thread::spawn(move || {
            let _unwinding = board.unwinding(0);
            panic!("in platform's plan");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(
            apps.verdict(),
            Verdict::NotPlanned("platform's plan panicked".into())
        );
    }

    /// A reader of a deployment not planned is not planned, for its
    /// reason, whether it had read its evaluation or not.
    #[test]
    fn a_reader_of_one_not_planned_is_not() {
        let (platform, apps) = tree();
        platform.tell(outputs("stacks.platform", "10.0.0.0/16", None));
        apps.evaluated().unwrap();
        platform
            .board
            .stopped(0, "stacks.platform[env=lab] refused".into());
        let why = Verdict::NotPlanned("stacks.platform[env=lab] refused".into());
        assert_eq!(apps.verdict(), why);
        let late = Upstreams::new(platform.board.clone(), 1, vec![0]);
        assert!(late.evaluated().is_err());
        assert_eq!(late.verdict(), why);
    }
}
