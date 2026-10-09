//! Progress lines (R-81): what a run that takes a while is doing, on
//! stderr, one line each, so stdout stays the report (R-63). A retry
//! (`plugin::link::Retry::line`), a tick waiting on open nulls ([`Wait`])
//! and a `DFORM_LOG=debug` line (`timing`) say so here.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where a progress line goes instead of stderr: the printer of the block
/// drawn meanwhile (the `dform` binary's apply), which says it above the
/// block and draws the block again below it.
type Route = Box<dyn Fn(&str) + Send>;

static ROUTE: Mutex<Option<Route>> = Mutex::new(None);

/// Send progress lines to `to` from now on; `None`: to stderr again.
pub fn route(to: Option<Route>) {
    *ROUTE.lock().unwrap_or_else(|e| e.into_inner()) = to;
}

/// Print one progress line: on stderr, or through the block drawn
/// meanwhile ([`route`]).
pub fn line(text: &str) {
    match &*ROUTE.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(to) => to(text),
        None => eprintln!("{text}"),
    }
}

/// How often a wait says it is still waiting.
pub const EVERY: Duration = Duration::from_secs(10);

/// A wait on open nulls: when it started, and when it last said so.
pub struct Wait {
    started: Instant,
    /// The wall-clock time it started, `HH:MM` local.
    since: String,
    said: Option<Instant>,
    /// Polls so far.
    polls: u32,
}

impl Default for Wait {
    fn default() -> Wait {
        Wait::new()
    }
}

impl Wait {
    pub fn new() -> Wait {
        Wait {
            started: Instant::now(),
            since: jiff::Zoned::now().strftime("%H:%M").to_string(),
            said: None,
            polls: 0,
        }
    }

    /// How long it has waited.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// When it started, `HH:MM`.
    pub fn since(&self) -> &str {
        &self.since
    }

    /// `waiting on WHAT since 02:14 (3m)`.
    pub fn text(&self, on: &[String]) -> String {
        format!(
            "waiting on {} since {} ({})",
            on.join(", "),
            self.since,
            crate::plugin::policy::show(Duration::from_secs(self.elapsed().as_secs()))
        )
    }

    /// Say what it waits on: the first time, then every [`EVERY`].
    pub fn tick(&mut self, on: &[String]) {
        if self.said.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        self.said = Some(Instant::now());
        line(&self.text(on));
    }

    /// How long to sleep before looking again, at most what is left of
    /// `budget`: 1s, doubling to 10s; `DFORM_WAIT_POLL_MS` fixes it (for
    /// tests).
    pub fn next_poll(&mut self, budget: Duration) -> Duration {
        self.polls += 1;
        let poll = std::env::var("DFORM_WAIT_POLL_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| {
                Duration::from_secs(1u64 << self.polls.saturating_sub(1).min(4)).min(EVERY)
            });
        poll.min(budget.saturating_sub(self.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_says_what_it_waits_on_and_since_when() {
        let w = Wait::new();
        let t = w.text(&["k8s.job[\"migrate-v42\"].status.succeeded".into()]);
        assert!(
            t.starts_with("waiting on k8s.job[\"migrate-v42\"].status.succeeded since ")
                && t.ends_with(" (0s)"),
            "{t}"
        );
        assert_eq!(w.since().len(), 5, "{}", w.since());
    }

    /// A line said while a block is drawn (a retry, a `DFORM_LOG=debug`
    /// line) goes to the block's printer, never past it to stderr; once
    /// the block is gone, to stderr again.
    #[test]
    fn a_line_goes_where_it_is_routed() {
        let said = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let to = said.clone();
        route(Some(Box::new(move |l: &str| {
            to.lock().unwrap().push(l.to_string())
        })));
        line("retry net.vpc main apply (2/5)");
        // A `DFORM_LOG=debug` line too: it is said as a progress line.
        crate::timing::say("apply net.vpc main: made", Duration::ZERO);
        route(None);
        line("after the block");
        let said = said.lock().unwrap();
        assert!(said.contains(&"retry net.vpc main apply (2/5)".to_string()));
        assert!(
            said.iter()
                .any(|l| l.starts_with("dform: ") && l.ends_with("  apply net.vpc main: made")),
            "{said:?}"
        );
        assert!(!said.contains(&"after the block".to_string()));
    }
}
