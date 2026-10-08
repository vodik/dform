//! What the credentials are and what they may do (R-179). A consumer key
//! made at `createToken` has a validity (a day, a week, unlimited) and a
//! list of rights; `GET /auth/currentCredential` answers both. Configure
//! warns of a key that expires within [`SHORT`], and, run outside a
//! program (as `dform provider check` runs it), says which form of
//! credentials is in use and each of [`RIGHTS`] the key lacks. A service
//! account's rights are its IAM policies, which the API does not answer
//! to the account itself.

use crate::api::{self, Client};
use crate::config::Auth;
use dform_core::approval::{parse_rfc3339, rfc3339};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::path::Path;

/// The calls the provider makes, as a consumer key's rights name them:
/// a type that calls another path adds its line here and to
/// docs/providers/ovh.md.
pub const RIGHTS: [(&str, &str); 9] = [
    ("GET", "/cloud/project"),
    ("GET", "/cloud/project/*"),
    ("POST", "/cloud/project/*"),
    ("PUT", "/cloud/project/*"),
    ("DELETE", "/cloud/project/*"),
    ("GET", "/domain/zone/*"),
    ("POST", "/domain/zone/*"),
    ("PUT", "/domain/zone/*"),
    ("DELETE", "/domain/zone/*"),
];

/// A consumer key that expires sooner than this is warned of.
pub const SHORT: u64 = 7 * 86_400;

/// The path the key's validity and rights are read from.
const CURRENT: &str = "/auth/currentCredential";

/// A consumer key as the API describes it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Key {
    /// When it expires, in seconds since the epoch; `None`: never.
    expires: Option<u64>,
    /// Its rights: method and path pattern (`*` any run of characters).
    rules: Vec<(String, String)>,
}

impl Key {
    fn from_answer(j: &Json) -> Key {
        Key {
            expires: j
                .get("expiration")
                .and_then(Json::as_str)
                .and_then(parse_time),
            rules: j
                .get("rules")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    let m = r.get("method").and_then(Json::as_str)?;
                    let p = r.get("path").and_then(Json::as_str)?;
                    Some((m.to_string(), p.to_string()))
                })
                .collect(),
        }
    }

    /// Each of [`RIGHTS`] none of its rules grants.
    fn lacks(&self) -> Vec<String> {
        RIGHTS
            .iter()
            .filter(|(method, path)| {
                !self
                    .rules
                    .iter()
                    .any(|(m, p)| m == method && glob(p.as_bytes(), path.as_bytes()))
            })
            .map(|(m, p)| format!("{m} {p}"))
            .collect()
    }
}

