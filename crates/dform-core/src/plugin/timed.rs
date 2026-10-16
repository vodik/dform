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
//! One thread serves several `Timed`s ([`Timed::another`]), each a front
//! of the backend with calls, answers and timeouts of its own: the
//! deployments of a tree planned at once, each on a thread of its own,
//! share a provider's process so. An answer goes to the front whose call
//! it is; the thread waits in the backend while a front that asked for an
//! answer has a call in flight, and takes every command sent meanwhile
//! before it waits again.
//!
//! The thread owns the backend, and a process backend owns its process:
//! dropping the last `Timed` of a thread stops the process first when a
//! call is still stuck (its [`Provider::stopper`]), which fails that call
//! and frees the thread, and then joins the thread, within [`JOIN`]. A
//! call that timed out answered `MaybeApplied` already, which is what a
//! provider stopped in the middle of it may have left; its late answer was
//! dropped anyway.

use super::backend::{Call, CallError, Provider, Reply, Session, Stop, Ticket, Within};
use super::pb;
use super::policy::{describe, show};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
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

/// What a front sends the thread, each command naming the front.
enum Cmd {
    /// A front, and where its answers and events go.
    Open(u64, Sender<Back>),
    /// A front dropped: what answers its calls in flight is dropped.
    Close(u64),
    /// A call, and its number while it is traced.
    Submit(u64, Ticket, Box<Call>, Option<u64>),
    /// Answer one call of the front.
    Next(u64),
    IsDead(Sender<bool>),
}

/// A call with no answer yet: when it times out, what it is, and its
/// `provider.call` span, which ends when the `Live` is dropped: when its
/// answer is taken ([`Live::finish`]), or when it is given up on.
struct Live {
    due: Instant,
    what: String,
    span: tracing::Span,
    traced: Option<Traced>,
}

/// The `provider.call` spans of the calls in flight, by number: what the
/// provider names a request it sends the host with (the call it is
/// answering, `backend::Within`), so the host's span of the request is
/// the call's child. A number, not the span's id: a request that comes
/// after its call ended (on a thread of the provider's own) finds none.
static TRACED: Mutex<BTreeMap<u64, tracing::Span>> = Mutex::new(BTreeMap::new());

/// The `provider.call` span of the call numbered `number`, while it is in
/// flight.
pub fn call_span(number: u64) -> Option<tracing::Span> {
    traced().get(&number).cloned()
}

fn traced() -> std::sync::MutexGuard<'static, BTreeMap<u64, tracing::Span>> {
    // An insert, a removal or a lookup: a panic leaves the map whole.
    TRACED.lock().unwrap_or_else(|e| e.into_inner())
}

/// A call's span, found by its number until dropped.
struct Traced(u64);

impl Traced {
    /// `span`'s number, when it is traced (a subscriber takes it).
    fn new(span: &tracing::Span) -> Option<Traced> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        if span.is_disabled() {
            return None;
        }
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        traced().insert(number, span.clone());
        Some(Traced(number))
    }
}

impl Drop for Traced {
    fn drop(&mut self) {
        traced().remove(&self.0);
    }
}

impl Live {
    /// `call`, submitted now to the provider `provider`, due at `due`.
    fn new(provider: &str, call: &Call, due: Instant) -> Live {
        let what = describe(call);
        let span = Live::span(provider, call.method(), &what);
        if call.session() != Session::NONE {
            span.record("session", call.session().0);
        }
        let traced = Traced::new(&span);
        Live {
            due,
            what,
            span,
            traced,
        }
    }

