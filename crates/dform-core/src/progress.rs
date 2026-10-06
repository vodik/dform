//! Progress lines (R-81): what a run that takes a while is doing, on
//! stderr, one line each, so stdout stays the report (R-63). A retry
//! (`plugin::link::Retry::line`) and a tick waiting on open nulls
//! ([`Wait`]) say so here.

use std::time::{Duration, Instant};

/// Print one progress line.
pub fn line(text: &str) {
    eprintln!("{text}");
}

/// How often a wait says it is still waiting.
pub const EVERY: Duration = Duration::from_secs(10);

/// How long a tick waits on open nulls when neither `apply --wait` nor
/// `[stacks.NAME] wait` says.
pub const WAIT: Duration = Duration::from_secs(600);

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
}
