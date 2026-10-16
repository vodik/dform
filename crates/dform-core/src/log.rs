//! `DFORM_LOG`: dform's spans and events as lines on stderr, through
//! `tracing_subscriber::fmt` with an `EnvFilter` over dform's target
//! (`timing::TARGET`), the spans a trace file holds and nothing besides.
//! `debug` says each phase of a run and each call it makes as it ends,
//! with its time (`close time.busy=..`), and what a provider says of a
//! call in flight; `trace` adds the steps only a trace file showed (the
//! refresh, the Plan calls, each extern round, each tick). A line is the
//! time since the run began, the level, the span it is said in (the
//! innermost, with its fields: the path above it is a trace's to show),
//! and what is said (a span's close, with its times).
//!
//! A line is said as a progress line is (`progress::line`): on stderr, or
//! above the apply's block while one is drawn, never through it.

use crate::timing::TARGET;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::format::{FmtSpan, FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{self, FmtContext, FormattedFields, MakeWriter};
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
    lines(asked, Lines)
}

/// [`layer_for`], each line written by `writer`.
fn lines<S, W>(asked: &str, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::OFF.into())
        .parse_lossy(asked);
    fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .event_format(Leaf {
            since: fmt::time::uptime(),
        })
        .with_filter(filter)
        .with_filter(tracing_subscriber::filter::filter_fn(|m| {
            m.target() == TARGET
        }))
        .boxed()
}

/// A line's format: the time `since` the run began, the level, the
/// innermost span the event is in with its fields, and the event's.
struct Leaf {
    since: fmt::time::Uptime,
}

impl<S, N> FormatEvent<S, N> for Leaf
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'w> FormatFields<'w> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        self.since.format_time(&mut w)?;
        write!(w, " {:>5} ", event.metadata().level())?;
        if let Some(span) = ctx.event_scope().and_then(|mut scope| scope.next()) {
            write!(w, "{}", span.name())?;
            if let Some(fields) = span.extensions().get::<FormattedFields<N>>()
                && !fields.is_empty()
            {
                write!(w, "{{{fields}}}")?;
            }
            w.write_str(": ")?;
        }
        ctx.field_format().format_fields(w.by_ref(), event)?;
        writeln!(w)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The lines written, in memory.
    #[derive(Clone, Default)]
    struct Kept(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Kept {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Kept {
        type Writer = Kept;

        fn make_writer(&'a self) -> Kept {
            self.clone()
        }
    }

    /// A line names the innermost span it is said in, with its fields,
    /// and not the run's and the phases' above it: a reader of a long
    /// run's lines sees what each is about.
    #[test]
    fn a_line_names_its_innermost_span() {
        let kept = Kept::default();
        let subscriber = tracing_subscriber::registry().with(lines("debug", kept.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let _run =
                tracing::debug_span!(target: TARGET, "dform.run", command = "plan").entered();
            let _phase =
                tracing::debug_span!(target: TARGET, "plan.phase", otel.name = "evaluated")
                    .entered();
            let call = tracing::debug_span!(target: TARGET, "provider.call", kind = "Plan");
            call.in_scope(|| tracing::debug!(target: TARGET, "apply net.vpc main: made"));
        });
        let text = String::from_utf8(kept.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().map(|l| l.trim_start()).collect();
        let said = |l: &str| l.split_once(' ').map_or("", |(_, rest)| rest).to_string();
        assert_eq!(
            said(lines[0]),
            "DEBUG provider.call{kind=\"Plan\"}: apply net.vpc main: made",
            "{text}"
        );
        assert!(
            lines[1].contains(" DEBUG provider.call{kind=\"Plan\"}: close time.busy="),
            "{text}"
        );
        // The run is named by its own close alone.
        let run: Vec<&&str> = lines.iter().filter(|l| l.contains("dform.run")).collect();
        assert_eq!(run.len(), 1, "{text}");
        assert!(
            run[0].contains(" DEBUG dform.run{command=\"plan\"}: close "),
            "{text}"
        );
    }
}
