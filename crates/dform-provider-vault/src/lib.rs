//! The Vault provider, `dform-provider-vault` (R-172): a secret manager as
//! a location dform reads, for a secret dform does not own (typed by a
//! human, issued by another team, shared with systems dform does not
//! run). It manages nothing: it declares the scheme `vault` and reads a
//! KV version 2 secret, `vault://MOUNT/PATH#KEY`, answering its bytes and
//! the version Vault names them by. dform records the version in the plan
//! file beside the secret's keyed digest, so `apply PLAN` refuses a secret
//! rotated in Vault since the plan.
//!
//! Written with the SDK (`dform-sdk`): every request goes through the
//! host's HTTP client, so TLS, the proxy and the token are the host's. A
//! token is a credential by name (`token = "bearer:vault"`), applied by
//! the host and never seen here; AppRole logs in with a role id and a
//! secret id the program gives (a secret, revealed into Configure), and
//! keeps the token it is answered in memory only.
//!
//! `location` reads a location, `fake` (feature `fake`) is a server for
//! tests.

#[cfg(feature = "fake")]
pub mod fake;
pub mod location;

use dform_sdk::typed::{Error, Result};
use dform_sdk::{Document, Failure, Provider, Request, Typed};
use location::Location;
use serde_json::{Value as Json, json};
use std::sync::Mutex;

/// The scheme it reads.
pub const SCHEME: &str = "vault";

/// The credential a token is, unless the program names another.
pub const TOKEN: &str = "bearer:vault";

/// How the provider proves who it is to Vault.
pub enum Auth {
    /// A token, the credential named (`bearer:NAME`, or `header:NAME`
    /// holding `X-Vault-Token: ..`): the host applies it.
    Token(String),
    /// AppRole (`auth/MOUNT/login`): the token its login answers, kept
    /// here until Vault refuses it.
    AppRole {
        mount: String,
        role_id: String,
        secret_id: String,
        token: Mutex<Option<String>>,
    },
}

/// The provider: one Vault server, as one identity.
pub struct Vault {
    /// `https://vault.example:8200`, no trailing slash.
    pub address: String,
    /// Vault Enterprise's namespace (`X-Vault-Namespace`), when set.
    pub namespace: Option<String>,
    pub auth: Auth,
}

fn string(settings: &Json, key: &str) -> Result<Option<String>> {
    match settings.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(Error::Refused(format!(
            "use vault: {key} is a string, not {v}"
        ))),
    }
}

impl Vault {
    /// From the provider block's settings, else (no `address`) Vault's own
    /// environment variable `VAULT_ADDR`.
    pub fn parse(settings: &Json, env: &dyn Fn(&str) -> Option<String>) -> Result<Vault> {
        if let Json::Object(m) = settings {
            for k in m.keys() {
                if !["address", "namespace", "token", "approle"].contains(&k.as_str()) {
                    return Err(Error::Refused(format!(
                        "use vault: no setting {k:?}: the settings are address, namespace, \
                         token (a credential's name) and approle"
                    )));
                }
            }
        }
        let address = string(settings, "address")?
            .or_else(|| env("VAULT_ADDR"))
            .ok_or_else(|| {
                Error::Refused(
                    "use vault: no address: write `address = \"https://vault.example:8200\"` \
                     (or set VAULT_ADDR)"
                        .into(),
                )
            })?;
        let address = address.trim_end_matches('/').to_string();
        if !address.starts_with("https://") && !address.starts_with("http://") {
            return Err(Error::Refused(format!(
                "use vault: address {address:?} is no URL: write `https://HOST:PORT`"
            )));
        }
        let namespace = string(settings, "namespace")?;
        let auth = match settings.get("approle") {
            None | Some(Json::Null) => {
                Auth::Token(string(settings, "token")?.unwrap_or_else(|| TOKEN.to_string()))
            }
            Some(a) => {
                if settings.get("token").is_some_and(|t| !t.is_null()) {
                    return Err(Error::Refused(
                        "use vault: token and approle are two ways in: keep one".into(),
                    ));
                }
                let field = |k: &str| -> Result<String> {
                    string(a, k)?.ok_or_else(|| {
                        Error::Refused(format!(
                            "use vault: approle needs {k}: `approle = {{ role_id = \"..\", \
                             secret_id = vault_secret_id }}`"
                        ))
                    })
                };
                Auth::AppRole {
                    mount: string(a, "mount")?.unwrap_or_else(|| "approle".into()),
                    role_id: field("role_id")?,
                    secret_id: field("secret_id")?,
                    token: Mutex::new(None),
                }
            }
        };
        Ok(Vault {
            address,
            namespace,
            auth,
        })
    }

