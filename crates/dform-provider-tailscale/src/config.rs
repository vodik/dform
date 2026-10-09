//! How the provider is configured: `use tailscale { tailnet = ".." }`, and
//! its credential, read by Tailscale's own names.
//!
//! The tailnet is the program's (`tailnet`, else `TAILSCALE_TAILNET`); it
//! is the account the provider reports, so `expect_account` holds a
//! deployment to it. The credential is one of three, never two:
//!
//! - an OAuth client, `TAILSCALE_OAUTH_CLIENT_ID` and
//!   `TAILSCALE_OAUTH_CLIENT_SECRET` (what Tailscale's Terraform provider
//!   reads): the provider mints a token from it, and its scopes are what
//!   the provider may do;
//! - an API access token, `TAILSCALE_API_KEY`;
//! - a credential by name, `credential = "bearer:tailscale"`, applied by
//!   the host: its value never reaches the provider.
//!
//! `base_url` (else `TAILSCALE_BASE_URL`) is the API's address,
//! `https://api.tailscale.com` unless a test's fake says otherwise.

use dform_sdk::typed::{Error, Result};
use serde_json::Value as Json;

pub const BASE_URL: &str = "https://api.tailscale.com";

/// The settings a `use tailscale { .. }` block takes.
pub const SETTINGS: [&str; 3] = ["tailnet", "credential", "base_url"];

/// How the provider proves who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// An OAuth client: a token minted from it with the client credentials
    /// grant, kept for the run.
    OAuth { id: String, secret: String },
    /// An API access token (`tskey-api-..`), sent as a bearer token.
    ApiKey(String),
    /// A credential by name the host applies (`bearer:tailscale`).
    Credential(String),
}

