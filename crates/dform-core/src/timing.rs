//! Where a run's time goes: `tracing` spans, for a trace file (`--trace
//! FILE`) or a collector (`OTEL_EXPORTER_OTLP_ENDPOINT`, the `dform`
//! crate's `otel` feature), both src/trace.rs, and for `DFORM_LOG`'s
//! lines ([`crate::log`]). Each is named for what it is, the same in
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
//! A call ends when the run takes its answer (`plugin::timed`), which for
//! a provider the run is not waiting on then is after it came. The order
//! the run took is a reader's to compute, from the spans' times and
//! parents. A phase `DFORM_LOG=debug` says is a `debug` span; one only a
//! trace showed, `trace`.
//!
//! With no subscriber (no trace asked for) and no `DFORM_LOG`, a span
//! costs its callsite's check.

use std::sync::OnceLock;

/// The target every span of dform's is under: a subscriber keeps these
/// and drops the libraries' own.
pub const TARGET: &str = "dform";

/// Whether a subscriber keeps dform's spans of `level`.
fn kept(level: tracing::Level) -> bool {
    match level {
        tracing::Level::TRACE => tracing::enabled!(target: TARGET, tracing::Level::TRACE),
        _ => tracing::enabled!(target: TARGET, tracing::Level::DEBUG),
    }
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

/// What `f` took, as a phase; `what` is only made when someone reads it.
pub fn time<T>(what: impl FnOnce() -> String, f: impl FnOnce() -> T) -> T {
    let _phase = span(what);
    f()
}

/// A phase, a `plan.phase` span entered until it is dropped (what is
/// done meanwhile is in it).
pub struct Span {
    _span: tracing::span::EnteredSpan,
}

/// The phase `what`, until the returned span is dropped.
pub fn span(what: impl FnOnce() -> String) -> Option<Span> {
    if !kept(tracing::Level::DEBUG) {
        return None;
    }
    let span = tracing::debug_span!(
        target: TARGET,
        parent: parent(),
        "plan.phase",
        otel.name = %what(),
    );
    Some(Span {
        _span: span.entered(),
    })
}

/// The phase `what` at `trace`: a step `DFORM_LOG=debug` does not say.
pub fn traced(what: impl FnOnce() -> String) -> Option<Span> {
    if !kept(tracing::Level::TRACE) {
        return None;
    }
    let span = tracing::trace_span!(
        target: TARGET,
        parent: parent(),
        "plan.phase",
        otel.name = %what(),
    );
    Some(Span {
        _span: span.entered(),
    })
}

/// A call in flight: a `provider.call` or `io.call` span, not entered
/// (its answer may come after others'), ended by [`Call::end`] or by
/// being dropped.
pub struct Call {
    _span: tracing::Span,
}

impl Call {
    /// Its answer is in.
    pub fn end(self) {}
}

/// A call to the provider `provider` begins: `kind` the method, `addr`
/// what it is about. `None` unless a subscriber keeps it.
pub fn provider_call(provider: &str, kind: &str, addr: &str) -> Option<Call> {
    if !kept(tracing::Level::DEBUG) {
        return None;
    }
    let addr = (!addr.is_empty()).then_some(addr);
    // A provider not yet named by its handshake, by its executable's name.
    let provider = provider.rsplit('/').next().unwrap_or(provider);
    let span = tracing::debug_span!(
        target: TARGET,
        parent: parent(),
        "provider.call",
        otel.name = %format_args!("{provider} {kind}"),
        otel.kind = "client",
        provider,
        kind,
        addr,
    );
    Some(Call { _span: span })
}

/// A request dform makes itself, `kind` of `addr`, for `f`.
pub fn io<T>(
    kind: &str,
    addr: impl FnOnce() -> String,
    _what: impl FnOnce() -> String,
    f: impl FnOnce() -> T,
) -> T {
    let io = io_begin(kind, addr);
    let out = f();
    drop(io);
    out
}

/// A request dform makes itself, begun ([`io_begin`]) and not yet
/// answered: one made beside others (a round's reads on threads) begins
/// with them, and ends when the run takes its answer.
pub struct Io {
    _call: Option<Call>,
}

/// The request `kind` of `addr` begins (`io`'s, made in parts).
pub fn io_begin(kind: &str, addr: impl FnOnce() -> String) -> Io {
    let call = kept(tracing::Level::DEBUG).then(|| {
        let span = tracing::debug_span!(
            target: TARGET,
            parent: parent(),
            "io.call",
            otel.name = kind,
            otel.kind = "client",
            kind,
            addr = addr(),
        );
        Call { _span: span }
    });
    Io { _call: call }
}

impl Io {
    /// Its answer is taken.
    pub fn end(self, _what: impl FnOnce() -> String) {}
}

/// Apply tick `tick` waiting on `count` values not reached yet, of
/// `kind` (`world`: an object's attribute; `extern`: a call's "not
/// yet"; both, joined): an `apply.wait` span entered until it is dropped.
pub fn wait(tick: usize, kind: &str, count: usize) -> Option<Span> {
    if !kept(tracing::Level::TRACE) {
        return None;
    }
    let span = tracing::trace_span!(
        target: TARGET,
        parent: parent(),
        "apply.wait",
        otel.name = %format_args!("wait, tick {tick}"),
        tick,
        kind,
        count,
    );
    Some(Span {
        _span: span.entered(),
    })
}

/// A request of the provider `provider` waiting on its bucket's window
/// (`dform_host::ratelimit`): a `ratelimit.wait` span, entered until it
/// is dropped, with `bucket` (empty: the default), `queued` (the requests
/// before it) and `window` (how many may be in flight).
pub fn ratelimit_wait(provider: &str, bucket: &str, queued: usize, window: usize) -> Option<Span> {
    if !kept(tracing::Level::TRACE) {
        return None;
    }
    let span = tracing::trace_span!(
        target: TARGET,
        parent: parent(),
        "ratelimit.wait",
        otel.name = %format_args!("{provider} waits on its window"),
        provider,
        bucket,
        queued,
        window,
    );
    Some(Span {
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
    if !kept(tracing::Level::TRACE) {
        return None;
    }
    let span = tracing::trace_span!(
        target: TARGET,
        parent: parent(),
        "tick",
        otel.name = %format_args!("tick {tick}"),
        tick,
    );
    Some(Span {
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
