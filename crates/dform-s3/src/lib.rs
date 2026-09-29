//! The `s3` state backend: `dform_core::store::Store` over the objects of
//! an S3 bucket under a prefix, for AWS S3, OVH Object Storage, MinIO and
//! anything else that speaks S3 with conditional writes (`If-Match` and
//! `If-None-Match: *` on PUT; AWS has them since November 2024). Leases
//! are the store's defaults over those writes.
//!
//! Requests are signed with `rusty-s3` (SigV4, query-string auth, the
//! condition headers among the signed ones) and sent with `ureq` over
//! rustls: blocking, with no async runtime, so the command line stays
//! synchronous and dform-core keeps no network stack.
//!
//! Conditional writes are checked once per bucket ([`S3Store::check_conditions`])
//! before a deployment's objects are written there: a server that ignores
//! `If-Match` or `If-None-Match` would let two writers overwrite each
//! other, and is refused. The answer is cached under the state root's
//! `cache/`.
//!
//! Credentials: `DFORM_S3_ACCESS_KEY_ID` and `DFORM_S3_SECRET_ACCESS_KEY`
//! (and `DFORM_S3_SESSION_TOKEN`), else AWS's `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`. Only the environment:
//! no profile file, no instance metadata.

use anyhow::{Context, Result, anyhow, bail};
use dform_core::store::{Cond, Object, S3Spec, Store};
use rusty_s3::actions::{
    CreateBucket, DeleteObject, GetObject, ListObjectsV2, PutObject, S3Action,
};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

/// How long a signed URL is good for: it is used at once.
const SIGNED_FOR: Duration = Duration::from_secs(300);

/// The largest object read: an audit log grows.
const READ_LIMIT: u64 = 1 << 30;

/// One deployment's objects: `s3://BUCKET/PREFIX[/<k>=<v>]/<key>`.
pub struct S3Store {
    /// The endpoint as given (`None`: AWS's), for the conditions cache.
    endpoint: Option<String>,
    bucket: Bucket,
    creds: Credentials,
    /// The deployment's prefix, without a trailing `/`.
    prefix: String,
    agent: ureq::Agent,
}

/// The credentials from the environment.
pub fn credentials() -> Result<Credentials> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    for (id, secret, token) in [
        (
            "DFORM_S3_ACCESS_KEY_ID",
            "DFORM_S3_SECRET_ACCESS_KEY",
            "DFORM_S3_SESSION_TOKEN",
        ),
        (
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ),
    ] {
        if let (Some(id), Some(secret)) = (get(id), get(secret)) {
            return Ok(match get(token) {
                Some(t) => Credentials::new_with_token(id, secret, t),
                None => Credentials::new(id, secret),
            });
        }
    }
    bail!(
        "the s3 backend has no credentials: set DFORM_S3_ACCESS_KEY_ID and \
         DFORM_S3_SECRET_ACCESS_KEY (or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY)"
    )
}

impl S3Store {
    /// The objects of the stack `spec` under `sub` (a keyed deployment's
    /// segment, else empty), with the environment's credentials.
    pub fn open(spec: &S3Spec, sub: &str) -> Result<S3Store> {
        S3Store::with_credentials(spec, sub, credentials()?)
    }

    /// An endpoint is addressed path-style (`URL/BUCKET/KEY`), as MinIO
    /// and OVH take it; without one, AWS's regional endpoint,
    /// virtual-host style. The region defaults to `us-east-1`.
    pub fn with_credentials(spec: &S3Spec, sub: &str, creds: Credentials) -> Result<S3Store> {
        let region = spec.region.clone().unwrap_or_else(|| "us-east-1".into());
        let (endpoint, style) = match &spec.endpoint {
            Some(e) => (e.clone(), UrlStyle::Path),
            None => (
                format!("https://s3.{region}.amazonaws.com"),
                UrlStyle::VirtualHost,
            ),
        };
        let url = endpoint
            .parse()
            .with_context(|| format!("backend {spec}: endpoint {endpoint}"))?;
        let bucket = Bucket::new(url, style, spec.bucket.clone(), region)
            .map_err(|e| anyhow!("backend {spec}: endpoint {endpoint}: {e:?}"))?;
        let prefix = [spec.prefix.as_str(), sub]
            .into_iter()
            .map(|p| p.trim_matches('/'))
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .new_agent();
        Ok(S3Store {
            endpoint: spec.endpoint.clone(),
            bucket,
            creds,
            prefix,
            agent,
        })
    }

