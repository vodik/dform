//! `tailscale.auth_key`: a key a node joins the tailnet with, named by its
//! description, its remote id the API's. Every attribute is given at its
//! creation: any change makes a new key (the new one first) and revokes
//! the old one, which leaves the devices that joined with it on the
//! tailnet. A key that expired or was revoked in the console is gone, and
//! the next plan makes another.
//!
//! The API answers the key itself once, to the create. The provider keeps
//! it for the run and reveals it to the engine only (R-45): what dform
//! has, in state, the plan file, the audit log and every message, is its
//! label. A run after the one that made it cannot reveal it: the API has
//! no copy.

use crate::client::Body;
use crate::{Tailscale, at};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

pub const TYPE: &str = "tailscale.auth_key";

/// The longest a key may live, as the API allows: 90 days.
pub const MAX_EXPIRY: i64 = 90 * 24 * 3600;

/// An auth key.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(
    type = "tailscale.auth_key",
    replace = "create_first",
    lookup = "description"
)]
pub struct AuthKey {
    /// What the admin console lists it by.
    #[dform(required, force_new)]
    pub description: String,
    /// It may join more than one node.
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reusable: Option<bool>,
    /// A node it joins leaves the tailnet once it is offline for a while.
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    /// A node it joins needs no approval.
    #[dform(optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preauthorized: Option<bool>,
    /// The tags a node it joins has (`tag:k8s`).
    #[dform(ty = "set(string)", force_new)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// How long it is good for, at most 90 days (the API's own default).
    #[dform(ty = "duration(seconds)", optional_computed, force_new)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<i64>,
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The key: its label outside the provider.
    #[dform(computed, sensitive)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A refused reveal of a key this run did not make.
pub fn not_held(id: &str) -> String {
    format!(
        "{}: the API answers a key once, when it is made, and this run did not make it; a \
         node that needs it now needs a new key: change the key's description, or taint it \
         (`dform state taint`), to make one",
        at(TYPE, id)
    )
}

/// `key`, as the API answers one: its capabilities flattened.
fn from_api(k: &Json) -> AuthKey {
    let create = &k["capabilities"]["devices"]["create"];
    let flag = |n: &str| Some(create[n].as_bool().unwrap_or(false));
    let time = |n: &str| k[n].as_str().and_then(seconds_of);
    AuthKey {
        description: k["description"].as_str().unwrap_or_default().to_string(),
        reusable: flag("reusable"),
        ephemeral: flag("ephemeral"),
        preauthorized: flag("preauthorized"),
        tags: create["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .map(str::to_string)
            .collect(),
        expiry: match (time("created"), time("expires")) {
            (Some(c), Some(e)) => k["expirySeconds"].as_i64().or(Some(e - c)),
            _ => k["expirySeconds"].as_i64(),
        },
        id: k["id"].as_str().map(str::to_string),
        // It exists; the bytes are not the API's to answer again.
        key: Some(String::new()),
    }
}

/// Seconds since the epoch of an RFC 3339 time in UTC
/// (`2026-10-08T09:00:00Z`, fractions dropped).
fn seconds_of(t: &str) -> Option<i64> {
    let (date, time) = t.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['.', '+']).next()?;
    let mut h = time.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (h.next()??, h.next()??, h.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

impl Lifecycle<Tailscale> for AuthKey {
    /// The key, unless it is revoked or expired.
    fn read(p: &Tailscale, remote: &str) -> Result<Option<AuthKey>> {
        let Some(a) = p
            .client
            .get(&p.client.tailnet_path(&format!("/keys/{remote}")))?
        else {
            return Ok(None);
        };
        if a.body["revoked"].as_str().is_some_and(|r| !r.is_empty())
            || a.body["invalid"].as_bool() == Some(true)
        {
            return Ok(None);
        }
        Ok(Some(from_api(&a.body)))
    }

    fn create(
        p: &Tailscale,
        desired: AuthKey,
        _: &str,
        progress: &Progress,
    ) -> Result<(String, AuthKey)> {
        let mut body = json!({
            "description": desired.description,
            "capabilities": {"devices": {"create": {
                "reusable": desired.reusable.unwrap_or(false),
                "ephemeral": desired.ephemeral.unwrap_or(false),
                "preauthorized": desired.preauthorized.unwrap_or(false),
                "tags": desired.tags,
            }}},
        });
        if let Some(s) = desired.expiry {
            body["expirySeconds"] = json!(s);
        }
        progress.message("making the key");
        let a = p
            .client
            .call("POST", &p.client.tailnet_path("/keys"), Body::Json(body))?;
        let id = a.body["id"]
            .as_str()
            .ok_or_else(|| {
                Error::MaybeApplied(format!(
                    "{}: the API's answer names no id",
                    at(TYPE, &desired.description)
                ))
            })?
            .to_string();
        if let Some(k) = a.body["key"].as_str() {
            p.hold(&id, k.to_string());
        }
        let mut made = from_api(&a.body);
        made.expiry = made.expiry.or(desired.expiry);
        Ok((id, made))
    }

    /// Every attribute replaces: nothing is changed in place.
    fn update(
        p: &Tailscale,
        remote: &str,
        prior: AuthKey,
        _: AuthKey,
        _: &Progress,
    ) -> Result<AuthKey> {
        let _ = p;
        let _ = remote;
        Ok(prior)
    }

    /// Revoke it: the devices that joined with it stay.
    fn delete(p: &Tailscale, remote: &str, _: &Progress) -> Result<()> {
        let path = p.client.tailnet_path(&format!("/keys/{remote}"));
        let a = p.client.send("DELETE", &path, &Body::None, None, false)?;
        match a.status {
            404 => Ok(()),
            _ => p.client.expect("DELETE", &path, a).map(|_| ()),
        }
    }

    /// At most 90 days; a key an OAuth client makes carries a tag (the
    /// API refuses an untagged one).
    fn check(p: &Tailscale, desired: &Json) -> Result<()> {
        if let Some(s) = desired.get("expiry").and_then(Json::as_i64)
            && !(1..=MAX_EXPIRY).contains(&s)
        {
            return Err(Error::Refused(format!(
                "expiry is {s}s: a key lives at most 90d (7776000s), and at least a second"
            )));
        }
        let untagged = desired
            .get("tags")
            .is_none_or(|t| t.as_array().is_some_and(Vec::is_empty));
        if untagged && matches!(p.client.settings.auth, crate::config::Auth::OAuth { .. }) {
            return Err(Error::Refused(
                "tags is empty: a key an OAuth client makes must carry a tag the client may \
                 apply (`tags = [\"tag:k8s\"]`)"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_time_is_seconds_since_the_epoch() {
        assert_eq!(seconds_of("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(seconds_of("2026-10-08T09:00:00Z"), Some(1791450000));
        assert_eq!(seconds_of("2026-10-08T09:00:00.123456Z"), Some(1791450000));
        assert_eq!(seconds_of("soon"), None);
    }

    #[test]
    fn a_key_as_the_api_answers_it() {
        let k = from_api(&json!({
            "id": "kAbC", "key": "tskey-auth-kAbC-secret", "description": "k3s",
            "created": "2026-10-08T09:00:00Z", "expires": "2026-10-15T09:00:00Z",
            "capabilities": {"devices": {"create": {
                "reusable": true, "ephemeral": true, "preauthorized": true, "tags": ["tag:k8s"],
            }}},
        }));
        assert_eq!(k.expiry, Some(7 * 86400));
        assert_eq!(k.tags, vec!["tag:k8s".to_string()]);
        assert_eq!(
            (k.reusable, k.ephemeral, k.preauthorized),
            (Some(true), Some(true), Some(true))
        );
        assert_eq!(k.key.as_deref(), Some(""), "the value is never read back");
    }
}
