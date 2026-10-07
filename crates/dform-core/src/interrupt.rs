//! Ctrl-C and SIGTERM (R-137): a request to stop, never an exit from
//! where the signal lands. The handler only records the signal (an
//! atomic store: async-signal-safe); the executor makes no new Apply call
//! once one is recorded, the calls in flight are awaited (each bounded by
//! its link's timeout) and their answers logged, and the run unwinds to
//! its outcome: the lock released, the audit log's `apply_end` written,
//! the providers' stdin closed, every destructor run; `main` exits
//! 128 + the signal's number last. A second signal while stopping
//! restores the disposition dform found and raises it again: it ends the
//! process at once (what a call that hangs past the first is for).
//!
//! [`install`] is called once, by `main`; its guard restores the
//! dispositions dform found when it drops. A signal dform found ignored
//! (`nohup`, a job started in the background of a non-interactive shell)
//! stays ignored.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// The signal recorded first; 0 while none is.
static SIGNALLED: AtomicI32 = AtomicI32::new(0);

/// The dispositions dform found, by signal: what a second signal, and the
/// guard's drop, restore.
static PREVIOUS: [OnceLock<libc::sigaction>; 2] = [OnceLock::new(), OnceLock::new()];

/// The signals handled, in [`PREVIOUS`]'s order.
const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

extern "C" fn on_signal(sig: libc::c_int) {
    if SIGNALLED
        .compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return;
    }
    // The second: the disposition dform found, and the signal again.
    // SAFETY: sigaction and raise are async-signal-safe; the previous
    // action was stored before this handler was installed.
    unsafe {
        if let Some(i) = SIGNALS.iter().position(|s| *s == sig)
            && let Some(prev) = PREVIOUS[i].get()
        {
            libc::sigaction(sig, prev, std::ptr::null_mut());
        } else {
            libc::signal(sig, libc::SIG_DFL);
        }
        libc::raise(sig);
    }
}

/// The handler, installed for SIGINT and SIGTERM until it drops.
#[must_use = "dropping the guard restores the dispositions dform found"]
pub struct Guard {
    installed: Vec<usize>,
}

/// Install the handler for SIGINT and SIGTERM, keeping what each was;
/// one found ignored is left ignored.
pub fn install() -> Guard {
    let mut installed = Vec::new();
    for (i, &sig) in SIGNALS.iter().enumerate() {
        // SAFETY: a zeroed sigaction is a valid value to read into; the
        // handler only touches atomics and async-signal-safe calls.
        unsafe {
            let mut prev: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut prev) != 0 {
                continue;
            }
            if prev.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            if PREVIOUS[i].set(prev).is_err() {
                // Installed before: in-process runs share it.
                continue;
            }
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut act.sa_mask);
            act.sa_flags = libc::SA_RESTART;
            if libc::sigaction(sig, &act, std::ptr::null_mut()) == 0 {
                installed.push(i);
            }
        }
    }
    Guard { installed }
}

impl Drop for Guard {
    fn drop(&mut self) {
        for &i in &self.installed {
            if let Some(prev) = PREVIOUS[i].get() {
                // SAFETY: restoring the action read at install.
                unsafe {
                    libc::sigaction(SIGNALS[i], prev, std::ptr::null_mut());
                }
            }
        }
    }
}

/// The signal that asked dform to stop, if one has.
pub fn requested() -> Option<i32> {
    match SIGNALLED.load(Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}

/// The signal's name, as messages say it.
pub fn name(sig: i32) -> &'static str {
    match sig {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}

/// An error when a stop was asked for: the run unwinds from there.
pub fn check() -> anyhow::Result<()> {
    match requested() {
        Some(sig) => Err(Interrupted(sig).into()),
        None => Ok(()),
    }
}

/// Sleep `d`, or less when a stop is asked for meanwhile: whether one was.
pub fn sleep(d: Duration) -> bool {
    let end = Instant::now() + d;
    loop {
        if requested().is_some() {
            return true;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// A stop was asked for by signal `.0`: no new Apply call was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted(pub i32);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "interrupted ({}): no new provider call was made",
            name(self.0)
        )
    }
}

impl std::error::Error for Interrupted {}
