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
//! late answer comes (they may time out first, if it never does). An
//! event a call sends while it runs crosses as it comes, and does not
//! move its timeout: a call that keeps saying how it goes still has to
//! answer in time.
//!
//! The thread owns the backend, and a process backend owns its process:
//! dropping a `Timed` stops the process first when a call is still stuck
//! (its [`Provider::stopper`]), which fails that call and frees the
//! thread, and then joins the thread, within [`JOIN`]. A call that timed
//! out answered `MaybeApplied` already, which is what a provider stopped
//! in the middle of it may have left; its late answer was dropped anyway.

use super::backend::{Call, CallError, Provider, Reply, Session, Stop, Ticket};
use super::pb;
use super::policy::{describe, show};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type Answer = (Ticket, Result<Reply, CallError>);

/// How long dropping a [`Timed`] waits for its thread to finish (the
/// backend to drop: a process killed and reaped).
const JOIN: Duration = Duration::from_secs(5);

/// What the thread sends back: an event of a call in flight, or an
/// answer.
enum Back {
    Event(Ticket, pb::Event),
    Answer(Box<Answer>),
}

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

/// A call with no answer yet: when it times out, what it is, and its
/// `provider.call` span, which ends when the `Live` is dropped: when its
/// answer is taken ([`Live::finish`]), or when it is given up on.
struct Live {
    due: Instant,
    what: String,
    span: tracing::Span,
}

impl Live {
    /// `call`, submitted now to the provider `provider`, due at `due`.
    fn new(provider: &str, call: &Call, due: Instant) -> Live {
        let what = describe(call);
        let span = Live::span(provider, call.method(), &what);
        if call.session() != Session::NONE {
            span.record("session", call.session().0);
        }
        Live { due, what, span }
    }

    /// The span of a call of `kind` to `provider`, `what` its description
    /// (the kind, then the resource's address or the extern's name).
    fn span(provider: &str, kind: &str, what: &str) -> tracing::Span {
        let addr = what.strip_prefix(kind).unwrap_or(what).trim_start();
        // A provider not yet named by its handshake, by its executable's name.
        let provider = provider.rsplit('/').next().unwrap_or(provider);
        tracing::debug_span!(
            target: crate::timing::TARGET,
            "provider.call",
            otel.name = %format_args!("{provider} {kind}"),
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            provider,
            kind,
            addr = (!addr.is_empty()).then_some(addr),
            session = tracing::field::Empty,
        )
    }

    /// Its answer, `r`, is taken: its status recorded (and the session a
    /// Configure opened), its span ended.
    fn finish(self, r: &Result<Reply, CallError>) {
        let status = match r {
            Ok(_) => "ok",
            Err(_) => "error",
        };
        self.span.record("otel.status_code", status);
        if let Ok(Reply::Configure(c)) = r {
            self.span.record("session", c.session);
        }
    }
}

pub struct Timed {
    /// What the backend is, for messages.
    what: String,
    timeout: Duration,
    cmds: Option<Sender<Cmd>>,
    answers: Receiver<Back>,
    worker: Option<JoinHandle<()>>,
    next: u64,
    /// Submitted, not yet passed on.
    batch: Vec<(Ticket, Call)>,
    /// Calls with no answer yet.
    live: BTreeMap<Ticket, Live>,
    /// Calls that timed out; their answers are dropped.
    abandoned: BTreeSet<Ticket>,
    /// Calls submitted while one that timed out had no answer: they wait
    /// behind it, and their timeouts start again once it answers.
    behind: BTreeSet<Ticket>,
    /// `Next` requests the thread has not answered yet.
    asked: usize,
    /// Whether the backend said it is dead, as of its last answer.
    dead: Arc<AtomicBool>,
    /// Stops the backend's process while the thread is stuck in a call.
    stop: Option<Stop>,
}