/// The notes on `client`'s credentials, each a line for stderr. `full`
/// (no program: `provider check`) says which form is in use, the key's
/// validity and each right it lacks; otherwise only a short validity is
/// warned of, and the key's expiry is kept in the cache directory `cache`
/// (by a digest of the key), so a later run asks nothing.
pub fn notes(client: &Client, full: bool, cache: Option<&Path>, now: u64) -> Vec<String> {
    let creds = client.credentials();
    let endpoint = &creds.endpoint;
    let consumer_key = match &creds.auth {
        Auth::Keys { consumer_key, .. } => consumer_key,
        Auth::OAuth2 { client_id, .. } => {
            return if full {
                vec![format!(
                    "provider ovh: credentials for {endpoint}: the service account {client_id}; \
                     its rights are its IAM policies"
                )]
            } else {
                Vec::new()
            };
        }
    };
    let file = cache.filter(|_| !full).map(|c| c.join(api::CACHE_FILE));
    let cache_key = format!("{endpoint} {CURRENT}");
    let digest: String = Sha256::digest(consumer_key.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let kept = file.as_deref().and_then(|f| {
        let entry = api::read_cache(f).remove(&cache_key)?;
        let (d, expires) = entry.split_once(' ')?;
        (d == digest).then(|| Key {
            expires: expires.parse().ok(),
            rules: Vec::new(),
        })
    });
    let key = match kept {
        Some(k) => k,
        None => match client.get(CURRENT) {
            Ok(j) => {
                let k = Key::from_answer(&j);
                if let Some(f) = &file {
                    let expires = k.expires.map_or("never".to_string(), |e| e.to_string());
                    api::write_cache(f, &cache_key, &format!("{digest} {expires}"));
                }
                k
            }
            // A refused key is said by the call that needs it.
            Err(e) if full => {
                return vec![format!("provider ovh: credentials for {endpoint}: {e}")];
            }
            Err(_) => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    if full {
        let validity = match key.expires {
            Some(t) => format!("expires {}", rfc3339(t)),
            None => "does not expire".to_string(),
        };
        out.push(format!(
            "provider ovh: credentials for {endpoint}: an application key and a consumer key; \
             the consumer key {validity}"
        ));
        out.extend(
            key.lacks()
                .into_iter()
                .map(|r| format!("provider ovh: the consumer key for {endpoint} lacks {r}")),
        );
    }
    if let Some(t) = key.expires
        && t > now
        && t - now < SHORT
    {
        out.push(format!(
            "warning: provider ovh: the consumer key for {endpoint} expires {}, in {}; make one \
             with unlimited validity at {} or use a service account (docs/providers/ovh.md)",
            rfc3339(t),
            within(t - now),
            creds.create_token_url()
        ));
    }
    out
}

/// `secs` in hours under two days, else in days.
fn within(secs: u64) -> String {
    match secs / 3600 {
        0 | 1 => "an hour".to_string(),
        h if h < 48 => format!("{h} hours"),
        h => format!("{} days", h / 24),
    }
}

/// An expiry as OVH gives it (`2026-10-09T08:00:00+02:00`), in seconds
/// since the epoch.
fn parse_time(s: &str) -> Option<u64> {
    if s.ends_with(['Z', 'z']) {
        return parse_rfc3339(s);
    }
    let (base, offset) = s.split_at(s.len().checked_sub(6)?);
    let (sign, hm) = match offset.as_bytes().first()? {
        b'+' => (1, &offset[1..]),
        b'-' => (-1, &offset[1..]),
        _ => return None,
    };
    let (h, m) = hm.split_once(':')?;
    let off = (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?) * 60;
    let utc = parse_rfc3339(&format!("{base}Z"))? as i64 - sign * off;
    u64::try_from(utc).ok()
}

/// Whether `pattern` (`*` any run of bytes) matches all of `text`.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        Some((c, rest)) => text.first() == Some(c) && glob(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expiry_with_an_offset() {
        assert_eq!(
            parse_time("2026-10-09T08:00:00+02:00"),
            parse_rfc3339("2026-10-09T06:00:00Z")
        );
        assert_eq!(
            parse_time("2026-10-09T08:00:00-04:00"),
            parse_rfc3339("2026-10-09T12:00:00Z")
        );
        assert_eq!(
            parse_time("2026-10-09T08:00:00Z"),
            parse_rfc3339("2026-10-09T08:00:00Z")
        );
        assert_eq!(parse_time("tomorrow"), None);
    }

    #[test]
    fn a_rule_grants_what_its_pattern_covers() {
        let key = |rules: &[(&str, &str)]| Key {
            expires: None,
            rules: rules
                .iter()
                .map(|(m, p)| (m.to_string(), p.to_string()))
                .collect(),
        };
        let all = key(&[
            ("GET", "/*"),
            ("POST", "/*"),
            ("PUT", "/*"),
            ("DELETE", "/*"),
        ]);
        assert!(all.lacks().is_empty());
        let cloud = key(&[
            ("GET", "/cloud/*"),
            ("POST", "/cloud/project/*"),
            ("PUT", "/cloud/project/*"),
            ("DELETE", "/cloud/project/*"),
            ("GET", "/domain/zone/*"),
        ]);
        assert_eq!(
            cloud.lacks(),
            [
                "POST /domain/zone/*",
                "PUT /domain/zone/*",
                "DELETE /domain/zone/*"
            ]
        );
    }

    #[test]
    fn within_reads_as_a_person_would() {
        assert_eq!(within(1800), "an hour");
        assert_eq!(within(20 * 3600 + 5), "20 hours");
        assert_eq!(within(3 * 86_400), "3 days");
    }
}
