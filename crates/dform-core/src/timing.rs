//! `DFORM_LOG=debug`: a line on stderr for each phase of a run and each
//! call it makes over the network, with its wall time, so where a slow
//! plan spends its time is read off the run rather than an strace (while
//! an apply's block is drawn, above it, as any progress line):
//!
//!   dform:   0.412s    61.3ms  s3 get s3://bucket/app/state.json (200)
//!
//! The first column is the time since the run began (when the line is
//! printed, at the end of what it times), the second how long that took.
//! A gap between one line's end and the next one's start is time spent in
//! nothing listed.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// When the run began, and whether the lines are on.
static START: OnceLock<(Instant, bool)> = OnceLock::new();

fn start() -> &'static (Instant, bool) {
    START.get_or_init(|| {
        let on = std::env::var("DFORM_LOG")
            .is_ok_and(|v| v.split(',').any(|w| matches!(w.trim(), "debug" | "trace")));
        (Instant::now(), on)
    })
}

/// Start the clock: the run's first act, so the first column counts from it.
pub fn begin() {
    start();
}

/// The run's last line: all of it.
pub fn finish() {
    line("finished", start().0.elapsed());
}

/// Whether `DFORM_LOG=debug` asks for the lines.
pub fn enabled() -> bool {
    start().1
}

/// One line: `what` took `took`, and ended now.
pub fn line(what: &str, took: Duration) {
    if enabled() {
        say(what, took);
    }
}

/// The line of `what`, which took `took`, said as a progress line is:
/// on stderr, or above the apply's block while one is drawn
/// (`progress::route`), never through it.
pub(crate) fn say(what: &str, took: Duration) {
    crate::progress::line(&format!(
        "dform: {:>8.3}s {:>9.1}ms  {what}",
        start().0.elapsed().as_secs_f64(),
        took.as_secs_f64() * 1000.0
    ));
}

/// What `f` took, as a line; `what` is only made when the lines are on.
pub fn time<T>(what: impl FnOnce() -> String, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let t = Instant::now();
    let out = f();
    line(&what(), t.elapsed());
    out
}

/// A phase that ends where it is dropped.
pub struct Span {
    what: String,
    from: Instant,
}

/// A line for the phase `what` when the returned span is dropped.
pub fn span(what: impl FnOnce() -> String) -> Option<Span> {
    enabled().then(|| Span {
        what: what(),
        from: Instant::now(),
    })
}

impl Drop for Span {
    fn drop(&mut self) {
        line(&self.what, self.from.elapsed());
    }
}
