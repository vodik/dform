//! A client of the OVH API: each call signed by the host with the keys it
//! holds (a `signed` credential: the provider's signer signs, apart from
//! it; the provider names it and never sees the keys), or
//! carrying the program's token the host holds, or a bearer token minted
//! from a service account's client credentials (R-179), or one given as
//! it is (`access_token`), and sent through the host's HTTP
//! (`dform_sdk::host().http`), so dform pools its connection, paces it
//! with the provider's other requests and traces it under the call it is
//! made for. Each request names the class of endpoint it is (`bucket`):
//! the host paces each class's window by what the API answers it.
//!
//! A bearer token is minted with OAuth2's client-credentials grant
//! (`grant_type=client_credentials`, `scope=all`, the client id and
//! secret as Basic auth) at the endpoint's token URL, kept until a minute
//! before its `expires_in`, and minted afresh once when a call is refused
//! with a 401.
//!
//! A failure names the method, the path and the HTTP status (`HTTP 503`),
//! so dform's retry policy
//! (R-81, `plugin::policy::retryable`) tells a transient one (429, 5xx)
//! from a refusal; a call that reached no answer is `Unreachable`, which
//! may have taken effect.
//!
//! A list (`Client::list`: a region's flavors, the project's private
//! networks, a network's subnets) is asked once per client, a session's
//! run: the callers asking it at the same time await the one request in
//! flight, and every later one has its answer. A write to its class of endpoint
//! (`bucket`) forgets it, whatever the write's outcome, so the next ask
//! is answered as the API is after the write. A failure is not kept.