    /// A request to Vault's API at `path` (`/v1/..`), its namespace set.
    fn request(&self, method: &str, path: &str) -> Request {
        let r = Request::new(method, format!("{}{path}", self.address));
        match &self.namespace {
            Some(n) => r.header("X-Vault-Namespace", n),
            None => r,
        }
    }

    /// Send `r` as the provider's identity: the token credential applied
    /// by the host, or the AppRole token, logging in first (and again once
    /// when Vault refuses a token it has let lapse).
    fn send(&self, r: impl Fn() -> Request) -> std::result::Result<dform_sdk::Response, Failure> {
        let host = dform_sdk::host();
        match &self.auth {
            Auth::Token(name) => {
                let c = host.secrets.open(name)?;
                Ok(host.http.request(r().auth(&c))?)
            }
            Auth::AppRole { token, .. } => {
                for fresh in [false, true] {
                    let t = self.approle_token(fresh)?;
                    let resp = host.http.request(r().header("X-Vault-Token", &t))?;
                    if resp.status != 403 || fresh {
                        return Ok(resp);
                    }
                    *token.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                unreachable!("the second try returns")
            }
        }
    }

    /// The AppRole token: the one kept, else a login's.
    fn approle_token(&self, fresh: bool) -> std::result::Result<String, Failure> {
        let Auth::AppRole {
            mount,
            role_id,
            secret_id,
            token,
        } = &self.auth
        else {
            unreachable!("approle_token of a token's provider")
        };
        let mut kept = token.lock().unwrap_or_else(|e| e.into_inner());
        if let (false, Some(t)) = (fresh, kept.as_ref()) {
            return Ok(t.clone());
        }
        let resp = dform_sdk::host().http.request(
            self.request("POST", &format!("/v1/auth/{mount}/login"))
                .json(&json!({ "role_id": role_id, "secret_id": secret_id })),
        )?;
        if resp.status != 200 {
            return Err(dform_sdk::Error::fatal(format!(
                "vault {}: the AppRole login at auth/{mount} was refused ({}): {}",
                self.address,
                resp.status,
                errors(&resp.body)
            ))
            .into());
        }
        let body: Json = serde_json::from_slice(&resp.body).unwrap_or_default();
        let t = body["auth"]["client_token"]
            .as_str()
            .ok_or_else(|| {
                dform_sdk::Error::fatal(format!(
                    "vault {}: the AppRole login answered no auth.client_token",
                    self.address
                ))
            })?
            .to_string();
        *kept = Some(t.clone());
        Ok(t)
    }

    /// The secret at `location`, with its version.
    pub fn read_secret(&self, location: &str) -> std::result::Result<Document, Failure> {
        let fatal = |m: String| -> Failure { dform_sdk::Error::fatal(m).into() };
        let at = Location::parse(location).map_err(fatal)?;
        let path = at.api_path();
        let resp = self.send(|| self.request("GET", &path))?;
        let body: Json = serde_json::from_slice(&resp.body).unwrap_or_default();
        match resp.status {
            200 => {}
            404 => {
                // Vault answers a deleted or destroyed version with its
                // metadata and a 404.
                let meta = &body["data"]["metadata"];
                if meta["destroyed"] == json!(true) {
                    return Err(fatal(format!(
                        "version {} was destroyed in Vault",
                        meta["version"]
                    )));
                }
                if meta["deletion_time"]
                    .as_str()
                    .is_some_and(|t| !t.is_empty())
                {
                    return Err(fatal(format!(
                        "version {} was deleted in Vault (`vault kv undelete` restores it)",
                        meta["version"]
                    )));
                }
                return Err(Failure::NotYet(format!(
                    "Vault has no secret at {}/{} yet",
                    at.mount, at.path
                )));
            }
            403 => {
                return Err(fatal(format!(
                    "Vault refused the read of {path} (403): the token's policy needs `read` \
                     on {}/data/{}",
                    at.mount, at.path
                )));
            }
            s if s == 429 || s >= 500 => {
                return Err(dform_sdk::Error::retryable(format!(
                    "Vault answered {s}: {}",
                    errors(&resp.body)
                ))
                .into());
            }
            s => {
                return Err(fatal(format!("Vault answered {s}: {}", errors(&resp.body))));
            }
        }
        let data = &body["data"]["data"];
        let version = match &body["data"]["metadata"]["version"] {
            Json::Number(n) => Some(n.to_string()),
            _ => None,
        };
        let bytes = match &at.key {
            None => serde_json::to_vec(data).unwrap_or_default(),
            Some(k) => match data.get(k) {
                Some(Json::String(s)) => s.clone().into_bytes(),
                Some(v) => serde_json::to_vec(v).unwrap_or_default(),
                None => {
                    let mut keys: Vec<&str> = data
                        .as_object()
                        .map(|m| m.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    keys.sort();
                    return Err(fatal(format!(
                        "the secret {}/{} has no key {k:?}: its keys are {}",
                        at.mount,
                        at.path,
                        keys.join(", ")
                    )));
                }
            },
        };
        Ok(Document { bytes, version })
    }
}

/// Vault's `errors` in a refusal's body, joined.
fn errors(body: &[u8]) -> String {
    let v: Json = serde_json::from_slice(body).unwrap_or_default();
    match v["errors"].as_array() {
        Some(es) if !es.is_empty() => es
            .iter()
            .map(|e| {
                e.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string())
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::from_utf8_lossy(body).trim().to_string(),
    }
}

impl Provider for Vault {
    const NAME: &'static str = "vault";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const SCHEMES: &'static [&'static str] = &[SCHEME];

    fn configure(settings: &Json) -> Result<(Vault, Option<String>)> {
        let v = Vault::parse(settings, &|k| std::env::var(k).ok())?;
        let account = v.address.clone();
        Ok((v, Some(account)))
    }

    fn read(&self, location: &str) -> std::result::Result<Document, Failure> {
        self.read_secret(location)
    }
}

/// The provider: no resource types, the scheme alone.
pub fn provider() -> Typed<Vault> {
    Typed::new()
}

dform_sdk::provider!(
    provider(),
    uses = ["dform:host/http", "dform:host/secrets", "dform:host/log"]
);

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(r: Result<Vault>) -> String {
        match r {
            Ok(_) => panic!("configured"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn settings_are_checked() {
        let env = |_: &str| None;
        let v = Vault::parse(&json!({"address": "https://v.example:8200/"}), &env).unwrap();
        assert_eq!(v.address, "https://v.example:8200");
        assert!(matches!(&v.auth, Auth::Token(t) if t == TOKEN));
        let e = refused(Vault::parse(&json!({}), &env));
        assert!(e.contains("no address"), "{e}");
        let v = Vault::parse(&json!({}), &|_| Some("http://127.0.0.1:8200".into())).unwrap();
        assert_eq!(v.address, "http://127.0.0.1:8200");
        let e = refused(Vault::parse(
            &json!({"address": "https://v", "tokn": "x"}),
            &env,
        ));
        assert!(e.contains("no setting \"tokn\""), "{e}");
        let e = refused(Vault::parse(
            &json!({"address": "https://v", "approle": {"role_id": "r"}}),
            &env,
        ));
        assert!(e.contains("approle needs secret_id"), "{e}");
    }
}