    /// Its number while it is traced.
    fn number(&self) -> Option<u64> {
        self.traced.as_ref().map(|t| t.0)
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

/// The thread a backend runs on, shared by its fronts: stopped once the
/// last is dropped.
struct Worker {
    cmds: Option<Sender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    /// Whether the backend said it is dead, as of its last answer.
    dead: Arc<AtomicBool>,
    /// The fronts that asked for an answer they have not taken yet: the
    /// thread is in a call, or will be.
    asking: AtomicUsize,
    /// Stops the backend's process while the thread is stuck in a call.
    stop: Mutex<Option<Stop>>,
    /// The next front's number.
    fronts: AtomicU64,
}

impl Worker {
    fn send(&self, c: Cmd) -> bool {
        self.cmds.as_ref().is_some_and(|tx| tx.send(c).is_ok())
    }

    /// A front of its own: its number, and where its answers come.
    fn open(&self) -> (u64, Receiver<Back>) {
        let front = self.fronts.fetch_add(1, Ordering::Relaxed);
        let (tx, answers) = mpsc::channel::<Back>();
        self.send(Cmd::Open(front, tx));
        (front, answers)
    }
}

/// A front of a backend's thread: its calls, each waited for at most the
/// timeout.
pub struct Timed {
    /// What the backend is, for messages.
    what: String,
    timeout: Duration,
    worker: Arc<Worker>,
    /// This front's number on the thread.
    front: u64,
    answers: Receiver<Back>,
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
    /// Whether this front asked the thread for an answer it has not
    /// taken yet.
    asked: bool,
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
        let dead = Arc::new(AtomicBool::new(false));
        let flag = dead.clone();
        let stop = backend.stopper();
        let thread = std::thread::Builder::new()
            .name("dform-provider".into())
            .spawn(move || serve(backend, rx, flag))?;
        let worker = Arc::new(Worker {
            cmds: Some(cmds),
            thread: Some(thread),
            dead,
            asking: AtomicUsize::new(0),
            stop: Mutex::new(stop),
            fronts: AtomicU64::new(0),
        });
        Ok(Timed::front(what, timeout, worker))
    }

    /// Another front of the same backend: calls of its own, answered on
    /// the same thread, whichever thread makes them.
    pub fn another(&self) -> Timed {
        Timed::front(self.what.clone(), self.timeout, self.worker.clone())
    }

    fn front(what: String, timeout: Duration, worker: Arc<Worker>) -> Timed {
        let (front, answers) = worker.open();
        Timed {
            what,
            timeout,
            worker,
            front,
            answers,
            next: 0,
            batch: Vec::new(),
            live: BTreeMap::new(),
            abandoned: BTreeSet::new(),
            behind: BTreeSet::new(),
            asked: false,
        }
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
            let number = self.live.get(&t).and_then(Live::number);
            self.worker
                .send(Cmd::Submit(self.front, t, Box::new(call), number));
        }
    }