impl Auth {
    /// Which it is, for a message: never the value.
    pub fn describe(&self) -> String {
        match self {
            Auth::OAuth { .. } => {
                "the OAuth client in TAILSCALE_OAUTH_CLIENT_ID and TAILSCALE_OAUTH_CLIENT_SECRET"
                    .into()
            }
            Auth::ApiKey(_) => "the API access token in TAILSCALE_API_KEY".into(),
            Auth::Credential(n) => format!("the credential {n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub tailnet: String,
    pub base_url: String,
    pub auth: Auth,
}

fn string(settings: &Json, key: &str) -> Result<Option<String>> {
    match settings.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(Error::Refused(format!(
            "use tailscale: {key} is a string, not {v}"
        ))),
    }
}

impl Settings {
    /// From the block's settings and the environment (`env`).
    pub fn parse(settings: &Json, env: &dyn Fn(&str) -> Option<String>) -> Result<Settings> {
        if let Json::Object(m) = settings
            && let Some(k) = m.keys().find(|k| !SETTINGS.contains(&k.as_str()))
        {
            return Err(Error::Refused(format!(
                "use tailscale: no setting {k:?}: the settings are tailnet, credential (a \
                 credential's name) and base_url"
            )));
        }
        let env = |k: &str| env(k).filter(|v| !v.is_empty());
        let tailnet = string(settings, "tailnet")?
            .or_else(|| env("TAILSCALE_TAILNET"))
            .ok_or_else(|| {
                Error::Refused(
                    "use tailscale: no tailnet: write `use tailscale { tailnet = \"example.com\" }` \
                     (its name in the admin console's settings, or \"-\" for the credential's own; \
                     or set TAILSCALE_TAILNET)"
                        .into(),
                )
            })?;
        if tailnet.is_empty() || tailnet.contains(['/', '?', '#', ' ']) {
            return Err(Error::Refused(format!(
                "use tailscale: tailnet {tailnet:?} is no tailnet's name"
            )));
        }
        let base_url = string(settings, "base_url")?
            .or_else(|| env("TAILSCALE_BASE_URL"))
            .unwrap_or_else(|| BASE_URL.into())
            .trim_end_matches('/')
            .to_string();
        if !base_url.starts_with("https://") && !base_url.starts_with("http://") {
            return Err(Error::Refused(format!(
                "use tailscale: base_url {base_url:?} is no URL: write `https://HOST`"
            )));
        }
        let mut given = Vec::new();
        if let Some(n) = string(settings, "credential")? {
            given.push(Auth::Credential(n));
        }
        match (
            env("TAILSCALE_OAUTH_CLIENT_ID"),
            env("TAILSCALE_OAUTH_CLIENT_SECRET"),
        ) {
            (Some(id), Some(secret)) => given.push(Auth::OAuth { id, secret }),
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::Refused(
                    "use tailscale: an OAuth client is TAILSCALE_OAUTH_CLIENT_ID and \
                     TAILSCALE_OAUTH_CLIENT_SECRET together: one of them is not set"
                        .into(),
                ));
            }
        }
        if let Some(k) = env("TAILSCALE_API_KEY") {
            given.push(Auth::ApiKey(k));
        }
        let auth = match given.len() {
            1 => given.remove(0),
            0 => {
                return Err(Error::Refused(
                    "use tailscale: no credential: set TAILSCALE_OAUTH_CLIENT_ID and \
                     TAILSCALE_OAUTH_CLIENT_SECRET (an OAuth client, its scopes what the \
                     provider may do), or write `credential = \"bearer:tailscale\"` and grant it \
                     in dform.toml"
                        .into(),
                ));
            }
            _ => {
                let named: Vec<String> = given.iter().map(Auth::describe).collect();
                return Err(Error::Refused(format!(
                    "use tailscale: two credentials are given, {}: keep one",
                    named.join(" and ")
                )));
            }
        };
        Ok(Settings {
            tailnet,
            base_url,
            auth,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn refused(r: Result<Settings>) -> String {
        match r {
            Ok(s) => panic!("configured: {s:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn the_credential_is_read_by_tailscales_names() {
        let oauth = env(&[
            ("TAILSCALE_OAUTH_CLIENT_ID", "k123"),
            ("TAILSCALE_OAUTH_CLIENT_SECRET", "tskey-client-k123-s"),
        ]);
        let s = Settings::parse(&json!({"tailnet": "vodik.github"}), &oauth).unwrap();
        assert_eq!(s.tailnet, "vodik.github");
        assert_eq!(s.base_url, BASE_URL);
        assert!(matches!(s.auth, Auth::OAuth { ref id, .. } if id == "k123"));
        let s = Settings::parse(
            &json!({"credential": "bearer:tailscale"}),
            &env(&[("TAILSCALE_TAILNET", "-")]),
        )
        .unwrap();
        assert_eq!(s.tailnet, "-");
        assert_eq!(s.auth, Auth::Credential("bearer:tailscale".into()));
    }

    #[test]
    fn two_credentials_or_none_are_refused() {
        let both = env(&[
            ("TAILSCALE_OAUTH_CLIENT_ID", "k123"),
            ("TAILSCALE_OAUTH_CLIENT_SECRET", "s"),
            ("TAILSCALE_API_KEY", "tskey-api-x"),
        ]);
        let e = refused(Settings::parse(&json!({"tailnet": "t"}), &both));
        assert!(
            e.contains("two credentials are given, the OAuth client in")
                && e.contains("TAILSCALE_API_KEY"),
            "{e}"
        );
        assert!(!e.contains("tskey-api-x"), "{e}");
        let e = refused(Settings::parse(&json!({"tailnet": "t"}), &env(&[])));
        assert!(e.contains("no credential"), "{e}");
        let e = refused(Settings::parse(
            &json!({"tailnet": "t"}),
            &env(&[("TAILSCALE_OAUTH_CLIENT_ID", "k")]),
        ));
        assert!(e.contains("one of them is not set"), "{e}");
        let e = refused(Settings::parse(
            &json!({}),
            &env(&[("TAILSCALE_API_KEY", "k")]),
        ));
        assert!(e.contains("no tailnet"), "{e}");
        let e = refused(Settings::parse(
            &json!({"tailnet": "t", "tailent": "x"}),
            &env(&[("TAILSCALE_API_KEY", "k")]),
        ));
        assert!(e.contains("no setting \"tailent\""), "{e}");
    }
}
