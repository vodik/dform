//! Progress lines (R-81): what a run that takes a while is doing, on
//! stderr, one line each, so stdout stays the report (R-63). A retry
//! (`plugin::link::Retry::line`) and a tick waiting on open nulls
//! (`Wait`) say so here.

use std::time::{Duration, Instant};

/// Print one progress line.
pub fn line(text: &str) {
    eprintln!("{text}");
}

/// How often a wait says it is still waiting.
pub const EVERY: Duration = Duration::from_secs(10);

/// A wait on open nulls: when it started, and when it last said so.
pub struct Wait {
    started: Instant,
    /// The wall-clock time it started, `HH:MM` UTC.
    since: String,
    said: Option<Instant>,
}

impl Default for Wait {
    fn default() -> Wait {
        Wait::new()
    }
}

impl Wait {
    pub fn new() -> Wait {
        let now = jiff::Timestamp::now();
        Wait {
            started: Instant::now(),
            since: now.strftime("%H:%M").to_string(),
            said: None,
        }
    }

    /// How long it has waited.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// `waiting on WHAT since 02:14 UTC (3m)`.
    pub fn text(&self, on: &[String]) -> String {
        format!(
            "waiting on {} since {} UTC ({})",
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
}
