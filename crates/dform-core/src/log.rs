//! `DFORM_LOG`: dform's spans and events as lines on stderr, through
//! `tracing_subscriber::fmt` with an `EnvFilter` over dform's target
//! (`timing::TARGET`), the spans a trace file holds and nothing besides.
//! `debug` says each phase of a run and each call it makes as it ends,
//! with its time (`close time.busy=..`), and what a provider says of a
//! call in flight; `trace` adds the steps only a trace file showed (the
//! refresh, the Plan calls, each extern round, each tick). The first
//! column is the time since the run began.
//!
//! A line is said as a progress line is (`progress::line`): on stderr, or
//! above the apply's block while one is drawn, never through it.

use crate::timing::TARGET;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::{self, MakeWriter, format::FmtSpan};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

/// The lines `DFORM_LOG` asks for, as a layer of the run's subscriber;
/// `None` when it asks for none.
pub fn layer<S>() -> Option<Box<dyn Layer<S> + Send + Sync>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let asked = std::env::var("DFORM_LOG").ok()?;
    (!asked.trim().is_empty()).then(|| layer_for(&asked))
}

/// The lines the directives `asked` (an `EnvFilter`'s: `debug`,
/// `dform=trace`) select of dform's spans and events.
pub fn layer_for<S>(asked: &str) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::OFF.into())
        .parse_lossy(asked);
    fmt::layer()
        .with_writer(Lines)
        .with_ansi(false)
        .with_target(false)
        .with_timer(fmt::time::uptime())
        .with_span_events(FmtSpan::CLOSE)
        .with_filter(filter)
        .with_filter(tracing_subscriber::filter::filter_fn(|m| {
            m.target() == TARGET
        }))
        .boxed()
}

/// The lines alone, as a process's only subscriber: a provider's (the
/// Kubernetes provider's chain of errors under `DFORM_LOG=debug`), which
/// exports no trace.
pub fn install() {
    if let Some(lines) = layer() {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(lines));
    }
}

/// Where the lines go: one progress line an event.
struct Lines;

impl<'a> MakeWriter<'a> for Lines {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}

/// An event's line, said when the formatter is done with it.
struct Line(Vec<u8>);

impl std::io::Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let text = text.trim_end_matches('\n');
        if !text.is_empty() {
            crate::progress::line(text);
        }
    }
}