    /// Does the server keep the conditions of a write, `If-None-Match: *`
    /// and `If-Match`? Asked once per endpoint and bucket, with a probe
    /// object under the prefix (written, refused over, deleted); a bucket
    /// that passed is remembered in `cache` (the state root's `cache/`).
    /// Refused, naming what the server ignored.
    pub fn check_conditions(&self, cache: Option<&Path>) -> Result<()> {
        let seen = cache.map(|c| self.conditions_mark(c));
        if seen.as_ref().is_some_and(|p| p.exists()) {
            return Ok(());
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let key = format!(".dform-conditions-{}-{nonce:x}", std::process::id());
        let r = self.probe(&key);
        let _ = self.delete(&key);
        let ignored = r?;
        if let Some(what) = ignored {
            bail!(
                "s3 bucket {} at {}: the server ignores {what} on a PUT; dform's leases and \
                 state writes rely on conditional writes (AWS S3 since November 2024, MinIO), \
                 and would let two writers overwrite each other here",
                self.bucket.name(),
                self.endpoint.as_deref().unwrap_or("AWS")
            );
        }
        if let Some(p) = seen {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
            }
            std::fs::write(
                &p,
                format!(
                    "s3 bucket {} at {} keeps If-Match and If-None-Match\n",
                    self.bucket.name(),
                    self.endpoint.as_deref().unwrap_or("AWS")
                ),
            )
            .with_context(|| format!("write {}", p.display()))?;
        }
        Ok(())
    }

    /// Where the check's pass for this endpoint and bucket is kept.
    fn conditions_mark(&self, cache: &Path) -> PathBuf {
        let id = format!(
            "{} {}",
            self.endpoint.as_deref().unwrap_or("aws"),
            self.bucket.name()
        );
        let digest = dform_core::approval::sha256_hex(id.as_bytes());
        cache.join("s3-conditions").join(&digest[..32])
    }

    /// The probe: which condition the server ignored, if one was.
    fn probe(&self, key: &str) -> Result<Option<&'static str>> {
        let Some(first) = self.put(key, b"1", &Cond::IfAbsent)? else {
            bail!(
                "s3 bucket {}: the probe {} exists already",
                self.bucket.name(),
                self.locate(key)
            );
        };
        if self.put(key, b"2", &Cond::IfAbsent)?.is_some() {
            return Ok(Some("If-None-Match: *"));
        }
        let wrong = if first == "\"0\"" { "\"1\"" } else { "\"0\"" };
        if self.put(key, b"3", &Cond::IfMatch(wrong.into()))?.is_some() {
            return Ok(Some("If-Match"));
        }
        if self.put(key, b"4", &Cond::IfMatch(first))?.is_none() {
            bail!(
                "s3 bucket {}: a PUT with If-Match of the object's own ETag was refused",
                self.bucket.name()
            );
        }
        Ok(None)
    }

    fn object(&self, key: &str) -> String {
        match self.prefix.as_str() {
            "" => key.to_string(),
            p => format!("{p}/{key}"),
        }
    }

    /// Make the bucket, unless it is there (tests: a fresh MinIO).
    pub fn create_bucket(&self) -> Result<()> {
        let url = CreateBucket::new(&self.bucket, &self.creds).sign(SIGNED_FOR);
        let resp = self
            .agent
            .put(url.as_str())
            .send_empty()
            .map_err(|e| anyhow!("create bucket {}: {e}", self.bucket.name()))?;
        match resp.status().as_u16() {
            200..=299 | 409 => Ok(()),
            _ => Err(failure("create bucket", self.bucket.name(), resp)),
        }
    }
}

