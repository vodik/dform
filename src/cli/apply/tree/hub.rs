//! Where a tree's apply meets its deployments ([`Hub`]): each is planned
//! on a thread of its own (`matrix::Tree`), and once planned takes its
//! seat there, saying where its thread takes the tree's requests
//! ([`Seat::take`]); the tree's thread waits for every seat
//! ([`Hub::seated`]), or for the first deployment that leaves without one.

use super::member::Job;
use crate::report;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// The seats of a tree's apply, one per deployment, in apply order.
pub(in crate::cli) struct Hub {
    seats: Mutex<Seats>,
    changed: Condvar,
}

/// Each seat as it is, and whether the tree still takes seats.
struct Seats {
    each: Vec<State>,
    closed: bool,
}

/// A deployment's seat.
enum State {
    /// Still planning.
    Planning,
    /// Planned, its thread taking the tree's requests.
    Taken(Seated),
    /// Its thread left without one: its plan failed, or it was not
    /// planned.
    Left,
}

/// What a deployment says as it takes its seat: where its thread takes
/// the tree's requests, and what the tree's thread says of it.
pub(super) struct Seated {
    pub(super) jobs: Sender<Job>,
    /// Its full name, as its state is stored and its readers' reports name it.
    pub(super) stored: String,
    /// How its plan text is painted.
    pub(super) style: report::Style,
    /// Chaos `stop-after`, its apply's.
    pub(super) stop_after: Option<usize>,
}

/// A deployment's seat at the tree's apply (`Cli::seat`).
#[derive(Clone)]
pub(in crate::cli) struct Seat {
    hub: Arc<Hub>,
    i: usize,
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Seat({})", self.i)
    }
}

impl Hub {
    /// The seats of `n` deployments, none taken.
    pub(super) fn new(n: usize) -> Arc<Hub> {
        Arc::new(Hub {
            seats: Mutex::new(Seats {
                each: (0..n).map(|_| State::Planning).collect(),
                closed: false,
            }),
            changed: Condvar::new(),
        })
    }

    fn seats(&self) -> MutexGuard<'_, Seats> {
        // A value set or taken: a panic leaves it whole.
        self.seats.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, i: usize, s: State) {
        let mut seats = self.seats();
        if !matches!(seats.each[i], State::Left) {
            seats.each[i] = s;
        }
        self.changed.notify_all();
    }

    /// Deployment `i`'s seat.
    pub(super) fn seat(self: &Arc<Hub>, i: usize) -> Seat {
        Seat {
            hub: self.clone(),
            i,
        }
    }

    /// Once every deployment has taken its seat, what each said, in
    /// order; `None` once one left without one (its plan failed: the tree
    /// is not applied). The seats are the tree's thread's then.
    pub(super) fn seated(&self) -> Option<Vec<Seated>> {
        let mut seats = self
            .changed
            .wait_while(self.seats(), |s| {
                s.each.iter().any(|e| matches!(e, State::Planning))
            })
            .unwrap_or_else(|e| e.into_inner());
        if seats.each.iter().any(|e| matches!(e, State::Left)) {
            return None;
        }
        let taken = std::mem::take(&mut seats.each);
        seats.each = taken.iter().map(|_| State::Left).collect();
        Some(
            taken
                .into_iter()
                .filter_map(|e| match e {
                    State::Taken(s) => Some(s),
                    _ => None,
                })
                .collect(),
        )
    }

    /// No seat is taken any more, and those taken are let go: their
    /// threads end.
    pub(super) fn close(&self) {
        let mut seats = self.seats();
        seats.closed = true;
        for e in seats.each.iter_mut() {
            *e = State::Left;
        }
        self.changed.notify_all();
    }
}

impl Seat {
    /// The deployment planned takes its seat (`seated`): `false` once the
    /// tree takes none, its apply over before it began.
    pub(super) fn take(&self, seated: Seated) -> bool {
        if self.hub.seats().closed {
            return false;
        }
        self.hub.set(self.i, State::Taken(seated));
        true
    }

    /// While this lives the deployment's thread is planning: once it is
    /// dropped (its run over, or unwinding) a seat not taken is left.
    pub(in crate::cli) fn leaving(&self) -> Leaving<'_> {
        Leaving(self)
    }
}

/// A deployment's thread, planning ([`Seat::leaving`]).
pub(in crate::cli) struct Leaving<'s>(&'s Seat);

impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        let mut seats = self.0.hub.seats();
        if let State::Planning = seats.each[self.0.i] {
            seats.each[self.0.i] = State::Left;
        }
        self.0.hub.changed.notify_all();
    }
}
