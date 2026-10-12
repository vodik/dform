//! Where a run's time goes, read two ways.
//!
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
//!
//! `tracing` spans, for a trace file (`--trace FILE`) or a collector
//! (`OTEL_EXPORTER_OTLP_ENDPOINT`, the `dform` crate's `otel` feature),
//! both src/trace.rs. Each is named for what it is, the same in
//! every run, and carries names, kinds and counts, never a value:
//!
//! - `plan.phase`: a step of the run (loaded and compiled, located the
//!   deployment, evaluated, refresh, Plan calls, the policy pass, ..),
//!   its text under `otel.name`;
//! - `provider.call`: one call to a provider, with `provider` (its name
//!   as the run says it), `kind` (`Configure`, `Read`, `Plan`, `Query`,
//!   ..) and `addr` (the resource's address, or the extern's name);
//! - `io.call`: one request dform makes itself, with `kind` (`s3.get`,
//!   `s3.put`, `s3.list`, `io.read`, `oci.resolve`, ..) and `addr` (the
//!   location, without its userinfo, query or fragment);
//! - `tick`: one apply tick, with `tick` (from 1);
//! - `apply.wait`: a tick waiting on values the world has not reached
//!   (R-81), with `tick`, `kind` (`world`, `extern`) and `count`;
//! - `ratelimit.wait`: a provider's request waiting on its bucket's
//!   window in the host (`dform_host::ratelimit`), with `provider`,
//!   `bucket`, `queued` and `window`.
//!
//! Every span carries `seq`, its number in the run, and `waited_on`, the
//! `seq` of the span that last ended before it began. The run drives its
//! calls from one thread and waits on each answer, so that is what it
//! came after: a chain of calls sent one after another is a chain of
//! `waited_on`, and a batch sent together shares one. It says the order
//! the run took, not that the later call needs the earlier's answer. A
//! call ends when the run takes its answer (`plugin::timed`), which for
//! a provider the run is not waiting on then is after it came.
//!
//! With no subscriber (no trace asked for) and no `DFORM_LOG`, a span
//! costs its callsite's check.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The target every span of dform's is under: a subscriber keeps these
/// and drops the libraries' own.
pub const TARGET: &str = "dform";

/// When the run began, and whether the lines are on.
static START: OnceLock<(Instant, bool)> = OnceLock::new();

/// The next span's `seq`.
static SEQ: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The `seq` of the span that ended last on this thread.
    static LAST: Cell<Option<u64>> = const { Cell::new(None) };
}

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

/// How long since the run began.
pub fn elapsed() -> Duration {
    start().0.elapsed()
}

/// The run's last line: all of it.
pub fn finish() {
    line("finished", elapsed());
}

/// Whether `DFORM_LOG=debug` asks for the lines.
pub fn enabled() -> bool {
    start().1
}

/// Whether a subscriber keeps dform's spans (a trace file, OTLP).
pub fn tracing() -> bool {
    tracing::enabled!(target: TARGET, tracing::Level::INFO)
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
        elapsed().as_secs_f64(),
        took.as_secs_f64() * 1000.0
    ));
}

/// The run's span, set by whoever exports the spans: a span made on a
/// thread with no span of its own (a state read beside the providers'
/// start, a git fetch) is under it, in the run's trace.
static ROOT: OnceLock<tracing::Id> = OnceLock::new();

/// The run's span is `id`.
pub fn set_root(id: Option<tracing::Id>) {
    if let Some(id) = id {
        let _ = ROOT.set(id);
    }
}

/// A new span's parent: the span it is made in, else the run's.
fn parent() -> Option<tracing::Id> {
    tracing::Span::current()
        .id()
        .or_else(|| ROOT.get().cloned())
}

/// A new span's `seq` and what it waited on.
fn next() -> (u64, Option<u64>) {
    (SEQ.fetch_add(1, Ordering::Relaxed), LAST.get())
}

/// What `f` took, as a phase; `what` is only made when someone reads it.
pub fn time<T>(what: impl FnOnce() -> String, f: impl FnOnce() -> T) -> T {
    let _phase = span(what);
    f()
}

