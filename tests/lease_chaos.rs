//! The lease under faults (R-135), on the memory store with a fault
//! injected into its writes of the lease object: a renewal that fails to
//! reach the store is tried again and the lease kept; one whose answer was
//! lost after it landed is still this holder's; a takeover stops the
//! renewer and refuses the next write; a renewer that died is the lost
//! lease at the next use; a panic releases; a release that fails says so.

use anyhow::{Result, bail};
use dform::state::State;
use dform::store::{
    Cond, Deployment, LOCK, LeaseRecord, LeaseTimes, MemoryStore, Object, STATE, Store,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// What the next writes of the lease object meet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The write does not reach the store (a timeout, a 503).
    Fails,
    /// The write lands, and its answer is lost.
    LandsUnanswered,
    /// The store panics (a bug in a client library).
    Panics,
    /// The write takes 2s, and lands.
    Slow,
}

/// The memory store, its next `n` writes of the lease object that renew or
/// release it meeting a fault.
struct Chaos {
    objects: MemoryStore,
    fault: std::sync::Mutex<Option<(Fault, usize)>>,
    /// The writes of the lease object that met a fault.
    faulted: AtomicUsize,
}

impl Chaos {
    fn new() -> Arc<Chaos> {
        Arc::new(Chaos {
            objects: MemoryStore::new(),
            fault: std::sync::Mutex::new(None),
            faulted: AtomicUsize::new(0),
        })
    }

    fn inject(&self, f: Fault, n: usize) {
        *self.fault.lock().unwrap() = Some((f, n));
    }

    fn record(&self) -> LeaseRecord {
        serde_json::from_slice(&self.objects.get(LOCK).unwrap().unwrap().bytes).unwrap()
    }
}

impl Store for Chaos {
    fn locate(&self, key: &str) -> String {
        self.objects.locate(key)
    }

    fn get(&self, key: &str) -> Result<Option<Object>> {
        self.objects.get(key)
    }

