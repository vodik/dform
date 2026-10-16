//! A client of the OVH API: each call signed (`sign`) with the three
//! keys, or carrying a bearer token minted from a service account's
//! client credentials (R-179), or one given as it is (`access_token`),
//! and sent through the host's HTTP (`dform_sdk::host().http`), so dform
//! pools its connection, paces it with the provider's other requests and
//! traces it under the call it is made for. The keys stay here: the
//! signature is over the request, which the host sends as it is. Each
//! request names the class of endpoint it is (`bucket`): the host paces
//! each class's window by what the API answers it.
//!
//! A bearer token is minted with OAuth2's client-credentials grant
//! (`grant_type=client_credentials`, `scope=all`, the client id and
//! secret as Basic auth) at the endpoint's token URL, kept until a minute
//! before its `expires_in`, and minted afresh once when a call is refused
//! with a 401.
//!
//! A signed call is signed with the server's clock: the difference to the
//! local one is asked once (`GET /auth/time`) and kept in dform's cache
//! beside the project ids (`ovh-projects.json`), so a run after the first
//! asks it again only when a signed call is refused (After R-123): the
//! clock moved, the offset is asked afresh and the call sent once more. A
//! failure names the method,
//! the path and the HTTP status (`HTTP 503`), so dform's retry policy
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
use crate::sign::signature;
use base64::Engine;
use dform_sdk::sync::Mutex as Held;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// Server time minus local time, in seconds, once known, and whether
    /// it was read from the cache rather than asked in this run.
    delta: Mutex<Option<(i64, bool)>>,
    /// The cache file the offset is kept in (`ovh-projects.json`).
    cache: Option<PathBuf>,
    /// The bearer token minted from a service account's credentials, and
    /// when it is to be minted afresh: held while one is minted.
    token: Held<Option<(String, Instant)>>,
    /// The lists asked once.
    kept: Mutex<Kept>,
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
    /// Not at all (`/auth/time`).
    Nothing,
    /// Signed with the three keys.
    Signature,
    Bearer(&'a str),
}

/// The file in dform's cache directory the project ids and the clock
/// offset are kept in.
pub const CACHE_FILE: &str = "ovh-projects.json";

