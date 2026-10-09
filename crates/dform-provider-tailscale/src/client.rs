//! The Tailscale API v2, through the host's HTTP client: a request's
//! credential applied (an OAuth client's token, minted here and kept for
//! the run; an API access token; a credential by name the host applies),
//! and its answer read as the API's JSON, a refusal as the API's
//! `message`.

use crate::config::{Auth, Settings};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Request, host};
use serde_json::{Value as Json, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A request's body.
pub enum Body {
    None,
    Json(Json),
    /// The policy file, as the API takes it.
    HuJson(String),
}

/// An answer: its status, its body (`null` for none) and its `ETag`.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Json,
    pub etag: Option<String>,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub struct Client {
    pub settings: Settings,
    /// An OAuth client's token and when it lapses.
    token: Mutex<Option<(String, Instant)>>,
}

/// `s` as a form value (`application/x-www-form-urlencoded`).
fn form(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The OAuth scope a refused request needs, by its path (Tailscale's
/// names for them).
fn scope(path: &str) -> &'static str {
    if path.contains("/acl") {
        "policy_file"
    } else if path.contains("/keys") {
        "auth_keys"
    } else if path.contains("/dns") {
        "dns"
    } else if path.contains("/routes") {
        "devices:routes"
    } else if path.contains("/users") {
        "users:read"
    } else {
        "devices:core"
    }
}

/// The API's `message` in a refusal's body, else the body.
pub fn message(body: &Json) -> String {
    match body.get("message").and_then(Json::as_str) {
        Some(m) => m.to_string(),
        None => body.to_string(),
    }
}

impl Client {
    pub fn new(settings: Settings) -> Client {
        Client {
            settings,
            token: Mutex::new(None),
        }
    }

    pub fn tailnet(&self) -> &str {
        &self.settings.tailnet
    }

    /// `/tailnet/{tailnet}{rest}`.
    pub fn tailnet_path(&self, rest: &str) -> String {
        format!("/tailnet/{}{rest}", self.settings.tailnet)
    }

    /// The OAuth client's token: the one kept while it is good, else one
    /// minted (`fresh`: always minted, the kept one refused).
    fn oauth_token(&self, id: &str, secret: &str, fresh: bool) -> Result<String> {
        let mut kept = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if let (false, Some((t, until))) = (fresh, kept.as_ref())
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let url = format!("{}/api/v2/oauth/token", self.settings.base_url);
        let r = host().http.request(
            Request::post(url)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(format!(
                    "client_id={}&client_secret={}&grant_type=client_credentials",
                    form(id),
                    form(secret)
                )),
        )?;
        let body: Json = serde_json::from_slice(&r.body).unwrap_or(Json::Null);
        if r.status != 200 {
            return Err(Error::Refused(format!(
                "tailscale: the OAuth client {id} was refused a token ({}): {}",
                r.status,
                message(&body)
            )));
        }
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| {
                Error::Refused("tailscale: the OAuth answer has no access_token".into())
            })?
            .to_string();
        // A minute before it lapses.
        let life = body["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(60);
        *kept = Some((token.clone(), Instant::now() + Duration::from_secs(life)));
        Ok(token)
    }

    /// `method path` with `body`, the credential applied; an OAuth token
    /// the API refuses is minted again, once. Any status the API answers
    /// is an answer; a 429 or a 5xx is a retryable refusal, a 401 or a
    /// 403 a refusal naming the credential and the scope. The policy
    /// file (`hujson`) is asked for as HuJSON, its body answered as text.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        body: &Body,
        if_match: Option<&str>,
        hujson: bool,
    ) -> Result<Answer> {
        let url = format!("{}/api/v2{path}", self.settings.base_url);
        let accept = match hujson {
            true => "application/hujson",
            false => "application/json",
        };
        let build = || {
            let r = Request::new(method, url.clone()).header("accept", accept);
            let r = match if_match {
                Some(e) => r.header("if-match", e),
                None => r,
            };
            match body {
                Body::None => r,
                Body::Json(j) => r.json(j),
                Body::HuJson(t) => r
                    .header("content-type", "application/hujson")
                    .body(t.clone().into_bytes()),
            }
        };
        let at = format!("tailscale {method} {path}");
        let mut answer = None;
        for fresh in [false, true] {
            let r = match &self.settings.auth {
                Auth::OAuth { id, secret } => {
                    let t = self.oauth_token(id, secret, fresh)?;
                    build().header("authorization", &format!("Bearer {t}"))
                }
                Auth::ApiKey(k) => build().header("authorization", &format!("Bearer {k}")),
                Auth::Credential(name) => build().auth(&host().secrets.open(name)?),
            };
            let r = host().http.request(r).map_err(|e| Error::from(e).at(&at))?;
            let oauth = matches!(self.settings.auth, Auth::OAuth { .. });
            if r.status == 401 && oauth && !fresh {
                continue;
            }
            answer = Some(r);
            break;
        }
        let r = answer.expect("the second try answers");
        let text = String::from_utf8_lossy(&r.body).to_string();
        let body: Json = match (r.body.is_empty(), hujson && (200..300).contains(&r.status)) {
            (true, _) => Json::Null,
            (false, true) => json!(text),
            (false, false) => serde_json::from_str(&text).unwrap_or_else(|_| json!(text)),
        };
        let etag = r.header("etag").map(str::to_string);
        match r.status {
            401 => Err(Error::Refused(format!(
                "{at}: {} was refused (401): {}",
                self.settings.auth.describe(),
                message(&body)
            ))),
            403 => Err(Error::Refused(format!(
                "{at}: refused (403): {}: the OAuth client needs the scope `{}` (or the token \
                 its rights)",
                message(&body),
                scope(path)
            ))),
            s if s == 429 || s >= 500 => Err(Error::Refused(format!(
                "retryable: {at}: the API answered {s}: {}",
                message(&body)
            ))),
            status => Ok(Answer { status, body, etag }),
        }
    }

    /// `GET path`: `None` for a 404.
    pub fn get(&self, path: &str) -> Result<Option<Answer>> {
        let a = self.send("GET", path, &Body::None, None, false)?;
        match a.status {
            404 => Ok(None),
            _ => self.expect("GET", path, a).map(Some),
        }
    }

    /// `method path` with `body`, which must succeed.
    pub fn call(&self, method: &str, path: &str, body: Body) -> Result<Answer> {
        let a = self.send(method, path, &body, None, false)?;
        self.expect(method, path, a)
    }

    pub fn expect(&self, method: &str, path: &str, a: Answer) -> Result<Answer> {
        match a.ok() {
            true => Ok(a),
            false => Err(Error::Refused(format!(
                "tailscale {method} {path}: the API answered {}: {}",
                a.status,
                message(&a.body)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_form_value_is_escaped() {
        assert_eq!(
            super::form("tskey-client-k/1+2=3"),
            "tskey-client-k%2F1%2B2%3D3"
        );
    }
}