/// A phase, a `plan.phase` span entered until it is dropped (what is
/// done meanwhile is in it), and a line then.
pub struct Span {
    /// The line's text, when the lines are on.
    what: Option<String>,
    from: Instant,
    seq: u64,
    _span: tracing::span::EnteredSpan,
}

/// The phase `what`, until the returned span is dropped.
pub fn span(what: impl FnOnce() -> String) -> Option<Span> {
    let (lines, traced) = (enabled(), tracing());
    if !lines && !traced {
        return None;
    }
    let what = what();
    let (seq, waited_on) = next();
    let span = match traced {
        true => tracing::info_span!(
            target: TARGET,
            parent: parent(),
            "plan.phase",
            otel.name = %what,
            seq,
            waited_on,
        ),
        false => tracing::Span::none(),
    };
    Some(Span {
        what: lines.then_some(what),
        from: Instant::now(),
        seq,
        _span: span.entered(),
    })
}

/// The phase `what` for a subscriber only, no line: a step the lines do
/// not say.
pub fn traced(what: impl FnOnce() -> String) -> Option<Span> {
    if !tracing() {
        return None;
    }
    let (seq, waited_on) = next();
    let span = tracing::info_span!(
        target: TARGET,
        parent: parent(),
        "plan.phase",
        otel.name = %what(),
        seq,
        waited_on,
    );
    Some(Span {
        what: None,
        from: Instant::now(),
        seq,
        _span: span.entered(),
    })
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(what) = &self.what {
            say(what, self.from.elapsed());
        }
        LAST.set(Some(self.seq));
    }
}

/// A call in flight: a `provider.call` or `io.call` span, not entered
/// (its answer may come after others'), ended by [`Call::end`] or by
/// being dropped.
pub struct Call {
    seq: u64,
    _span: tracing::Span,
}

impl Call {
    /// Its answer is in: what is asked next waited on it.
    pub fn end(self) {}
}

impl Drop for Call {
    fn drop(&mut self) {
        LAST.set(Some(self.seq));
    }
}

/// A call to the provider `provider` begins: `kind` the method, `addr`
/// what it is about. `None` unless a subscriber keeps it.
pub fn provider_call(provider: &str, kind: &str, addr: &str) -> Option<Call> {
    if !tracing() {
        return None;
    }
    let (seq, waited_on) = next();
    let addr = (!addr.is_empty()).then_some(addr);
    // A provider not yet named by its handshake, by its executable's name.
    let provider = provider.rsplit('/').next().unwrap_or(provider);
    let span = tracing::info_span!(
        target: TARGET,
        parent: parent(),
        "provider.call",
        otel.name = %format_args!("{provider} {kind}"),
        otel.kind = "client",
        provider,
        kind,
        addr,
        seq,
        waited_on,
    );
    Some(Call { seq, _span: span })
}

/// A request dform makes itself, `kind` of `addr`, for `f`; its line,
/// `what`, under `DFORM_LOG`.
pub fn io<T>(
    kind: &str,
    addr: impl FnOnce() -> String,
    what: impl FnOnce() -> String,
    f: impl FnOnce() -> T,
) -> T {
    let io = io_begin(kind, addr);
    let out = f();
    io.end(what);
    out
}

/// A request dform makes itself, begun ([`io_begin`]) and not yet
/// answered: one made beside others (a round's reads on threads) begins
/// with them, so they wait on one span, and ends when the run takes its
/// answer.
pub struct Io {
    call: Option<Call>,
    from: Option<Instant>,
}

/// The request `kind` of `addr` begins (`io`'s, made in parts).
pub fn io_begin(kind: &str, addr: impl FnOnce() -> String) -> Io {
    let (lines, traced) = (enabled(), tracing());
    let call = traced.then(|| {
        let (seq, waited_on) = next();
        let addr = addr();
        let span = tracing::info_span!(
            target: TARGET,
            parent: parent(),
            "io.call",
            otel.name = kind,
            otel.kind = "client",
            kind,
            addr,
            seq,
            waited_on,
        );
        Call { seq, _span: span }
    });
    Io {
        call,
        from: (lines || traced).then(Instant::now),
    }
}

