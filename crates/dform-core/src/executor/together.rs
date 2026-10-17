//! A tree's tick walked part by part, each deployment's on the thread that
//! planned it ([`super::run_tick`]'s `beside`): what the walks share.
//!
//! One window: at most `--parallel` Apply calls in flight, of every part.
//! One stop: after a part's call fails no part starts another, and once a
//! part's walk stops dform as if killed (chaos `stop-after`, a state that
//! could not be written) every walk stops so, its calls in flight left to
//! their write-ahead records. And what a reader's call waits on in another
//! part: a secret its upstream holds at an object the tick changes is read
//! where it is held, inside the reader's Apply (`provider::Held`), so the
//! call waits for the holder's call that changes that object.

use crate::address::Address;
use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex, MutexGuard};

/// The walks of a tree's tick ([`super::Beside`]): their window, their
/// stop, and what each makes and has made.
pub struct Together {
    parallel: usize,
    at: Mutex<Shared>,
    changed: Condvar,
}

/// What the walks of a tick share, as it is now.
#[derive(Default)]
struct Shared {
    /// The calls in flight, of every part, or answered at once and not
    /// taken yet.
    in_flight: usize,
    /// Each part's objects its calls change, once its walk has begun.
    made: Vec<Option<Made>>,
    /// The calls that answered, by part and address.
    done: BTreeSet<(usize, Address)>,
    /// The parts whose walks have ended.
    ended: BTreeSet<usize>,
    /// A part's call failed: no new call starts.
    failed: bool,
    /// The part whose walk stopped dform as if killed, first.
    stopped: Option<usize>,
    /// Counts every change of the above: what a walk waiting for one
    /// compares.
    generation: u64,
}

/// A part's deployment, as a reader of it names it, and the objects its
/// calls change: each by its type and remote id, and the call's address.
#[derive(Clone)]
pub(super) struct Made {
    pub(super) name: String,
    pub(super) objects: Vec<(String, String, Address)>,
}

impl Together {
    /// The walks of `parts` parts, at most `parallel` calls in flight
    /// together.
    pub fn new(parts: usize, parallel: usize) -> Together {
        Together {
            parallel: parallel.max(1),
            at: Mutex::new(Shared {
                made: (0..parts).map(|_| None).collect(),
                ..Shared::default()
            }),
            changed: Condvar::new(),
        }
    }

    fn at(&self) -> MutexGuard<'_, Shared> {
        // A value set or read: a panic leaves it whole.
        self.at.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `f` done to what the walks share, and those waiting told.
    fn change(&self, f: impl FnOnce(&mut Shared)) {
        let mut at = self.at();
        f(&mut at);
        at.generation += 1;
        self.changed.notify_all();
    }

    /// Where the walks are, to wait from ([`Together::wait`]).
    pub(super) fn generation(&self) -> u64 {
        self.at().generation
    }

    /// Once anything changed since `generation`.
    pub(super) fn wait(&self, generation: u64) {
        let at = self.at();
        drop(
            self.changed
                .wait_while(at, |a| a.generation == generation)
                .unwrap_or_else(|e| e.into_inner()),
        );
    }

    /// A call in flight, if the window has room for one.
    pub(super) fn take_slot(&self) -> bool {
        let mut at = self.at();
        if at.in_flight >= self.parallel {
            return false;
        }
        at.in_flight += 1;
        true
    }

    /// A call taken, or one that was not sent: its room in the window
    /// free.
    pub(super) fn free_slot(&self) {
        self.change(|a| a.in_flight = a.in_flight.saturating_sub(1));
    }

    /// Part `part`'s walk has begun, what its calls make `made`; once
    /// every part's has, what each of the others makes, by part.
    pub(super) fn begin(&self, part: usize, made: Made) -> Vec<(usize, Made)> {
        self.change(|a| a.made[part] = Some(made));
        let at = self
            .changed
            .wait_while(self.at(), |a| {
                (0..a.made.len()).any(|q| a.made[q].is_none() && !a.ended.contains(&q))
            })
            .unwrap_or_else(|e| e.into_inner());
        at.made
            .iter()
            .enumerate()
            .filter(|&(q, _)| q != part)
            .filter_map(|(q, m)| Some((q, m.clone()?)))
            .collect()
    }

    /// Part `part`'s call for `addr` answered.
    pub(super) fn done(&self, part: usize, addr: &Address) {
        self.change(|a| {
            a.done.insert((part, addr.clone()));
        });
    }

    /// Whether every call of `calls` (by part and address) answered; `None`
    /// once one never will: its part's walk ended without it.
    pub(super) fn answered(&self, calls: &[(usize, Address)]) -> Option<bool> {
        let at = self.at();
        let mut all = true;
        for c in calls {
            if at.done.contains(c) {
                continue;
            }
            if at.ended.contains(&c.0) {
                return None;
            }
            all = false;
        }
        Some(all)
    }

    /// A part's call failed: no new call starts.
    pub(super) fn fail(&self) {
        self.change(|a| a.failed = true);
    }

    pub(super) fn failed(&self) -> bool {
        self.at().failed
    }

    /// Part `part`'s walk stopped dform as if killed: every walk stops.
    pub(super) fn stop(&self, part: usize) {
        self.change(|a| {
            a.stopped.get_or_insert(part);
        });
    }

    /// The part whose walk stopped dform as if killed, if one did.
    pub fn stopped(&self) -> Option<usize> {
        self.at().stopped
    }

    /// Part `part`'s walk ended.
    pub(super) fn end(&self, part: usize) {
        self.change(|a| {
            a.ended.insert(part);
        });
    }
}
