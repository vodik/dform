//! Every call to a provider has a timeout (R-81, [`super::policy`]),
//! whatever its backend: [`Timed`] runs the backend on a thread of its own
//! and waits for each answer at most the policy's timeout. A call with no
//! answer by then answers `MaybeApplied`; its late answer, when it comes,
//! is dropped.
//!
//! The backend sees exactly the calls, in the order, it would see
//! unwrapped: a submit is passed on when the next answer is asked for, all
//! of them before one `next_completed`, so a backend on a simulated clock
//! (`queue::Order::Clock`) or a seeded one answers as it did. The thread
//! waits in the backend's `next_completed` for one answer at a time: a
//! call submitted while it waits reaches the backend once that answer is
//! in, and while a call that timed out is still unanswered, the calls
//! after it wait behind it: each one's timeout starts again when that
//! late answer comes (they may time out first, if it never does).

use super::backend::{Call, CallError, Provider, Reply, Ticket};
use super::policy::{describe, show};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Answer = (Ticket, Result<Reply, CallError>);

/// What the message of a call dform took as timed out says, and nothing
/// else's ([`timed_out`]).
const PAST: &str = "call within";
const ITS_TIMEOUT: &str = "(its timeout); the call may have taken effect";

/// Whether `e` is a call [`Timed`] took as timed out: no answer came
/// within its timeout (not the provider saying it timed out).
pub fn timed_out(e: &CallError) -> bool {
    matches!(e, CallError::MaybeApplied(m) if m.contains(PAST) && m.contains(ITS_TIMEOUT))
}

enum Cmd {
    Submit(Ticket, Box<Call>),
    /// Answer one call.
    Next,
    IsDead(Sender<bool>),
}

pub struct Timed {
    /// What the backend is, for messages.
    what: String,
    timeout: Duration,
    cmds: Option<Sender<Cmd>>,
    answers: Receiver<Answer>,
    worker: Option<JoinHandle<()>>,
    next: u64,
    /// Submitted, not yet passed on.
    batch: Vec<(Ticket, Call)>,
    /// Calls with no answer yet: when each times out, what it is, and
    /// when it was submitted.
    live: BTreeMap<Ticket, (Instant, String, Instant)>,
    /// Calls that timed out; their answers are dropped.
    abandoned: BTreeSet<Ticket>,
    /// Calls submitted while one that timed out had no answer: they wait
    /// behind it, and their timeouts start again once it answers.
    behind: BTreeSet<Ticket>,
    /// `Next` requests the thread has not answered yet.
    asked: usize,
    /// Whether the backend said it is dead, as of its last answer.
    dead: Arc<AtomicBool>,
}

impl Timed {
    /// `backend` on a thread of its own, each call waited for at most
    /// `timeout`.
    pub fn new(what: String, backend: Box<dyn Provider + Send>, timeout: Duration) -> Timed {
        let (cmds, rx) = mpsc::channel::<Cmd>();
        let (tx, answers) = mpsc::channel::<Answer>();
        let dead = Arc::new(AtomicBool::new(false));
        let flag = dead.clone();
        let worker = std::thread::Builder::new()
            .name("dform-provider".into())
            .spawn(move || serve(backend, rx, tx, flag))
            .expect("start the provider call thread");
        Timed {
            what,
            timeout,
            cmds: Some(cmds),
            answers,
            worker: Some(worker),
            next: 0,
            batch: Vec::new(),
            live: BTreeMap::new(),
            abandoned: BTreeSet::new(),
            behind: BTreeSet::new(),
            asked: 0,
            dead,
        }
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Name the backend in messages (once its handshake has named it).
    pub fn set_name(&mut self, what: &str) {
        self.what = what.to_string();
    }

    fn send(&self, c: Cmd) -> bool {
        self.cmds.as_ref().is_some_and(|tx| tx.send(c).is_ok())
    }
}

/// The thread: pass each submit on, answer each `Next` with the
/// backend's next answer.
fn serve(
    mut backend: Box<dyn Provider + Send>,
    rx: Receiver<Cmd>,
    tx: Sender<Answer>,
    dead: Arc<AtomicBool>,
) {
    let mut ours: HashMap<Ticket, Ticket> = HashMap::new();
    for c in rx {
        match c {
            Cmd::Submit(t, call) => {
                ours.insert(backend.submit(*call), t);
            }
            Cmd::Next if ours.is_empty() => {}
            Cmd::Next => {
                let (t, r) = backend.next_completed();
                dead.store(backend.is_dead(), Ordering::SeqCst);
                let Some(t) = ours.remove(&t) else { continue };
                if tx.send((t, r)).is_err() {
                    return;
                }
            }
            Cmd::IsDead(reply) => {
                let d = backend.is_dead();
                dead.store(d, Ordering::SeqCst);
                let _ = reply.send(d);
            }
        }
    }
}

impl Provider for Timed {
    fn submit(&mut self, call: Call) -> Ticket {
        let t = Ticket(self.next);
        self.next += 1;
        let now = Instant::now();
        self.live
            .insert(t, (now + self.timeout, describe(&call), now));
        if !self.abandoned.is_empty() {
            self.behind.insert(t);
        }
        self.batch.push((t, call));
        t
    }

    fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>) {
        assert!(!self.live.is_empty(), "internal: no call in flight");
        for (t, call) in std::mem::take(&mut self.batch) {
            self.send(Cmd::Submit(t, Box::new(call)));
        }
        loop {
            if self.asked == 0 && self.send(Cmd::Next) {
                self.asked += 1;
            }
            let (&first, (due, _, _)) = self
                .live
                .iter()
                .min_by_key(|(t, (due, _, _))| (*due, **t))
                .expect("a call is live");
            let wait = due.saturating_duration_since(Instant::now());
            match self.answers.recv_timeout(wait) {
                Ok((t, r)) => {
                    self.asked -= 1;
                    if self.abandoned.remove(&t) {
                        if self.abandoned.is_empty() {
                            let due = Instant::now() + self.timeout;
                            for b in std::mem::take(&mut self.behind) {
                                if let Some(l) = self.live.get_mut(&b) {
                                    l.0 = due;
                                }
                            }
                        }
                        continue;
                    }
                    if let Some((_, what, from)) = self.live.remove(&t) {
                        crate::timing::line(
                            &format!("provider {}: {what}", self.what),
                            from.elapsed(),
                        );
                    }
                    self.behind.remove(&t);
                    return (t, r);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let (_, what, _) = self.live.remove(&first).expect("live");
                    self.behind.remove(&first);
                    self.abandoned.insert(first);
                    return (
                        first,
                        Err(CallError::MaybeApplied(format!(
                            "the provider {} did not answer the {what} {PAST} {} {ITS_TIMEOUT}",
                            self.what,
                            show(self.timeout)
                        ))),
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let (_, what, _) = self.live.remove(&first).expect("live");
                    return (
                        first,
                        Err(CallError::Crashed(format!(
                            "the provider {} is gone: its {what} call has no answer",
                            self.what
                        ))),
                    );
                }
            }
        }
    }

    fn is_dead(&mut self) -> bool {
        if self.asked > 0 {
            return self.dead.load(Ordering::SeqCst);
        }
        let (tx, rx) = mpsc::channel();
        if !self.send(Cmd::IsDead(tx)) {
            return true;
        }
        rx.recv().unwrap_or(true)
    }
}

impl Drop for Timed {
    /// The backend is dropped on its thread (a process backend stops its
    /// process); waited for unless it is still in a call that timed out.
    fn drop(&mut self) {
        self.cmds.take();
        if let Some(w) = self.worker.take()
            && self.asked == 0
        {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::Handler;
    use super::super::pb;
    use super::super::queue::{Order, Queue};
    use super::*;

    /// Answers each Apply after sleeping as many ms as its name says.
    struct Slow;

    impl Handler for Slow {
        fn handle(&self, call: Call) -> Result<Reply, CallError> {
            let Call::Apply(r) = call else {
                return Err(CallError::Refused("apply only".into()));
            };
            std::thread::sleep(Duration::from_millis(r.name.parse().unwrap_or(0)));
            Ok(Reply::Apply(pb::ApplyResponse {
                remote: r.name,
                ..Default::default()
            }))
        }
    }

    fn apply(name: &str) -> Call {
        Call::Apply(pb::ApplyRequest {
            r#type: "net.vpc".into(),
            name: name.into(),
            ..Default::default()
        })
    }

    fn timed(timeout_ms: u64) -> Timed {
        let q = Queue::new(Slow, Order::Clock, false);
        Timed::new(
            "slow".into(),
            Box::new(q),
            Duration::from_millis(timeout_ms),
        )
    }

    /// A call past its timeout answers `MaybeApplied`, naming the call and
    /// the timeout; its late answer is dropped, and the next call, which
    /// waited behind it, has its whole timeout from then (it ends at 650ms,
    /// past 600ms, its timeout from when it was submitted).
    #[test]
    fn a_call_past_its_timeout_answers_maybe_applied() {
        let mut p = timed(300);
        let slow = p.submit(apply("450"));
        let (t, r) = p.next_completed();
        assert_eq!(t, slow);
        let Err(CallError::MaybeApplied(m)) = r else {
            panic!("{r:?}")
        };
        assert_eq!(
            m,
            "the provider slow did not answer the Apply net.vpc 450 call within 300ms \
             (its timeout); the call may have taken effect"
        );
        assert!(timed_out(&CallError::MaybeApplied(m)));
        assert!(!timed_out(&CallError::MaybeApplied("timed out".into())));
        let fast = p.submit(apply("200"));
        let (t, r) = p.next_completed();
        assert_eq!(t, fast);
        assert!(r.is_ok(), "{r:?}");
    }

    /// Within the timeout the backend answers as it would unwrapped: the
    /// clock's order.
    #[test]
    fn within_the_timeout_the_backend_answers_as_it_would() {
        let mut p = timed(5_000);
        let a = p.submit(apply("20"));
        let b = p.submit(apply("0"));
        assert_eq!(p.next_completed().0, a);
        assert_eq!(p.next_completed().0, b);
        assert!(!p.is_dead());
    }
}