/// The cache file's entries: `"ENDPOINT NAME"` to a project's id, and
/// `"ENDPOINT /auth/time"` to the clock offset in seconds.
pub fn read_cache(file: &std::path::Path) -> BTreeMap<String, String> {
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write `key = value` into the cache file, keeping its other entries.
pub fn write_cache(file: &std::path::Path, key: &str, value: &str) {
    let mut kept = read_cache(file);
    if kept.get(key).map(String::as_str) == Some(value) {
        return;
    }
    kept.insert(key.to_string(), value.to_string());
    if let Ok(bytes) = serde_json::to_vec_pretty(&kept) {
        let _ = file.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(file, bytes);
    }
}

impl Client {
    pub fn new(creds: Credentials) -> Client {
        Client {
            creds,
            delta: Mutex::new(None),
            cache: None,
            token: Held::new(None),
            kept: Mutex::default(),
        }
    }

    /// The client keeping its clock offset in dform's cache directory
    /// `dir` (`None`: asked every run).
    pub fn with_cache(mut self, dir: Option<&std::path::Path>) -> Client {
        self.cache = dir.map(|d| d.join(CACHE_FILE));
        self
    }

    /// The cache key of this endpoint's clock offset.
    fn clock_key(&self) -> String {
        format!("{} /auth/time", self.creds.endpoint)
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// The server's time now, from its clock's offset: known, kept in
    /// the cache, or asked.
    async fn timestamp(&self) -> Result<i64> {
        let known = *self.delta.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, _)) = known {
            return Ok(Self::now() + d);
        }
        let cached = self.cache.as_deref().and_then(|f| {
            read_cache(f)
                .get(&self.clock_key())
                .and_then(|v| v.parse::<i64>().ok())
        });
        let d = match cached {
            Some(d) => {
                let mut delta = self.delta.lock().unwrap_or_else(|e| e.into_inner());
                delta.get_or_insert((d, true)).0
            }
            None => self.ask_time().await?,
        };
        Ok(Self::now() + d)
    }

    /// Ask the server its time (`GET /auth/time`): the offset now known
    /// and kept in the cache.
    async fn ask_time(&self) -> Result<i64> {
        // Boxed: a call may ask the time, and asking it is a call.
        let server = Box::pin(self.send("GET", "/auth/time", None, false))
            .await?
            .as_i64()
            .ok_or_else(|| Error::Status {
                method: "GET".into(),
                path: "/auth/time".into(),
                status: 200,
                message: "the server's time is not a number".into(),
            })?;
        let d = server - Self::now();
        *self.delta.lock().unwrap_or_else(|e| e.into_inner()) = Some((d, false));
        if let Some(f) = &self.cache {
            write_cache(f, &self.clock_key(), &d.to_string());
        }
        Ok(d)
    }

    pub async fn get(&self, path: &str) -> Result<Json> {
        self.send("GET", path, None, true).await
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
        let out = self.send(method, path, body, true).await;
        lock(&self.kept).wrote(path);
        out
    }

    /// One call: `path` under the endpoint's URL (it carries its query),
    /// `authed` unless it is `/auth/time`. A bearer token refused (a 401)
    /// is minted afresh and the call sent once more. A signed call the
    /// server refuses while its timestamp rests on an offset read from
    /// the cache asks the server's time again, and is sent once more when
    /// the offset moved.
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
        authed: bool,
    ) -> Result<Json> {
        if let (true, Auth::Token { access_token }) = (authed, &self.creds.auth) {
            return self
                .send_once(method, path, body, By::Bearer(access_token))
                .await;
        }
        if authed && matches!(self.creds.auth, Auth::OAuth2 { .. }) {
            let token = self.bearer().await?;
            let out = self.send_once(method, path, body, By::Bearer(&token)).await;
            if !matches!(&out, Err(e) if e.status() == Some(401)) {
                return out;
            }
            self.forget(&token).await;
            let token = self.bearer().await?;
            return self.send_once(method, path, body, By::Bearer(&token)).await;
        }
        let by = if authed { By::Signature } else { By::Nothing };
        let out = self.send_once(method, path, body, by).await;
        let refused = matches!(&out, Err(e) if matches!(e.status(), Some(400 | 401 | 403)));
        let cached = *self.delta.lock().unwrap_or_else(|e| e.into_inner());
        match cached {
            Some((was, true)) if authed && refused => {
                if self.ask_time().await? == was {
                    return out;
                }
                self.send_once(method, path, body, by).await
            }
            _ => out,
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
            .exchange("POST", token_url, token_url, &headers, Some(body))
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
        match (&self.creds.auth, by) {
            (_, By::Bearer(token)) => headers.push(("Authorization", format!("Bearer {token}"))),
            (
                Auth::Keys {
                    application_key,
                    application_secret,
                    consumer_key,
                },
                by,
            ) => {
                headers.push(("X-Ovh-Application", application_key.clone()));
                if let By::Signature = by {
                    let ts = self.timestamp().await?;
                    headers.push(("X-Ovh-Timestamp", ts.to_string()));
                    headers.push(("X-Ovh-Consumer", consumer_key.clone()));
                    headers.push((
                        "X-Ovh-Signature",
                        signature(
                            application_secret,
                            consumer_key,
                            method,
                            &url,
                            body.as_deref().unwrap_or(""),
                            ts,
                        ),
                    ));
                }
            }
            (Auth::OAuth2 { .. } | Auth::Token { .. }, _) => {}
        }
        let (status, text) = self
            .exchange(method, path, &url, &headers, body.as_deref())
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
        if status == 403 && invalid && matches!(self.creds.auth, Auth::Keys { .. }) {
            return self.creds.expired_key();
        }
        if status == 401 && matches!(self.creds.auth, Auth::Token { .. }) {
            return self.creds.expired_token();
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