    /// Ask the thread for an answer of this front, unless it was asked.
    fn ask(&mut self) {
        if !self.asked && self.worker.send(Cmd::Next(self.front)) {
            self.asked = true;
            self.worker.asking.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// An answer came: what this front asked for, or one the thread took
    /// for it while it answered another front.
    fn answered(&mut self) {
        if std::mem::take(&mut self.asked) {
            self.worker.asking.fetch_sub(1, Ordering::SeqCst);
        }
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
            self.ask();
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
        self.answered();
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

/// The thread: pass each front's submit on, and while a front that asked
/// for an answer has a call in flight, the backend's next answer to
/// whichever front's call it is; the commands sent meanwhile are taken
/// first.
fn serve(backend: Box<dyn Provider + Send>, rx: Receiver<Cmd>, dead: Arc<AtomicBool>) {
    let mut s = Serving {
        backend,
        dead,
        fronts: HashMap::new(),
        ours: HashMap::new(),
        asking: BTreeSet::new(),
    };
    loop {
        let c = match s.answerable() {
            true => match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            },
            false => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        match c {
            Some(c) => s.command(c),
            None => s.answer(),
        }
    }
}

/// What the thread serves: the backend, its fronts and their calls.
struct Serving {
    backend: Box<dyn Provider + Send>,
    dead: Arc<AtomicBool>,
    /// Where each front's answers and events go.
    fronts: HashMap<u64, Sender<Back>>,
    /// The calls in flight, by the backend's ticket: whose, by its own.
    ours: HashMap<Ticket, (u64, Ticket)>,
    /// The fronts that asked for an answer and have had none since.
    asking: BTreeSet<u64>,
}

impl Serving {
    /// Whether a front that asked for an answer has a call in flight.
    fn answerable(&self) -> bool {
        self.ours.values().any(|(f, _)| self.asking.contains(f))
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Open(front, tx) => {
                self.fronts.insert(front, tx);
            }
            Cmd::Close(front) => {
                self.fronts.remove(&front);
                self.asking.remove(&front);
            }
            Cmd::Submit(front, t, call, number) => {
                // The transport sends the call's number with it.
                let _in = Within::call(number);
                self.ours.insert(self.backend.submit(*call), (front, t));
            }
            Cmd::Next(front) => {
                self.asking.insert(front);
            }
            Cmd::IsDead(reply) => {
                let d = self.backend.is_dead();
                self.dead.store(d, Ordering::SeqCst);
                let _ = reply.send(d);
            }
        }
    }

    /// The backend's next answer, to the front whose call it is; the
    /// events of calls in flight meanwhile to theirs.
    fn answer(&mut self) {
        let (ours, fronts) = (&self.ours, &self.fronts);
        let (t, r) = self.backend.next_completed(&mut |t, e| {
            if let Some(&(front, t)) = ours.get(&t)
                && let Some(tx) = fronts.get(&front)
            {
                let _ = tx.send(Back::Event(t, e));
            }
        });
        self.dead.store(self.backend.is_dead(), Ordering::SeqCst);
        let Some((front, t)) = self.ours.remove(&t) else {
            return;
        };
        self.asking.remove(&front);
        if let Some(tx) = self.fronts.get(&front) {
            let _ = tx.send(Back::Answer(Box::new((t, r))));
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
        if self.worker.asking.load(Ordering::SeqCst) > 0 {
            return self.worker.dead.load(Ordering::SeqCst);
        }
        let (tx, rx) = mpsc::channel();
        if !self.worker.send(Cmd::IsDead(tx)) {
            return true;
        }
        rx.recv().unwrap_or(true)
    }
}

impl Drop for Timed {
    /// The front closed: the thread drops what answers its calls in
    /// flight. The last front's drop stops the thread ([`Worker`]).
    fn drop(&mut self) {
        self.worker.send(Cmd::Close(self.front));
    }
}

impl Drop for Worker {
    /// The backend is dropped on its thread (a process backend stops its
    /// process). A thread still in a call (one that timed out) is freed by
    /// stopping the backend's process first; then it is waited for, at
    /// most [`JOIN`]. An in-process backend stuck in a call cannot be
    /// stopped: its thread is left to finish when the call does.
    fn drop(&mut self) {
        self.cmds.take();
        let Some(w) = self.thread.take() else { return };
        if self.asking.load(Ordering::SeqCst) > 0 {
            let stop = self
                .stop
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            match stop {
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

    /// Two fronts of one backend, each waiting on a thread of its own:
    /// each takes its own call's answer, never the other's, though their
    /// tickets are alike; the backend lives until the last is dropped.
    #[test]
    fn each_front_takes_its_own_answers() {
        let a = timed(5_000);
        let b = a.another();
        let ask = |mut p: Timed, name: &'static str| {
            std::thread::spawn(move || {
                let t = p.submit(apply(name));
                let (got, r) = p.next_completed(&mut |_, _| {});
                assert_eq!(got, t);
                let Ok(Reply::Apply(r)) = r else {
                    panic!("{r:?}")
                };
                (r.remote, p)
            })
        };
        let (slow, fast) = (ask(a, "50"), ask(b, "0"));
        let (fast, b) = fast.join().unwrap();
        let (slow, a) = slow.join().unwrap();
        assert_eq!((slow.as_str(), fast.as_str()), ("50", "0"));
        drop(a);
        let mut b = b;
        let t = b.submit(apply("0"));
        assert_eq!(b.next_completed(&mut |_, _| {}).0, t);
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