use crate::config::{Auth, Credentials};
use base64::Engine;
use dform_sdk::sync::Mutex as Held;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long before its expiry a bearer token is minted afresh.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// How a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The API answered with an error status: nothing was done (but for a
    /// 5xx, which the retry policy sends again).
    Status {
        method: String,
        path: String,
        status: u16,
        message: String,
    },
    /// No answer came: the call may or may not have taken effect.
    Unreachable {
        method: String,
        path: String,
        why: String,
    },
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            Error::Unreachable { .. } => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Status {
                method,
                path,
                status,
                message,
            } => {
                write!(f, "OVH API {method} {path}: HTTP {status}")?;
                if !message.is_empty() {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
            Error::Unreachable { method, path, why } => {
                write!(f, "OVH API {method} {path}: no answer: {why}")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Client {
    creds: Credentials,
    /// The bearer token minted from a service account's credentials, and
    /// when it is to be minted afresh: held while one is minted.
    token: Held<Option<(String, Instant)>>,
    /// The lists asked once.
    kept: Mutex<Kept>,
    /// The credential the host holds and applies (`Auth::Held`,
    /// `Auth::Signed`), opened.
    held: Option<dform_sdk::Credential>,
}

/// The lists a client asked (`Client::list`), by path, and how many
/// writes each class of endpoint has had: an answer asked while one was
/// sent may predate it, and is not kept.
#[derive(Default)]
struct Kept {
    lists: BTreeMap<String, Slot>,
    writes: BTreeMap<String, u64>,
}

impl Kept {
    /// The writes to `path`'s class of endpoint so far.
    fn writes(&self, path: &str) -> u64 {
        self.writes.get(bucket(path)).copied().unwrap_or(0)
    }

    /// A write to `path`'s class: its lists forgotten.
    fn wrote(&mut self, path: &str) {
        let class = bucket(path);
        *self.writes.entry(class.to_string()).or_default() += 1;
        self.lists.retain(|p, _| bucket(p) != class);
    }
}

/// One list: not asked yet, or its answer, `None` where the API has none
/// (a 404). Its lock is held while it is asked, so a second asker awaits
/// the first's request.
type Slot = Arc<Held<Option<Option<Arc<Json>>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How one call is authenticated.
#[derive(Clone, Copy)]
enum By<'a> {
    /// Not at all: no credential was opened.
    Nothing,
    Bearer(&'a str),
    /// The host applies the credential it holds: signs the call, or sets
    /// the token.
    Held(&'a dform_sdk::Credential),
}

/// What the provider keeps of an account from one run to the next, in
/// the host's cache (`dform:host/cache`, the account's: the host keeps
/// one account's apart from another's): the id of a project named by its
/// description (`project ENDPOINT NAME`), the consumer key's expiry
/// (`key ENDPOINT`). A miss asks the API.
pub fn kept(key: &str) -> Option<String> {
    dform_sdk::host()
        .cache
        .get_text(dform_sdk::Scope::Account, key)
}

/// Keep `value` at `key` ([`kept`]), unless it is kept already.
pub fn keep(key: &str, value: &str) {
    if kept(key).as_deref() != Some(value) {
        dform_sdk::host()
            .cache
            .put(dform_sdk::Scope::Account, key, value);
    }
}

impl Client {
    pub fn new(creds: Credentials) -> Client {
        Client {
            creds,
            token: Held::new(None),
            kept: Mutex::default(),
            held: None,
        }
    }

    /// The client of `creds`, applying what the host holds for it (the
    /// program's token, the keys it signs with, `Auth::Held` and
    /// `Auth::Signed`), opened by its name: refused, saying why, when the
    /// host does not open it (dform.toml does not grant it).
    pub fn opened(creds: Credentials) -> std::result::Result<Client, String> {
        let held = match &creds.auth {
            Auth::Held { name } | Auth::Signed { name } => Some(
                dform_sdk::host()
                    .secrets
                    .open(name)
                    .map_err(|e| e.message)?,
            ),
            Auth::OAuth2 { .. } | Auth::Token { .. } => None,
        };
        let mut client = Client::new(creds);
        client.held = held;
        Ok(client)
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    pub async fn get(&self, path: &str) -> Result<Json> {
        self.send("GET", path, None).await
    }

    /// GET, `None` for a 404.
    pub async fn get_opt(&self, path: &str) -> Result<Option<Json>> {
        match self.get(path).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The list at `path`, asked once (the module's doc).
    pub async fn list(&self, path: &str) -> Result<Arc<Json>> {
        let asked = self
            .remember(path, async || self.get(path).await.map(Some))
            .await?;
        Ok(asked.unwrap_or_default())
    }

    /// [`Client::list`], `None` for a 404 (kept as the answer).
    pub async fn list_opt(&self, path: &str) -> Result<Option<Arc<Json>>> {
        self.remember(path, async || self.get_opt(path).await).await
    }

    /// The answer kept for `path`, else `ask`'s, kept unless it failed or
    /// a write to its class was sent while it was asked.
    async fn remember(
        &self,
        path: &str,
        ask: impl AsyncFnOnce() -> Result<Option<Json>>,
    ) -> Result<Option<Arc<Json>>> {
        let (slot, writes) = {
            let mut kept = lock(&self.kept);
            let slot = kept.lists.entry(path.to_string()).or_default().clone();
            (slot, kept.writes(path))
        };
        let mut held = slot.lock().await;
        if let Some(answer) = &*held {
            return Ok(answer.clone());
        }
        let answer = ask().await?.map(Arc::new);
        if lock(&self.kept).writes(path) == writes {
            *held = Some(answer.clone());
        }
        Ok(answer)
    }

    pub async fn post(&self, path: &str, body: Option<&Json>) -> Result<Json> {
        self.write("POST", path, body).await
    }

    pub async fn put(&self, path: &str, body: &Json) -> Result<Json> {
        self.write("PUT", path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Json> {
        self.write("DELETE", path, None).await
    }

    /// A write: the lists of its class forgotten as it is sent and once
    /// it is answered (or not), so none asked meanwhile is kept.
    async fn write(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json> {
        lock(&self.kept).wrote(path);
        let out = self.send(method, path, body).await;
        lock(&self.kept).wrote(path);
        out
    }

    /// One call: `path` under the endpoint's URL (it carries its query).
    /// A bearer token minted from a service account and refused (a 401) is
    /// minted afresh and the call sent once more.
    async fn send(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json> {
        if let Some(c) = &self.held {
            return self.send_once(method, path, body, By::Held(c)).await;
        }
        match &self.creds.auth {
            Auth::Token { access_token } => {
                self.send_once(method, path, body, By::Bearer(access_token))
                    .await
            }
            Auth::OAuth2 { .. } => {
                let token = self.bearer().await?;
                let out = self.send_once(method, path, body, By::Bearer(&token)).await;
                if !matches!(&out, Err(e) if e.status() == Some(401)) {
                    return out;
                }
                self.forget(&token).await;
                let token = self.bearer().await?;
                self.send_once(method, path, body, By::Bearer(&token)).await
            }
            Auth::Held { .. } | Auth::Signed { .. } => {
                self.send_once(method, path, body, By::Nothing).await
            }
        }
    }

    /// The bearer token: the one held while it is good, else one minted.
    async fn bearer(&self) -> Result<String> {
        let mut held = self.token.lock().await;
        if let Some((token, until)) = &*held
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let (token, life) = self.mint().await?;
        *held = Some((
            token.clone(),
            Instant::now() + life.saturating_sub(TOKEN_MARGIN),
        ));
        Ok(token)
    }

    /// Drop `token`, refused, unless another call has minted its
    /// successor already.
    async fn forget(&self, token: &str) {
        let mut held = self.token.lock().await;
        if held.as_ref().is_some_and(|(t, _)| t == token) {
            *held = None;
        }
    }

    /// A bearer token from the service account's client credentials,
    /// and how long it is good for.
    async fn mint(&self) -> Result<(String, Duration)> {
        let Auth::OAuth2 {
            client_id,
            client_secret,
            token_url,
        } = &self.creds.auth
        else {
            unreachable!("a token is minted for a service account only");
        };
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{client_id}:{client_secret}"));
        let headers = [
            ("Authorization", format!("Basic {basic}")),
            (
                "Content-Type",
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept", "application/json".to_string()),
        ];
        let body = "grant_type=client_credentials&scope=all";
        let (status, text) = self
            .exchange("POST", token_url, token_url, &headers, Some(body), None)
            .await?;
        let answer = serde_json::from_str::<Json>(&text).unwrap_or(Json::Null);
        let refused = |message: String| Error::Status {
            method: "POST".into(),
            path: token_url.clone(),
            status,
            message,
        };
        if !(200..300).contains(&status) {
            let why = ["error_description", "error", "message"]
                .iter()
                .find_map(|k| answer.get(*k).and_then(Json::as_str))
                .unwrap_or("no reason given");
            return Err(refused(format!(
                "the service account {client_id} was refused a token for {}: {why}",
                self.creds.endpoint
            )));
        }
        let Some(token) = answer.get("access_token").and_then(Json::as_str) else {
            return Err(refused("the answer holds no access_token".into()));
        };
        let life = answer.get("expires_in").and_then(Json::as_u64).unwrap_or(0);
        Ok((token.to_string(), Duration::from_secs(life)))
    }

    /// One HTTP request to `url` (`path` names it in an error): its
    /// status and its answer's text.
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&str>,
        auth: Option<&dform_sdk::Credential>,
    ) -> Result<(u16, String)> {
        let unreachable = |why: String| Error::Unreachable {
            method: method.into(),
            path: path.into(),
            why,
        };
        let mut req = dform_sdk::Request::new(method, url)
            .timeout(REQUEST_TIMEOUT)
            .bucket(bucket(path));
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        if let Some(c) = auth {
            req = req.auth(c);
        }
        let resp = dform_sdk::host()
            .http
            .request(req)
            .await
            .map_err(|e| unreachable(e.message))?;
        let text = String::from_utf8(resp.body)
            .map_err(|e| unreachable(format!("reading the answer: {e}")))?;
        Ok((resp.status, text))
    }

    async fn send_once(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
        by: By<'_>,
    ) -> Result<Json> {
        let url = format!("{}{path}", self.creds.url);
        let body = body.map(Json::to_string);
        let mut headers = Vec::new();
        if body.is_some() {
            headers.push(("Content-Type", "application/json".to_string()));
        }
        if let By::Bearer(token) = by {
            headers.push(("Authorization", format!("Bearer {token}")));
        }
        let held = match by {
            By::Held(c) => Some(c),
            _ => None,
        };
        let (status, text) = self
            .exchange(method, path, &url, &headers, body.as_deref(), held)
            .await?;
        if !(200..300).contains(&status) {
            return Err(Error::Status {
                method: method.into(),
                path: path.into(),
                status,
                message: self.refusal(status, &text),
            });
        }
        if text.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&text).map_err(|e| Error::Status {
            method: method.into(),
            path: path.into(),
            status,
            message: format!("the answer is not JSON: {e}"),
        })
    }

    /// What a refusal (`status`, the answer `text`) says. OVH's
    /// `errorCode` says which kind: INVALID_CREDENTIAL (the consumer key,
    /// expired or revoked: said as such, with the fix), NOT_GRANTED_CALL
    /// (its rights), ...
    fn refusal(&self, status: u16, text: &str) -> String {
        // `{"class": "Client::NotFound", "message": "..."}`
        let Ok(j) = serde_json::from_str::<Json>(text) else {
            return text.trim().chars().take(200).collect();
        };
        let msg = j.get("message").and_then(Json::as_str).unwrap_or("");
        let code = j.get("errorCode").and_then(Json::as_str).unwrap_or("");
        let invalid = code == "INVALID_CREDENTIAL"
            || msg.contains("credential is not valid")
            || msg.contains("credential does not exist");
        if status == 403 && invalid && matches!(self.creds.auth, Auth::Signed { .. }) {
            return self.creds.expired_key();
        }
        if status == 401 && matches!(self.creds.auth, Auth::Token { .. }) {
            return self.creds.expired_token();
        }
        if let (401, Auth::Held { name }) = (status, &self.creds.auth) {
            return self.creds.refused_held(name);
        }
        match (msg, code) {
            ("", _) => text.trim().chars().take(200).collect(),
            (msg, "") => msg.to_string(),
            (msg, code) => format!("{msg} ({code})"),
        }
    }
}

/// The class of endpoint the request to `path` is, as the host's window
/// for it is named: a project's kind of object (`instance`, `network`,
/// `volume`, `sshkey`, `flavor`, ..), `project` for the projects
/// themselves, else the API's first segment (`domain`, `auth`, `me`); a
/// token URL is `auth`.
pub fn bucket(path: &str) -> &str {
    if path.contains("://") {
        return "auth";
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segments = path.trim_start_matches('/').split('/');
    match (segments.next(), segments.next(), segments.nth(1)) {
        (Some("cloud"), Some("project"), Some(kind)) if !kind.is_empty() => kind,
        (Some("cloud"), Some("project"), _) => "project",
        (Some(first), _, _) if !first.is_empty() => first,
        _ => "api",
    }
}

/// `s` as a URL path segment or query value.
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_names_the_call_so_the_retry_policy_can_read_it() {
        let e = Error::Status {
            method: "POST".into(),
            path: "/cloud/project/p/instance".into(),
            status: 503,
            message: "Service Unavailable".into(),
        };
        let m = e.to_string();
        assert_eq!(
            m,
            "OVH API POST /cloud/project/p/instance: HTTP 503: Service Unavailable"
        );
        assert!(dform_core::plugin::policy::retryable(&m));
        let refused = Error::Status {
            method: "POST".into(),
            path: "/x".into(),
            status: 400,
            message: "bad".into(),
        };
        assert!(!dform_core::plugin::policy::retryable(&refused.to_string()));
    }

    /// A request's window is its class of endpoint's.
    #[test]
    fn a_request_is_paced_by_its_class_of_endpoint() {
        for (path, class) in [
            ("/cloud/project/p/instance/i-1/interface", "instance"),
            ("/cloud/project/p/instance?region=BHS5", "instance"),
            ("/cloud/project/p/network/private/pn-1/subnet", "network"),
            ("/cloud/project/p/flavor?region=BHS5", "flavor"),
            ("/cloud/project/p", "project"),
            ("/cloud/project", "project"),
            ("/domain/zone/example.com/record?subDomain=www", "domain"),
            ("/auth/time", "auth"),
            ("https://www.ovh.com/auth/oauth2/token", "auth"),
        ] {
            assert_eq!(bucket(path), class, "{path}");
        }
    }

    #[test]
    fn escapes_a_segment() {
        assert_eq!(escape("Ubuntu 24.04"), "Ubuntu%2024.04");
        assert_eq!(escape("ca-east-tor"), "ca-east-tor");
    }
}
