//! A signed client of the OVH API (`sign`), blocking, over `ureq`.
//!
//! Every call is signed with the server's clock: the difference to the
//! local one is asked once (`GET /auth/time`) and kept in dform's cache
//! beside the project ids (`ovh-projects.json`), so a run after the first
//! asks it again only when a signed call is refused (After R-123): the
//! clock moved, the offset is asked afresh and the call sent once more. A
//! failure names the method,
//! the path and the HTTP status (`HTTP 503`), so dform's retry policy
//! (R-81, `plugin::policy::retryable`) tells a transient one (429, 5xx)
//! from a refusal; a call that reached no answer is `Unreachable`, which
//! may have taken effect.

use crate::config::Credentials;
use crate::sign::signature;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    agent: ureq::Agent,
    /// Server time minus local time, in seconds, once known, and whether
    /// it was read from the cache rather than asked in this run.
    delta: Mutex<Option<(i64, bool)>>,
    /// The cache file the offset is kept in (`ovh-projects.json`).
    cache: Option<PathBuf>,
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
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        Client {
            creds,
            agent,
            delta: Mutex::new(None),
            cache: None,
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
    fn timestamp(&self) -> Result<i64> {
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
            None => self.ask_time()?,
        };
        Ok(Self::now() + d)
    }

    /// Ask the server its time (`GET /auth/time`): the offset now known
    /// and kept in the cache.
    fn ask_time(&self) -> Result<i64> {
        let server = self
            .send("GET", "/auth/time", None, false)?
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

    pub fn get(&self, path: &str) -> Result<Json> {
        self.send("GET", path, None, true)
    }

    /// GET, `None` for a 404.
    pub fn get_opt(&self, path: &str) -> Result<Option<Json>> {
        match self.get(path) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn post(&self, path: &str, body: &Json) -> Result<Json> {
        self.send("POST", path, Some(body), true)
    }

    pub fn put(&self, path: &str, body: &Json) -> Result<Json> {
        self.send("PUT", path, Some(body), true)
    }

    pub fn delete(&self, path: &str) -> Result<Json> {
        self.send("DELETE", path, None, true)
    }

    /// One call: `path` under the endpoint's URL (it carries its query).
    /// A signed call the server refuses while its timestamp rests on an
    /// offset read from the cache asks the server's time again, and is
    /// sent once more when the offset moved.
    fn send(&self, method: &str, path: &str, body: Option<&Json>, signed: bool) -> Result<Json> {
        let out = self.send_once(method, path, body, signed);
        let refused = matches!(&out, Err(e) if matches!(e.status(), Some(400 | 401 | 403)));
        let cached = *self.delta.lock().unwrap_or_else(|e| e.into_inner());
        match cached {
            Some((was, true)) if signed && refused => {
                if self.ask_time()? == was {
                    return out;
                }
                self.send_once(method, path, body, signed)
            }
            _ => out,
        }
    }

    fn send_once(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
        signed: bool,
    ) -> Result<Json> {
        let url = format!("{}{path}", self.creds.url);
        let body = body.map(Json::to_string).unwrap_or_default();
        let unreachable = |why: String| Error::Unreachable {
            method: method.into(),
            path: path.into(),
            why,
        };
        let mut headers = vec![
            ("Content-Type", "application/json".to_string()),
            ("X-Ovh-Application", self.creds.application_key.clone()),
        ];
        if signed {
            let ts = self.timestamp()?;
            headers.push(("X-Ovh-Timestamp", ts.to_string()));
            headers.push(("X-Ovh-Consumer", self.creds.consumer_key.clone()));
            headers.push((
                "X-Ovh-Signature",
                signature(
                    &self.creds.application_secret,
                    &self.creds.consumer_key,
                    method,
                    &url,
                    &body,
                    ts,
                ),
            ));
        }
        let resp = match method {
            "GET" | "DELETE" => {
                let mut req = match method {
                    "GET" => self.agent.get(&url),
                    _ => self.agent.delete(&url),
                };
                for (k, v) in &headers {
                    req = req.header(*k, v);
                }
                req.call()
            }
            _ => {
                let mut req = match method {
                    "POST" => self.agent.post(&url),
                    _ => self.agent.put(&url),
                };
                for (k, v) in &headers {
                    req = req.header(*k, v);
                }
                req.send(body.as_bytes())
            }
        };
        let mut resp = resp.map_err(|e| unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| unreachable(format!("reading the answer: {e}")))?;
        if !(200..300).contains(&status) {
            // `{"class": "Client::NotFound", "message": "..."}`
            // OVH's `errorCode` says which kind of refusal: INVALID_CREDENTIAL
            // (the consumer key), NOT_GRANTED_CALL (its rights), ...
            let message = serde_json::from_str::<Json>(&text)
                .ok()
                .map(|j| {
                    let msg = j
                        .get("message")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string();
                    match j.get("errorCode").and_then(Json::as_str) {
                        Some(code) if !code.is_empty() => format!("{msg} ({code})"),
                        _ => msg,
                    }
                })
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| text.trim().chars().take(200).collect());
            return Err(Error::Status {
                method: method.into(),
                path: path.into(),
                status,
                message,
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

    #[test]
    fn escapes_a_segment() {
        assert_eq!(escape("Ubuntu 24.04"), "Ubuntu%2024.04");
        assert_eq!(escape("ca-east-tor"), "ca-east-tor");
    }
}
