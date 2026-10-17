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
//!   ..), `addr` (the resource's address, or the extern's name) and
//!   `session` (the configured instance the call is made in);
//! - `io.call`: one request dform makes itself, with `kind` (`s3.get`,
//!   `s3.put`, `s3.list`, `io.read`, `oci.resolve`, ..) and `addr` (the
//!   location, without its userinfo, query or fragment); or one a
//!   provider sends through the host (`http`, with `provider`, `bucket`
//!   and `status`), under the `provider.call` it is made for
//!   (`plugin::timed::call_span`);
//! - `tick`: one apply tick, with `tick` (from 1);
//! - `apply.wait`: a tick waiting on values the world has not reached
//!   (R-81), with `tick`, `kind` (`world`, `extern`) and `count`;
//! - `ratelimit.wait`: a provider's request waiting on its bucket's
//!   window in the host (`dform_host::ratelimit`), under its `io.call`,
//!   with `provider`, `session`, `bucket`, `queued` and `window`.
//!
//! Each is the function that does the work, `#[tracing::instrument]`ed
//! under [`TARGET`]; a call, whose answer may come after others', is a
//! span held by what tracks it and ended when that is dropped
//! (`plugin::timed`'s `Live`, a round's `Fetching`), when the run takes
//! its answer, which for a provider the run is not waiting on then is
//! after it came. Work a thread of dform's does for the run is in the
//! span current where the thread was started (`Span::current()` taken
//! there and entered around its work, as tokio's `Instrumented` does): the
//! lease's renewer, the host serving a provider, a round's requests. The
//! order the run took is a reader's to compute, from the spans' times and
//! parents. A phase `DFORM_LOG=debug` says is a
//! `debug` span; one only a trace showed, `trace`.
//!
//! With no subscriber (no trace asked for) and no `DFORM_LOG`, a span
//! costs its callsite's check.

/// The target every span of dform's is under: a subscriber keeps these
/// and drops the libraries' own.
pub const TARGET: &str = "dform";

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

/// Whether a request to `url` asks its server for a dry run, which
/// [`location`] does not say: a query parameter `dryRun` (Kubernetes's
/// `dryRun=All`), `DryRun` or `dry_run`, not `false`.
pub fn dry_run(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    let query = query.split('#').next().unwrap_or_default();
    query.split('&').any(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        k.replace('_', "").eq_ignore_ascii_case("dryrun") && !v.eq_ignore_ascii_case("false")
    })
}

#[cfg(test)]
mod tests {
    /// A dry run is the query's to say, in any API's spelling of it.
    #[test]
    fn a_dry_run_is_its_querys() {
        for (url, dry) in [
            ("https://k/api/v1/namespaces/a?dryRun=All", true),
            (
                "https://k/api/v1/namespaces/a?fieldManager=dform&dryRun=All",
                true,
            ),
            ("https://ec2/?Action=RunInstances&DryRun=true", true),
            ("https://x/apply?dry_run", true),
            ("https://x/apply?dry_run=false", false),
            ("https://k/api/v1/namespaces/a", false),
            (
                "https://k/api/v1/namespaces/a?fieldManager=dform#dryRun=All",
                false,
            ),
        ] {
            assert_eq!(super::dry_run(url), dry, "{url}");
        }
    }

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
