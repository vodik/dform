//! The host's pacing of a provider's requests: one window per account
//! and bucket (what the server throttles: the account the session of the
//! request's call reported at its Configure, else that session alone,
//! `HttpRequest::session`; and the limit it counts against, in the
//! provider's word: `HttpRequest::bucket`, none the default bucket), and
//! no declared rates. Two sessions on one account (a tree's deployments in
//! one process of a provider) share its windows and halve together. A
//! window is how many of a bucket's requests may be
//! in flight at once. It starts small and grows while the server answers
//! (by one for each answer until the first throttle, so it doubles each
//! round trip; then by one each window's worth of answers), and halves
//! on a throttle (a 429 or a 503) or a timeout: congestion control for
//! an API's calls (TCP's additive increase and multiplicative decrease;
//! Netflix's concurrency-limits, Envoy's adaptive concurrency, the AWS
//! SDK's adaptive retry mode). A `Retry-After` pauses the bucket that
//! long. The request that was throttled is answered to the provider as
//! the server answered it: its retry is the provider's, and waits its
//! turn here. Requests wait in the order they came; a wait is a
//! `ratelimit.wait` span. [`CEILING`] bounds a window whatever the
//! server says: a safety ceiling, not a rate. The account is never a
//! span's: a revealed secret may be one.

use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Session, Stop, Ticket};
use dform_core::plugin::host::{Class, Error, HttpResponse};
use dform_core::plugin::pb;
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The most requests of one bucket in flight at once.
pub const CEILING: usize = 64;

/// A bucket's window at first contact.
const FIRST: f64 = 2.0;

/// The longest pause a `Retry-After` is taken for.
const LONGEST_PAUSE: Duration = Duration::from_secs(300);

/// What the server throttles a request's call by: the account its
/// session reported, else the session alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Throttled {
    Account(String),
    Session(Session),
}

/// A bucket: what its requests are throttled by, and its name.
type Key = (Throttled, String);

/// The buckets, locked.
type Buckets<'a> = MutexGuard<'a, BTreeMap<Key, Bucket>>;

/// One provider's buckets, each account's apart.
pub struct Limiter {
    provider: String,
    /// The account each session's last Configure reported ([`Paced`]).
    accounts: Mutex<BTreeMap<Session, String>>,
    buckets: Mutex<BTreeMap<Key, Bucket>>,
    turn: Condvar,
}

/// One bucket's window, and its queue.
#[derive(Debug)]
struct Bucket {
    window: f64,
    /// Growing by one an answer: no throttle seen yet.
    starting: bool,
    in_flight: usize,
    /// The next request's place in the queue, and the place admitted next.
    next: u64,
    admitted: u64,
    /// No request is sent before this (a `Retry-After`).
    paused: Option<Instant>,
}

impl Bucket {
    fn new() -> Bucket {
        Bucket {
            window: FIRST,
            starting: true,
            in_flight: 0,
            next: 0,
            admitted: 0,
            paused: None,
        }
    }

    /// How many may be in flight now.
    fn size(&self) -> usize {
        (self.window as usize).clamp(1, CEILING)
    }

    /// A new request's place in the queue.
    fn queue(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    /// The requests queued before `place`.
    fn before(&self, place: u64) -> usize {
        (place - self.admitted) as usize
    }

    /// Until when it is paused, as of `now`.
    fn paused_at(&self, now: Instant) -> Option<Instant> {
        self.paused.filter(|&until| until > now)
    }

    /// Admit `place` at `now`, if it may be sent: its turn in the queue,
    /// a place in the window, the bucket not paused.
    fn admit(&mut self, place: u64, now: Instant) -> bool {
        let admits =
            self.paused_at(now).is_none() && place == self.admitted && self.in_flight < self.size();
        if admits {
            self.admitted += 1;
            self.in_flight += 1;
        }
        admits
    }

    /// The server answered `r`: the window learns from it.
    fn learn(&mut self, r: &Result<HttpResponse, Error>, now: Instant) {
        match r {
            Ok(resp) if matches!(resp.status, 429 | 503) => {
                self.shrink();
                if let Some(d) = resp.header("retry-after").and_then(retry_after) {
                    let until = now + d.min(LONGEST_PAUSE);
                    self.paused = Some(self.paused.map_or(until, |p| p.max(until)));
                }
            }
            Ok(_) => self.grow(),
            Err(e) if e.class == Class::MaybeApplied => self.shrink(),
            // Refused, unreachable, a bad request: nothing about the pace.
            Err(_) => {}
        }
    }

    fn grow(&mut self) {
        self.window += match self.starting {
            true => 1.0,
            false => 1.0 / self.window,
        };
        self.window = self.window.min(CEILING as f64);
    }

    fn shrink(&mut self) {
        self.starting = false;
        self.window = (self.window / 2.0).max(1.0);
    }
}

/// `Retry-After` in seconds (its HTTP-date form is not read: the window
/// halves all the same).
fn retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A request admitted to its bucket's window, until it is dropped.
pub struct Permit<'a> {
    limiter: &'a Limiter,
    bucket: Key,
}

