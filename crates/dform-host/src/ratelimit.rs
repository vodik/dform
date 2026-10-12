//! The host's pacing of a provider's requests: one window per bucket
//! (the limit a request counts against, in the provider's word:
//! `HttpRequest::bucket`; none is the provider's default bucket), and
//! no declared rates. A window is how many of a bucket's requests may be
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
//! server says: a safety ceiling, not a rate.

use dform_core::plugin::host::{Class, Error, HttpResponse};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// The most requests of one bucket in flight at once.
pub const CEILING: usize = 64;

/// A bucket's window at first contact.
const FIRST: f64 = 2.0;

/// The longest pause a `Retry-After` is taken for.
const LONGEST_PAUSE: Duration = Duration::from_secs(300);

/// One provider's buckets.
pub struct Limiter {
    provider: String,
    buckets: Mutex<BTreeMap<String, Bucket>>,
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
    bucket: String,
}

impl Limiter {
    /// The buckets of the provider `provider` (its name in a span).
    pub fn new(provider: &str) -> Limiter {
        Limiter {
            provider: provider.to_string(),
            buckets: Mutex::new(BTreeMap::new()),
            turn: Condvar::new(),
        }
    }

    fn buckets(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until a request of `bucket` may be sent: its turn in the
    /// queue, a place in the window, the bucket not paused.
    pub fn acquire(&self, bucket: Option<&str>) -> Permit<'_> {
        let name = bucket.unwrap_or_default().to_string();
        let mut buckets = self.buckets();
        let place = {
            let b = buckets.entry(name.clone()).or_insert_with(Bucket::new);
            b.next += 1;
            b.next - 1
        };
        let mut waiting = None;
        loop {
            let now = Instant::now();
            let b = buckets.get_mut(&name).expect("made above");
            let paused = b.paused.filter(|&until| until > now);
            if paused.is_none() && place == b.admitted && b.in_flight < b.size() {
                b.admitted += 1;
                b.in_flight += 1;
                break;
            }
            if waiting.is_none() {
                let queued = (place - b.admitted) as usize;
                waiting =
                    dform_core::timing::ratelimit_wait(&self.provider, &name, queued, b.size());
            }
            buckets = match paused {
                Some(until) => {
                    self.turn
                        .wait_timeout(buckets, until - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.turn.wait(buckets).unwrap_or_else(|e| e.into_inner()),
            };
        }
        drop(buckets);
        // The next in the queue may fit too.
        self.turn.notify_all();
        drop(waiting);
        Permit {
            limiter: self,
            bucket: name,
        }
    }

    /// The window of `bucket` now, and whether it is paused (tests).
    pub fn window(&self, bucket: Option<&str>) -> (usize, bool) {
        let buckets = self.buckets();
        match buckets.get(bucket.unwrap_or_default()) {
            Some(b) => (b.size(), b.paused.is_some_and(|p| p > Instant::now())),
            None => (FIRST as usize, false),
        }
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
        assert_eq!(l.window(None), (2, false));
        for _ in 0..6 {
            l.acquire(None).answered(&status(200, &[]));
        }
        assert_eq!(l.window(None).0, 8);
        l.acquire(None).answered(&status(429, &[]));
        assert_eq!(l.window(None).0, 4);
        for _ in 0..4 {
            l.acquire(None).answered(&status(200, &[]));
        }
        assert_eq!(
            l.window(None).0,
            4,
            "one more a window's worth: {:?}",
            l.window(None)
        );
        l.acquire(None)
            .answered(&Err(Error::maybe_applied("timed out")));
        assert_eq!(l.window(None).0, 2);
        for _ in 0..4 {
            l.acquire(None).answered(&status(503, &[]));
        }
        assert_eq!(l.window(None).0, 1);
        // Another bucket is its own.
        assert_eq!(l.window(Some("route53.change:example.com")), (2, false));
        let m = Limiter::new("p");
        for _ in 0..500 {
            m.acquire(None).answered(&status(200, &[]));
        }
        assert_eq!(m.window(None).0, CEILING);
    }

    /// A `Retry-After` pauses its bucket that long, and only its bucket.
    #[test]
    fn retry_after_pauses_the_bucket() {
        let l = Limiter::new("p");
        l.acquire(Some("a"))
            .answered(&status(429, &[("Retry-After", "1")]));
        assert_eq!(l.window(Some("a")), (1, true));
        let t = Instant::now();
        drop(l.acquire(Some("b")));
        assert!(t.elapsed() < Duration::from_millis(500), "b is not paused");
        drop(l.acquire(Some("a")));
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
        let held = l.acquire(None);
        let _second = l.acquire(None);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for i in 0..3 {
            let (l, order) = (l.clone(), order.clone());
            waiting.push(std::thread::spawn(move || {
                let p = l.acquire(None);
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
}