    fn put(&self, key: &str, bytes: &[u8], cond: &Cond) -> Result<Option<String>> {
        // A renewal or a release: a conditional write over the lease the
        // holder last wrote (a taking is `IfAbsent`, or over an expired one).
        let fault = match key == LOCK && matches!(cond, Cond::IfMatch(_)) {
            true => match self.fault.lock().unwrap().as_mut() {
                Some((fault, n)) if *n > 0 => {
                    *n -= 1;
                    Some(*fault)
                }
                _ => None,
            },
            false => None,
        };
        let Some(fault) = fault else {
            return self.objects.put(key, bytes, cond);
        };
        self.faulted.fetch_add(1, Ordering::SeqCst);
        match fault {
            Fault::Fails => bail!("PUT {key}: timed out"),
            Fault::LandsUnanswered => {
                self.objects.put(key, bytes, cond)?;
                bail!("PUT {key}: timed out")
            }
            Fault::Panics => panic!("the store's client panicked"),
            Fault::Slow => {
                std::thread::sleep(Duration::from_secs(2));
                self.objects.put(key, bytes, cond)
            }
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.objects.list(prefix)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.objects.delete(key)
    }
}

/// A lease of `ms`, renewed every quarter of it.
fn deployment(store: &Arc<Chaos>, ms: u64) -> Deployment {
    let times = LeaseTimes {
        duration: Duration::from_millis(ms),
        renewal: Duration::from_millis(ms / 4),
    };
    Deployment::new(store.clone(), "app", times)
}

/// Waits until `f`, at most 10s.
fn until(what: &str, f: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_renewal_that_fails_is_tried_again_and_the_lease_kept() {
    let store = Chaos::new();
    let a = deployment(&store, 400);
    a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.inject(Fault::Fails, 2);
    until("two renewals failed", || {
        store.faulted.load(Ordering::SeqCst) == 2
    });
    // Past the lease's whole life since the failures began: renewed since.
    std::thread::sleep(Duration::from_millis(500));
    g.check().unwrap();
    let b = deployment(&store, 400);
    b.load_state().unwrap();
    assert!(b.lock().is_err(), "the lease lapsed: B took it");
    a.save_state(&State::default()).unwrap();
    g.release().unwrap();
    assert_eq!(store.record().holder, "");
}

#[test]
fn a_renewal_whose_answer_was_lost_is_still_this_holders() {
    let store = Chaos::new();
    let a = deployment(&store, 400);
    a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.inject(Fault::LandsUnanswered, 1);
    until("a renewal landed unanswered", || {
        store.faulted.load(Ordering::SeqCst) == 1
    });
    // The next renewal is refused over the ETag this holder last wrote,
    // and finds the lease still names it.
    std::thread::sleep(Duration::from_millis(500));
    g.check().unwrap();
    a.save_state(&State::default()).unwrap();
    g.release().unwrap();
    assert_eq!(store.record().holder, "");
}

/// The renewer holds no lock across the store's call: a state write
/// beside a renewal that hangs goes through at once.
#[test]
fn a_hanging_renewal_does_not_hold_up_a_state_write() {
    let store = Chaos::new();
    let a = deployment(&store, 400);
    a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.inject(Fault::Slow, 1);
    until("a renewal hangs", || {
        store.faulted.load(Ordering::SeqCst) == 1
    });
    let start = std::time::Instant::now();
    a.save_state(&State::default()).unwrap();
    let took = start.elapsed();
    assert!(took < Duration::from_millis(1000), "{took:?}");
    g.release().unwrap();
    assert_eq!(store.record().holder, "");
}

#[test]
fn a_takeover_stops_the_renewer_and_refuses_the_next_write() {
    let store = Chaos::new();
    let a = deployment(&store, 400);
    let st = a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.break_lease(LOCK, "app").unwrap();
    let b = deployment(&store, 60_000);
    b.load_state().unwrap();
    let gb = b.lock().unwrap();
    until("A's renewer found the lease B's", || g.check().is_err());
    let e = g.check().unwrap_err();
    assert!(format!("{e:#}").contains("holds it now (fence 2)"), "{e:#}");
    let e = a.save_state(&st).unwrap_err();
    assert!(format!("{e:#}").contains("refused by fencing"), "{e:#}");
    // A's release leaves B's lease alone.
    g.release().unwrap();
    assert_eq!(store.record().fence, 2);
    assert!(!store.record().holder.is_empty());
    gb.release().unwrap();
    assert_eq!(store.record().holder, "");
}

#[test]
fn a_renewer_that_died_is_the_lost_lease_at_the_next_use() {
    let store = Chaos::new();
    let a = deployment(&store, 400);
    a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.inject(Fault::Panics, 1);
    until("the renewer panicked", || {
        store.faulted.load(Ordering::SeqCst) == 1
    });
    until("the guard sees it", || g.check().is_err());
    let e = g.check().unwrap_err();
    assert!(
        format!("{e:#}").contains("is no longer renewed: its renewer stopped"),
        "{e:#}"
    );
    let e = a.save_state(&State::default()).unwrap_err();
    assert!(format!("{e:#}").contains("refused by fencing"), "{e:#}");
    // Still this holder's: released.
    g.release().unwrap();
    assert_eq!(store.record().holder, "");
}

#[test]
fn a_panic_while_holding_the_lock_releases_it() {
    let store = Chaos::new();
    let a = deployment(&store, 60_000);
    a.load_state().unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = a.lock().unwrap();
        a.save_state(&State::default()).unwrap();
        panic!("mid-tick");
    }));
    assert!(r.is_err());
    assert_eq!(store.record().holder, "");
    let b = deployment(&store, 60_000);
    b.load_state().unwrap();
    b.lock().unwrap().release().unwrap();
}

#[test]
fn a_release_that_fails_says_so() {
    let store = Chaos::new();
    let a = deployment(&store, 60_000);
    a.load_state().unwrap();
    let g = a.lock().unwrap();
    store.inject(Fault::Fails, 1);
    let e = g.release().unwrap_err();
    let e = format!("{e:#}");
    assert!(
        e.contains("stack app: release the lease memory:state.lock; it is held until it expires"),
        "{e}"
    );
    assert!(!store.record().holder.is_empty());
    // The state was never touched by the faults.
    assert!(store.get(STATE).unwrap().is_none());
}