impl Io {
    /// Its answer is taken; its line, `what`, under `DFORM_LOG`.
    pub fn end(self, what: impl FnOnce() -> String) {
        if let Some(from) = self.from {
            line(&what(), from.elapsed());
        }
        drop(self.call);
    }
}

/// Apply tick `tick` waiting on `count` values not reached yet, of
/// `kind` (`world`: an object's attribute; `extern`: a call's "not
/// yet"; both, joined): an `apply.wait` span entered until it is dropped.
pub fn wait(tick: usize, kind: &str, count: usize) -> Option<Span> {
    if !tracing() {
        return None;
    }
    let (seq, waited_on) = next();
    let span = tracing::info_span!(
        target: TARGET,
        parent: parent(),
        "apply.wait",
        otel.name = %format_args!("wait, tick {tick}"),
        tick,
        kind,
        count,
        seq,
        waited_on,
    );
    Some(Span {
        what: None,
        from: Instant::now(),
        seq,
        _span: span.entered(),
    })
}

/// A request of the provider `provider` waiting on its bucket's window
/// (`dform_host::ratelimit`): a `ratelimit.wait` span, entered until it
/// is dropped, with `bucket` (empty: the default), `queued` (the requests
/// before it) and `window` (how many may be in flight).
pub fn ratelimit_wait(provider: &str, bucket: &str, queued: usize, window: usize) -> Option<Span> {
    if !tracing() {
        return None;
    }
    let (seq, waited_on) = next();
    let span = tracing::info_span!(
        target: TARGET,
        parent: parent(),
        "ratelimit.wait",
        otel.name = %format_args!("{provider} waits on its window"),
        provider,
        bucket,
        queued,
        window,
        seq,
        waited_on,
    );
    Some(Span {
        what: None,
        from: Instant::now(),
        seq,
        _span: span.entered(),
    })
}

/// A location as a span names it: a URI without its userinfo, query or
/// fragment, where a credential could be written; anything else as it is.
pub fn location(text: &str) -> String {
    let Some((scheme, rest)) = text.split_once("://") else {
        return text.to_string();
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}://{host}{path}")
}

/// Apply tick `tick`, a `tick` span entered until it is dropped.
pub fn tick(tick: usize) -> Option<Span> {
    if !tracing() {
        return None;
    }
    let (seq, waited_on) = next();
    let span = tracing::info_span!(
        target: TARGET,
        parent: parent(),
        "tick",
        otel.name = %format_args!("tick {tick}"),
        tick,
        seq,
        waited_on,
    );
    Some(Span {
        what: None,
        from: Instant::now(),
        seq,
        _span: span.entered(),
    })
}

#[cfg(test)]
mod tests {
    /// A location's credentials are not a span's: userinfo, query and
    /// fragment go; a path or an image reference is as written.
    #[test]
    fn a_location_is_named_without_its_credentials() {
        let cases = [
            (
                "ssh://ubuntu@10.0.0.5/etc/rancher/k3s/k3s.yaml",
                "ssh://10.0.0.5/etc/rancher/k3s/k3s.yaml",
            ),
            (
                "https://user:token@example.com/a/b.yaml?sig=abc#frag",
                "https://example.com/a/b.yaml",
            ),
            (
                "git+https://example.com/repo.git//charts/x.yaml?ref=v1",
                "git+https://example.com/repo.git//charts/x.yaml",
            ),
            (
                "s3://bucket/state/app/state.json",
                "s3://bucket/state/app/state.json",
            ),
            ("config/net.csv", "config/net.csv"),
            ("ghcr.io/org/app:1.2", "ghcr.io/org/app:1.2"),
        ];
        for (text, named) in cases {
            assert_eq!(super::location(text), named, "{text}");
        }
    }
}