/// An S3 error response as a message: the status, and the body's `Code`
/// and `Message`.
fn failure(op: &str, at: &str, mut resp: ureq::http::Response<ureq::Body>) -> anyhow::Error {
    let status = resp.status();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    let tag = |t: &str| {
        let open = format!("<{t}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find(&format!("</{t}>"))? + start;
        Some(body[start..end].to_string())
    };
    match (tag("Code"), tag("Message")) {
        (Some(c), Some(m)) => anyhow!("s3 {op} {at}: {status}: {c}: {m}"),
        (Some(c), None) => anyhow!("s3 {op} {at}: {status}: {c}"),
        _ => anyhow!("s3 {op} {at}: {status}"),
    }
}

fn etag(resp: &ureq::http::Response<ureq::Body>, op: &str, at: &str) -> Result<String> {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| anyhow!("s3 {op} {at}: the answer has no ETag"))
}

impl Store for S3Store {
    fn locate(&self, key: &str) -> String {
        format!("s3://{}/{}", self.bucket.name(), self.object(key))
    }

    fn get(&self, key: &str) -> Result<Option<Object>> {
        let (obj, at) = (self.object(key), self.locate(key));
        let url = GetObject::new(&self.bucket, Some(&self.creds), &obj).sign(SIGNED_FOR);
        let mut resp = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| anyhow!("s3 get {at}: {e}"))?;
        match resp.status().as_u16() {
            200 => {
                let etag = etag(&resp, "get", &at)?;
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .limit(READ_LIMIT)
                    .read_to_vec()
                    .map_err(|e| anyhow!("s3 get {at}: {e}"))?;
                Ok(Some(Object { bytes, etag }))
            }
            404 => Ok(None),
            _ => Err(failure("get", &at, resp)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8], cond: &Cond) -> Result<Option<String>> {
        let (obj, at) = (self.object(key), self.locate(key));
        let header = match cond {
            Cond::Any => None,
            Cond::IfMatch(e) => Some(("if-match", e.clone())),
            Cond::IfAbsent => Some(("if-none-match", "*".to_string())),
        };
        let mut action = PutObject::new(&self.bucket, Some(&self.creds), &obj);
        if let Some((k, v)) = &header {
            action.headers_mut().insert(*k, v.clone());
        }
        let url = action.sign(SIGNED_FOR);
        let mut req = self.agent.put(url.as_str());
        if let Some((k, v)) = &header {
            req = req.header(*k, v);
        }
        let resp = req.send(bytes).map_err(|e| anyhow!("s3 put {at}: {e}"))?;
        match resp.status().as_u16() {
            200..=299 => Ok(Some(etag(&resp, "put", &at)?)),
            // 409: a conditional write raced another one (AWS).
            412 | 409 => Ok(None),
            _ => Err(failure("put", &at, resp)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full = self.object(prefix);
        let strip = match self.prefix.as_str() {
            "" => String::new(),
            p => format!("{p}/"),
        };
        let at = self.locate(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = ListObjectsV2::new(&self.bucket, Some(&self.creds));
            action.with_prefix(full.as_str());
            if let Some(t) = &token {
                action.with_continuation_token(t.as_str());
            }
            let url = action.sign(SIGNED_FOR);
            let mut resp = self
                .agent
                .get(url.as_str())
                .call()
                .map_err(|e| anyhow!("s3 list {at}: {e}"))?;
            if resp.status().as_u16() != 200 {
                return Err(failure("list", &at, resp));
            }
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| anyhow!("s3 list {at}: {e}"))?;
            let page =
                ListObjectsV2::parse_response(&text).map_err(|e| anyhow!("s3 list {at}: {e}"))?;
            out.extend(
                page.contents
                    .into_iter()
                    .filter_map(|c| c.key.strip_prefix(&strip).map(String::from)),
            );
            match page.next_continuation_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        out.sort();
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let (obj, at) = (self.object(key), self.locate(key));
        let url = DeleteObject::new(&self.bucket, Some(&self.creds), &obj).sign(SIGNED_FOR);
        let resp = self
            .agent
            .delete(url.as_str())
            .call()
            .map_err(|e| anyhow!("s3 delete {at}: {e}"))?;
        match resp.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => Err(failure("delete", &at, resp)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dform_core::store::{Deployment, LOCK, LeaseTimes, STATE};
    use std::sync::Arc;

    fn spec(endpoint: &str, prefix: &str) -> S3Spec {
        S3Spec {
            bucket: "dform".into(),
            prefix: prefix.into(),
            endpoint: Some(endpoint.into()),
            region: None,
        }
    }

    fn store(server: &fake::Server, prefix: &str, sub: &str) -> S3Store {
        S3Store::with_credentials(
            &spec(&server.endpoint, prefix),
            sub,
            Credentials::new("id", "secret"),
        )
        .unwrap()
    }

    #[test]
    fn objects_round_trip_with_conditions() {
        let server = fake::Server::start();
        let s = store(&server, "p/", "app/env=prod,region=us%2F1");
        s.create_bucket().unwrap();
        assert_eq!(
            s.locate(STATE),
            "s3://dform/p/app/env=prod,region=us%2F1/state.json"
        );
        assert_eq!(s.get(STATE).unwrap(), None);
        let e1 = s.put(STATE, b"one", &Cond::IfAbsent).unwrap().unwrap();
        assert_eq!(s.put(STATE, b"two", &Cond::IfAbsent).unwrap(), None);
        let e2 = s
            .put(STATE, b"two", &Cond::IfMatch(e1.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(s.put(STATE, b"x", &Cond::IfMatch(e1)).unwrap(), None);
        let o = s.get(STATE).unwrap().unwrap();
        assert_eq!((o.bytes.as_slice(), o.etag), (&b"two"[..], e2));
        s.put(LOCK, b"{}", &Cond::Any).unwrap();
        assert_eq!(s.list("").unwrap(), [STATE, LOCK]);
        assert_eq!(s.list("state.j").unwrap(), [STATE]);
        s.delete(STATE).unwrap();
        s.delete(STATE).unwrap();
        assert_eq!(s.list("").unwrap(), [LOCK]);
    }

    #[test]
    fn a_server_that_ignores_conditions_is_refused_once_and_a_good_one_remembered() {
        let cache = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-scratch")
            .join(format!("s3-conditions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let lax = fake::Server::ignoring_conditions();
        let e = store(&lax, "p", "app")
            .check_conditions(Some(&cache))
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("the server ignores If-None-Match: *"),
            "{e}"
        );
        assert!(!cache.exists(), "a refusal is not cached");
        assert_eq!(
            store(&lax, "p", "app").list("").unwrap(),
            Vec::<String>::new()
        );
        let good = fake::Server::start();
        store(&good, "p", "app")
            .check_conditions(Some(&cache))
            .unwrap();
        assert_eq!(
            std::fs::read_dir(cache.join("s3-conditions"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            store(&good, "p", "app").list("").unwrap(),
            Vec::<String>::new()
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_lease_and_a_fenced_write_over_http() {
        let server = fake::Server::start();
        let times = LeaseTimes {
            duration: Duration::from_secs(60),
            renewal: Duration::from_secs(20),
        };
        let a = Deployment::new(Arc::new(store(&server, "", "app")), "app", times);
        let st = a.load_state().unwrap();
        let ga = a.lock().unwrap();
        a.save_state(&st).unwrap();
        let b = Deployment::new(Arc::new(store(&server, "", "app")), "app", times);
        let e = b.lock().err().unwrap();
        assert!(
            e.to_string()
                .contains("stack app is locked by another apply"),
            "{e}"
        );
        assert!(b.unlock().unwrap().contains("is broken"));
        b.load_state().unwrap();
        let _gb = b.lock().unwrap();
        let e = a.save_state(&st).unwrap_err();
        assert!(format!("{e:#}").contains("refused by fencing"), "{e:#}");
        drop(ga);
    }
}