impl Timed {
    /// `backend` on a thread of its own, each call waited for at most
    /// `timeout`; `Err` when the thread cannot start.
    pub fn new(
        what: String,
        backend: Box<dyn Provider + Send>,
        timeout: Duration,
    ) -> std::io::Result<Timed> {
        let (cmds, rx) = mpsc::channel::<Cmd>();
        let (tx, answers) = mpsc::channel::<Back>();
        let dead = Arc::new(AtomicBool::new(false));
        let flag = dead.clone();
        let stop = backend.stopper();
        let worker = std::thread::Builder::new()
            .name("dform-provider".into())
            .spawn(move || serve(backend, rx, tx, flag))?;
        Ok(Timed {
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
            stop,
        })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Name the backend in messages (once its handshake has named it).
    pub fn set_name(&mut self, what: &str) {
        self.what = what.to_string();
    }

    /// Pass the calls submitted so far on to the backend now, rather than
    /// when an answer is next asked for: calls to several providers then
    /// run at once. They reach the backend in the order submitted, as
    /// they would.
    pub fn flush(&mut self) {
        for (t, call) in std::mem::take(&mut self.batch) {
            self.send(Cmd::Submit(t, Box::new(call)));
        }
    }

    fn send(&self, c: Cmd) -> bool {
        self.cmds.as_ref().is_some_and(|tx| tx.send(c).is_ok())
    }
    /// [`Provider::next_completed`], waiting no later than `deadline`:
    /// `None` when it passes first (a call that waits to be sent again,
    /// `providers::Tick`), the call in flight still awaited.
    pub fn next_completed_by(
        &mut self,
        deadline: Option<Instant>,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> Option<Answer> {
        assert!(!self.live.is_empty(), "internal: no call in flight");
        self.flush();
        loop {
            if self.asked == 0 && self.send(Cmd::Next) {
                self.asked += 1;
            }
            let (first, due) = self.first_due();
            let until = deadline.map_or(due, |d| d.min(due));
            match self
                .answers
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(Back::Event(t, e)) => {
                    if self.live.contains_key(&t) {
                        events(t, e);
                    }
                }
                Ok(Back::Answer(answer)) => {
                    if let Some(a) = self.taken(*answer) {
                        return Some(a);
                    }
                }
                Err(RecvTimeoutError::Timeout) if until < due => return None,
                Err(RecvTimeoutError::Timeout) => return Some(self.past_timeout(first)),
                Err(RecvTimeoutError::Disconnected) => return Some(self.gone(first)),
            }
        }
    }

    /// The live call due first, and when.
    fn first_due(&self) -> (Ticket, Instant) {
        self.live
            .iter()
            .map(|(t, l)| (*t, l.due))
            .min_by_key(|&(t, due)| (due, t))
            .expect("a call is live")
    }

    /// An answer from the thread: the call's, its span ended; `None` for
    /// one that timed out already, dropped (the calls behind it have their
    /// whole timeout from now when it was the last).
    fn taken(&mut self, (t, r): Answer) -> Option<Answer> {
        self.asked -= 1;
        if self.abandoned.remove(&t) {
            if self.abandoned.is_empty() {
                let due = Instant::now() + self.timeout;
                for b in std::mem::take(&mut self.behind) {
                    if let Some(l) = self.live.get_mut(&b) {
                        l.due = due;
                    }
                }
            }
            return None;
        }
        if let Some(live) = self.live.remove(&t) {
            live.finish(&r);
        }
        self.behind.remove(&t);
        Some((t, r))
    }

    /// The call `first`, past its timeout: `MaybeApplied`, its late
    /// answer to be dropped.
    fn past_timeout(&mut self, first: Ticket) -> Answer {
        let live = self.live.remove(&first).expect("the first due is live");
        self.behind.remove(&first);
        self.abandoned.insert(first);
        let r = Err(CallError::MaybeApplied(format!(
            "the provider {} did not answer the {} {PAST} {} {ITS_TIMEOUT}",
            self.what,
            live.what,
            show(self.timeout)
        )));
        live.finish(&r);
        (first, r)
    }

    /// The call `first`, its provider gone.
    fn gone(&mut self, first: Ticket) -> Answer {
        let live = self.live.remove(&first).expect("the first due is live");
        let r = Err(CallError::Crashed(format!(
            "the provider {} is gone: its {} call has no answer",
            self.what, live.what
        )));
        live.finish(&r);
        (first, r)
    }
}

/// The thread: pass each submit on, answer each `Next` with the
/// backend's next answer.
fn serve(
    mut backend: Box<dyn Provider + Send>,
    rx: Receiver<Cmd>,
    tx: Sender<Back>,
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
                let (t, r) = backend.next_completed(&mut |t, e| {
                    if let Some(&t) = ours.get(&t) {
                        let _ = tx.send(Back::Event(t, e));
                    }
                });
                dead.store(backend.is_dead(), Ordering::SeqCst);
                let Some(t) = ours.remove(&t) else { continue };
                if tx.send(Back::Answer(Box::new((t, r)))).is_err() {
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
        let live = Live::new(&self.what, &call, Instant::now() + self.timeout);
        self.live.insert(t, live);
        if !self.abandoned.is_empty() {
            self.behind.insert(t);
        }
        self.batch.push((t, call));
        t
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        self.next_completed_by(None, events)
            .expect("with no deadline, an answer")
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
    /// process). A thread still in a call (one that timed out) is freed by
    /// stopping the backend's process first; then it is waited for, at
    /// most [`JOIN`]. An in-process backend stuck in a call cannot be
    /// stopped: its thread is left to finish when the call does.
    fn drop(&mut self) {
        self.cmds.take();
        let Some(w) = self.worker.take() else { return };
        if self.asked > 0 {
            match self.stop.take() {
                Some(stop) => stop(),
                None => return,
            }
        }
        let deadline = Instant::now() + JOIN;
        while !w.is_finished() {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = w.join();
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::{Handler, Progress, Session};
    use super::super::queue::{Order, Queue};
    use super::*;

    /// Answers each Apply after sleeping as many ms as its name says.
    struct Slow;

    impl Handler for Slow {
        fn handle(&self, _: Session, call: Call, _: Progress) -> Result<Reply, CallError> {
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
        .unwrap()
    }

    /// A call past its timeout answers `MaybeApplied`, naming the call and
    /// the timeout; its late answer is dropped, and the next call, which
    /// waited behind it, has its whole timeout from then (it ends at 650ms,
    /// past 600ms, its timeout from when it was submitted).
    #[test]
    fn a_call_past_its_timeout_answers_maybe_applied() {
        let mut p = timed(300);
        let slow = p.submit(apply("450"));
        let (t, r) = p.next_completed(&mut |_, _| {});
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
        let (t, r) = p.next_completed(&mut |_, _| {});
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
        assert_eq!(p.next_completed(&mut |_, _| {}).0, a);
        assert_eq!(p.next_completed(&mut |_, _| {}).0, b);
        assert!(!p.is_dead());
    }
}
