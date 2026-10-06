//! What dform does when a provider call is slow or fails (R-81): each
//! call has a timeout, and a call that failed in a way worth trying again
//! is retried with exponential backoff and jitter, up to a budget. A
//! provider's policy is its `[providers.NAME]` table's `timeout`,
//! `retries` and `backoff` in dform.toml, else [`Policy::default`].
//!
//! Which failures are retried is read from what the protocol already
//! returns (the proto does not change, DESIGN.org R-13): a refusal changed
//! nothing, so one that says it is transient is sent again; a call that
//! may have taken effect (a timeout, `DEADLINE_EXCEEDED`) is retried only
//! where sending it again is safe (a Read, a Plan; an Apply only after the
//! executor has looked, `plugin::providers::Tick`); a crash never.

use super::backend::{Call, CallError};
use std::time::Duration;

/// A provider's call policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How long a call may go unanswered before dform takes it as timed
    /// out (`MaybeApplied`).
    pub timeout: Duration,
    /// How many times a failed call is sent again before the run stops
    /// with its last error.
    pub retries: u32,
    /// The first retry's delay; each next one doubles, up to
    /// [`MAX_BACKOFF`].
    pub backoff: Duration,
}

/// The longest delay between two attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            timeout: Duration::from_secs(60),
            retries: 5,
            backoff: Duration::from_secs(1),
        }
    }
}

impl Policy {
    /// The delay before retry `attempt` (from 1): `backoff * 2^(attempt-1)`,
    /// at most [`MAX_BACKOFF`], jittered to between half of it and all of
    /// it, so that many callers backing off together do not retry
    /// together.
    pub fn delay(&self, attempt: u32) -> Duration {
        let full = self
            .backoff
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
            .min(MAX_BACKOFF.max(self.backoff));
        let half = full / 2;
        half + full.saturating_sub(half).mul_f64(jitter())
    }
}

/// A number in [0, 1), different each call.
fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// How a failed call is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Nothing changed and the failure is transient: send it again.
    Retryable,
    /// No answer came (a timeout): it may have taken effect.
    MaybeApplied,
    /// Retrying would fail the same way, or the provider is gone.
    Final,
}

/// The class of `e`. A refusal is retryable when its message says the
/// failure is transient, as a provider can say it within the protocol:
///
/// * the provider marks it: the message is, or has a clause that is,
///   `retryable: ...`;
/// * an HTTP status of 429 or 5xx, written `(429)` (the k8s provider's
///   `message (code)`), `HTTP 503` or `status 503`;
/// * the transport failed (`the provider NAME failed: status:
///   Unavailable ...`, the process backend's message for a call that
///   reached no answer while the process lives).
pub fn class(e: &CallError) -> Class {
    match e {
        CallError::Refused(m) if retryable(m) => Class::Retryable,
        CallError::Refused(_) | CallError::Crashed(_) => Class::Final,
        CallError::MaybeApplied(_) => Class::MaybeApplied,
    }
}

/// Whether a refusal's message says it is transient ([`class`]).
pub fn retryable(m: &str) -> bool {
    if m.starts_with("retryable:") || m.contains(": retryable:") {
        return true;
    }
    for code in ["Unavailable", "Unknown", "Cancelled"] {
        if m.contains(&format!("failed: status: {code}")) {
            return true;
        }
    }
    let transient = |code: &str| code == "429" || (code.starts_with('5') && code.len() == 3);
    let digits = |s: &str| -> String { s.chars().take_while(char::is_ascii_digit).collect() };
    for (i, _) in m.match_indices('(') {
        let code = digits(&m[i + 1..]);
        if code.len() == 3 && m[i + 1 + 3..].starts_with(')') && transient(&code) {
            return true;
        }
    }
    for prefix in ["HTTP ", "status ", "Status "] {
        for (i, _) in m.match_indices(prefix) {
            let code = digits(&m[i + prefix.len()..]);
            let after = m[i + prefix.len() + code.len()..].chars().next();
            if code.len() == 3 && !after.is_some_and(|c| c.is_ascii_digit()) && transient(&code) {
                return true;
            }
        }
    }
    false
}

/// What a call is, for messages: its method and what it is about
/// (`Read net.vpc["main"]`).
pub fn describe(call: &Call) -> String {
    let at = |typ: &str, name: &str| {
        if typ.is_empty() {
            String::new()
        } else {
            format!(
                " {}",
                crate::ir::Address {
                    typ: typ.to_string(),
                    name: name.to_string(),
                }
            )
        }
    };
    match call {
        Call::Read(r) => format!("Read{}", at(&r.r#type, &r.name)),
        Call::Plan(r) => format!("Plan{}", at(&r.r#type, &r.name)),
        Call::Apply(r) => format!("Apply{}", at(&r.r#type, &r.name)),
        Call::Import(r) => format!("Import {}::{}", r.r#type, r.remote),
        Call::Query(r) => format!("Query {}", r.pred),
        c => c.method().to_string(),
    }
}

/// `90s`, `2m`, `1m30s`, `500ms`: a duration as a person writes it.
pub fn show(d: Duration) -> String {
    let ms = d.as_millis();
    if ms == 0 {
        return "0s".into();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = d.as_secs();
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, 0, s) if ms.is_multiple_of(1000) => format!("{s}s"),
        (0, 0, _) => format!("{:.1}s", d.as_secs_f64()),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transient_refusal_is_retryable() {
        for m in [
            "retryable: the API is busy",
            "apply net.vpc[\"a\"]: retryable: throttled",
            "apply k8s.job[\"m\"]: Too Many Requests (429)",
            "apply x: HTTP 503 Service Unavailable",
            "read x: status 500",
            "the provider k8s failed: status: Unavailable, message: \"\"",
        ] {
            assert!(retryable(m), "{m}");
        }
        for m in [
            "apply x: create failed: x already exists in the world",
            "apply x: Conflict (409)",
            "apply x: HTTP 404",
            "apply x: (5000)",
            "apply x: status 5000",
            "the provider k8s failed: status: InvalidArgument",
        ] {
            assert!(!retryable(m), "{m}");
        }
        assert_eq!(class(&CallError::Crashed("x (503)".into())), Class::Final);
        assert_eq!(
            class(&CallError::MaybeApplied("x".into())),
            Class::MaybeApplied
        );
    }

    #[test]
    fn backoff_doubles_with_jitter_up_to_the_cap() {
        let p = Policy {
            backoff: Duration::from_secs(1),
            ..Policy::default()
        };
        for (attempt, full) in [(1, 1), (2, 2), (3, 4), (6, 30), (40, 30)] {
            let full = Duration::from_secs(full);
            for _ in 0..20 {
                let d = p.delay(attempt);
                assert!(d >= full / 2 && d <= full, "{attempt}: {d:?}");
            }
        }
    }

    #[test]
    fn durations_show_as_written() {
        let s = |ms| show(Duration::from_millis(ms));
        assert_eq!(s(0), "0s");
        assert_eq!(s(250), "250ms");
        assert_eq!(s(1500), "1.5s");
        assert_eq!(s(60_000), "1m");
        assert_eq!(s(180_000), "3m");
        assert_eq!(s(90_000), "1m30s");
        assert_eq!(s(7_200_000), "2h");
    }
}