impl Limiter {
    /// The buckets of the provider `provider` (its name in a span).
    pub fn new(provider: &str) -> Limiter {
        Limiter {
            provider: provider.to_string(),
            accounts: Mutex::new(BTreeMap::new()),
            buckets: Mutex::new(BTreeMap::new()),
            turn: Condvar::new(),
        }
    }

    fn buckets(&self) -> Buckets<'_> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn accounts(&self) -> MutexGuard<'_, BTreeMap<Session, String>> {
        self.accounts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `session` was configured, and reported `account` (none: it does
    /// not tell): its requests count against that account's windows.
    pub fn configured(&self, session: Session, account: Option<&str>) {
        let mut accounts = self.accounts();
        match account {
            Some(a) => accounts.insert(session, a.to_string()),
            None => accounts.remove(&session),
        };
    }

    /// The bucket `bucket` of what `session`'s requests are throttled by.
    fn key(&self, session: Session, bucket: Option<&str>) -> Key {
        let by = match self.accounts().get(&session) {
            Some(a) => Throttled::Account(a.clone()),
            None => Throttled::Session(session),
        };
        (by, bucket.unwrap_or_default().to_string())
    }

    /// Wait until a request of `session`'s `bucket` may be sent: its turn
    /// in the queue, a place in the window, the bucket not paused.
    pub fn acquire(&self, session: Session, bucket: Option<&str>) -> Permit<'_> {
        let name = self.key(session, bucket);
        let mut buckets = self.buckets();
        let b = buckets.entry(name.clone()).or_insert_with(Bucket::new);
        let place = b.queue();
        if !b.admit(place, Instant::now()) {
            buckets = self.wait(buckets, session, &name, place);
        }
        drop(buckets);
        // The next in the queue may fit too.
        self.turn.notify_all();
        Permit {
            limiter: self,
            bucket: name,
        }
    }

    /// Wait until `place` in the queue of the bucket `name` is admitted:
    /// a `ratelimit.wait` span, with the `session` of the request's call
    /// and the `bucket`, `queued` (the requests before it) and `window`
    /// (how many may be in flight), under the span the host's thread is
    /// in (`grpc::serve`).
    #[tracing::instrument(
        target = "dform",
        level = "trace",
        name = "ratelimit.wait",
        skip_all,
        fields(
            otel.name = %format_args!("{} waits on its window", self.provider),
            provider = self.provider.as_str(),
            session = session.0,
            bucket = name.1.as_str(),
            queued = buckets[name].before(place),
            window = buckets[name].size(),
        )
    )]
    fn wait<'a>(
        &'a self,
        mut buckets: Buckets<'a>,
        session: Session,
        name: &Key,
        place: u64,
    ) -> Buckets<'a> {
        loop {
            let now = Instant::now();
            let b = buckets.get_mut(name).expect("queued before it waits");
            if b.admit(place, now) {
                return buckets;
            }
            buckets = match b.paused_at(now) {
                Some(until) => {
                    self.turn
                        .wait_timeout(buckets, until - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.turn.wait(buckets).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    /// The window of `session`'s `bucket` now, and whether it is paused
    /// (tests).
    pub fn window(&self, session: Session, bucket: Option<&str>) -> (usize, bool) {
        let key = self.key(session, bucket);
        let buckets = self.buckets();
        match buckets.get(&key) {
            Some(b) => (b.size(), b.paused.is_some_and(|p| p > Instant::now())),
            None => (FIRST as usize, false),
        }
    }
}

/// A provider whose host paces its requests by `limiter`: each Configure
/// it answers tells the limiter the account its session reported.
pub struct Paced<P> {
    provider: P,
    limiter: Arc<Limiter>,
}

impl<P: Provider> Paced<P> {
    pub fn new(provider: P, limiter: Arc<Limiter>) -> Paced<P> {
        Paced { provider, limiter }
    }
}

impl<P: Provider> Provider for Paced<P> {
    fn submit(&mut self, call: Call) -> Ticket {
        self.provider.submit(call)
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        let (t, r) = self.provider.next_completed(events);
        if let Ok(Reply::Configure(c)) = &r {
            self.limiter
                .configured(Session(c.session), c.account.as_deref());
        }
        (t, r)
    }

    fn is_dead(&mut self) -> bool {
        self.provider.is_dead()
    }

    fn stopper(&self) -> Option<Stop> {
        self.provider.stopper()
    }
}

impl Permit<'_> {
    /// Its answer came, `r`: the window learns from it.
    pub fn answered(self, r: &Result<HttpResponse, Error>) {
        if let Some(b) = self.limiter.buckets().get_mut(&self.bucket) {
            b.learn(r, Instant::now());
        }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.limiter.buckets().get_mut(&self.bucket) {
            b.in_flight -= 1;
        }
        self.limiter.turn.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NONE: Session = Session::NONE;
    use std::sync::Arc;

    fn status(s: u16, headers: &[(&str, &str)]) -> Result<HttpResponse, Error> {
        Ok(HttpResponse {
            status: s,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        })
    }

    /// A bucket starts small, doubles each round trip until the first
    /// throttle, halves on it, then grows by one a window; a timeout
    /// halves it too; it never passes the ceiling nor goes under one.
    #[test]
    fn a_window_grows_on_answers_and_halves_on_a_throttle() {
        let l = Limiter::new("p");
        assert_eq!(l.window(NONE, None), (2, false));
        for _ in 0..6 {
            l.acquire(NONE, None).answered(&status(200, &[]));
        }
        assert_eq!(l.window(NONE, None).0, 8);
        l.acquire(NONE, None).answered(&status(429, &[]));
        assert_eq!(l.window(NONE, None).0, 4);
        for _ in 0..4 {
            l.acquire(NONE, None).answered(&status(200, &[]));
        }
        assert_eq!(
            l.window(NONE, None).0,
            4,
            "one more a window's worth: {:?}",
            l.window(NONE, None)
        );
        l.acquire(NONE, None)
            .answered(&Err(Error::maybe_applied("timed out")));
        assert_eq!(l.window(NONE, None).0, 2);
        for _ in 0..4 {
            l.acquire(NONE, None).answered(&status(503, &[]));
        }
        assert_eq!(l.window(NONE, None).0, 1);
        // Another bucket is its own.
        assert_eq!(
            l.window(NONE, Some("route53.change:example.com")),
            (2, false)
        );
        let m = Limiter::new("p");
        for _ in 0..500 {
            m.acquire(NONE, None).answered(&status(200, &[]));
        }
        assert_eq!(m.window(NONE, None).0, CEILING);
    }

    /// A `Retry-After` pauses its bucket that long, and only its bucket.
    #[test]
    fn retry_after_pauses_the_bucket() {
        let l = Limiter::new("p");
        l.acquire(NONE, Some("a"))
            .answered(&status(429, &[("Retry-After", "1")]));
        assert_eq!(l.window(NONE, Some("a")), (1, true));
        let t = Instant::now();
        drop(l.acquire(NONE, Some("b")));
        assert!(t.elapsed() < Duration::from_millis(500), "b is not paused");
        drop(l.acquire(NONE, Some("a")));
        assert!(
            t.elapsed() >= Duration::from_millis(900),
            "{:?}",
            t.elapsed()
        );
    }

    /// No more of a bucket's requests are in flight than its window, and
    /// they are admitted in the order they came.
    #[test]
    fn a_window_admits_in_order() {
        let l = Arc::new(Limiter::new("p"));
        let held = l.acquire(NONE, None);
        let _second = l.acquire(NONE, None);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for i in 0..3 {
            let (l, order) = (l.clone(), order.clone());
            waiting.push(std::thread::spawn(move || {
                let p = l.acquire(NONE, None);
                order.lock().unwrap().push(i);
                drop(p);
            }));
            // Each is queued before the next comes.
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(order.lock().unwrap().is_empty(), "the window is full");
        drop(held);
        for w in waiting {
            w.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }

    /// Sessions that reported one account share its windows: one's
    /// throttle halves the other's, one's `Retry-After` pauses the
    /// other's bucket; a session on another account, or that reports
    /// none, keeps its own.
    #[test]
    fn sessions_on_one_account_share_its_windows() {
        let l = Limiter::new("p");
        let (east, west, apart, untold) = (Session(1), Session(2), Session(3), Session(4));
        l.configured(east, Some("acct-1"));
        l.configured(west, Some("acct-1"));
        l.configured(apart, Some("acct-2"));
        for _ in 0..2 {
            l.acquire(east, None).answered(&status(200, &[]));
        }
        assert_eq!(l.window(west, None).0, 4, "west sees east's growth");
        l.acquire(west, None).answered(&status(429, &[]));
        assert_eq!(l.window(east, None).0, 2, "east halves with west");
        assert_eq!(l.window(apart, None), (2, false));
        assert_eq!(l.window(untold, None), (2, false));
        l.acquire(east, Some("a"))
            .answered(&status(429, &[("Retry-After", "1")]));
        assert_eq!(l.window(west, Some("a")), (1, true));
        // Configured again with none, a session is its own again.
        l.configured(west, None);
        assert_eq!(l.window(west, Some("a")), (2, false));
    }

    /// A provider's Configure answers, as its link takes them, tell its
    /// host's limiter each session's account (`Paced`).
    #[test]
    fn a_configure_answer_names_its_sessions_account() {
        /// Answers each Configure with its session's account.
        struct Accounts(Vec<(Ticket, Reply)>);
        impl Provider for Accounts {
            fn submit(&mut self, call: Call) -> Ticket {
                let Call::Configure(c) = call else {
                    unreachable!("Configure only")
                };
                let t = Ticket(self.0.len() as u64);
                let account = (c.session != 3).then(|| "acct-1".to_string());
                let r = pb::ConfigureResponse {
                    session: c.session,
                    account,
                    ..Default::default()
                };
                self.0.push((t, Reply::Configure(r)));
                t
            }
            fn next_completed(
                &mut self,
                _: &mut dyn FnMut(Ticket, pb::Event),
            ) -> (Ticket, Result<Reply, CallError>) {
                let (t, r) = self.0.remove(0);
                (t, Ok(r))
            }
        }
        let l = Arc::new(Limiter::new("p"));
        let mut p = Paced::new(Accounts(Vec::new()), l.clone());
        for session in 1..=3 {
            p.submit(Call::Configure(pb::ConfigureRequest {
                session,
                ..Default::default()
            }));
            let (_, answered) = p.next_completed(&mut |_, _| {});
            answered.expect("a Configure the fake answers");
        }
        l.acquire(Session(1), None).answered(&status(429, &[]));
        assert_eq!(l.window(Session(2), None).0, 1, "one account");
        assert_eq!(l.window(Session(3), None), (2, false), "none told");
    }

    /// Each session's buckets are its own: one session's throttle leaves
    /// another's window as it was.
    #[test]
    fn each_session_has_its_own_windows() {
        let l = Limiter::new("p");
        let (east, west) = (Session(1), Session(2));
        l.acquire(east, None).answered(&status(429, &[]));
        assert_eq!(l.window(east, None).0, 1);
        assert_eq!(l.window(west, None), (2, false));
        l.acquire(west, Some("a"))
            .answered(&status(429, &[("Retry-After", "1")]));
        let t = Instant::now();
        drop(l.acquire(east, Some("a")));
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "east's a is not paused"
        );
    }
}
