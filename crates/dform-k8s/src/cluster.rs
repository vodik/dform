//! The API server, reached through the host: a thin client of the
//! Kubernetes API over the SDK's `Http` (discovery, get, list, server-side
//! apply, delete), every request sent with the session's kubeconfig
//! credential applied by the host, which the provider holds by name and
//! never by value. Each request is awaited in the call it is sent for: an
//! `io.call` span under that call, paced in its session's window (its
//! bucket the verb and the resource, `get deployments`), over the host's
//! one connection to the server. Writes are server-side apply as the
//! field manager `dform`, never forced.
//!
//! A status is answered as it is waited for: by polling (a delete's
//! [`Cluster::gone`]), not by a watch, since the host answers a request
//! whole (`http.send` has no streamed body) and a watch is a body that
//! does not end. Between polls the call sleeps (`dform_sdk::sleep`): a
//! wasm provider answers its other calls meanwhile.

use crate::cache::{self, Scope};
use crate::object::MANAGER;
use crate::openapi::Kind;
use anyhow::{Context, Result, anyhow, bail};
use dform_sdk::{Credential, Request, Response, host, join_all};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;

/// How long the API server may take to answer its OpenAPI index, the
/// first request of a session (connecting, TLS), before the provider
/// works offline.
const REACH: Duration = Duration::from_secs(5);

/// The objects a list asks for at once; the server's `continue` token
/// asks for the rest.
const PAGE: usize = 500;

/// A setting [`Cluster::configured`] reads.
pub struct Setting {
    pub name: &'static str,
    /// Its value is a secret's: what the handshake declares, so a
    /// program's secret goes only where the host keeps it (E0304).
    pub sensitive: bool,
    /// It is the kubeconfig, or a part of it: the host holds it, and the
    /// provider is configured with the credential's name.
    pub credential: bool,
}

const fn setting(name: &'static str, sensitive: bool, credential: bool) -> Setting {
    Setting {
        name,
        sensitive,
        credential,
    }
}

/// The settings [`Cluster::configured`] reads: the kubeconfig, or its
/// parts (`host`, `ca`, `token`, `client_certificate`, `client_key`),
/// each a part of the session's kubeconfig credential, and `namespace`.
pub const SETTINGS: [Setting; 7] = [
    setting("kubeconfig", true, true),
    setting("host", false, true),
    setting("ca", false, true),
    setting("token", true, true),
    setting("client_certificate", false, true),
    setting("client_key", true, true),
    setting("namespace", false, false),
];

/// The credential kind of the [`SETTINGS`] that are credentials.
pub const CREDENTIAL: &str = "kubeconfig";

pub struct Cluster {
    /// The session's kubeconfig, as the host holds it: a handle.
    credential: Credential,
    /// The objects' namespace when a document names none: the `namespace`
    /// setting, else the kubeconfig context's, else `default`.
    pub namespace: String,
    /// The server's URL, for messages, the schema cache's name and the
    /// account the session reaches.
    pub url: String,
}

/// A status the API server answered for a request it refused
/// (`kind: Status`).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct Status {
    pub code: u16,
    pub reason: String,
    pub message: String,
    pub details: Option<StatusDetails>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct StatusDetails {
    pub causes: Vec<StatusCause>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct StatusCause {
    pub reason: String,
    pub message: String,
    pub field: String,
}

/// Why a request answered nothing it asked for.
#[derive(Debug)]
pub enum Refused {
    /// The server refused it, saying why.
    Api(Status),
    /// No answer: the host's error (a write may have happened).
    Transport(anyhow::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Api(s) => write!(f, "{} ({})", s.message, s.code),
            Refused::Transport(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// The server's status, if it answered `code`.
    fn is(&self, code: u16) -> bool {
        matches!(self, Refused::Api(s) if s.code == code)
    }
}

/// Why a write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// Another field manager owns fields this apply sets: the manager and
    /// the field, per conflict.
    Conflict(Vec<(String, String)>),
    /// The change sets a field the server will not change in place.
    Immutable(String),
    /// Something the object needs is not there: its namespace.
    NotFound(String),
    /// The server refused it for another reason.
    Other(anyhow::Error),
    /// No answer: the write may have happened.
    Transport(anyhow::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict(cs) => {
                let cs: Vec<String> = cs
                    .iter()
                    .map(|(m, field)| format!("{field} is owned by field manager {m:?}"))
                    .collect();
                write!(
                    f,
                    "server-side apply conflict: {} (dform applies as {MANAGER:?} without \
                     force; change the program to match, or remove that manager's ownership)",
                    cs.join("; ")
                )
            }
            WriteError::Immutable(m) | WriteError::NotFound(m) => write!(f, "{m}"),
            WriteError::Other(e) | WriteError::Transport(e) => write!(f, "{e:#}"),
        }
    }
}

/// The manager a conflict cause names: `conflict with "kubectl" using
/// apps/v1` or `conflict with "kubectl"`.
fn conflicting_manager(message: &str) -> String {
    message.split('"').nth(1).unwrap_or(message).to_string()
}

fn write_error(e: Refused) -> WriteError {
    match e {
        Refused::Api(s) => api_write_error(&s),
        Refused::Transport(e) => WriteError::Transport(e),
    }
}

fn api_write_error(s: &Status) -> WriteError {
    let causes = s
        .details
        .as_ref()
        .map(|d| d.causes.as_slice())
        .unwrap_or(&[]);
    if s.code == 409 && s.reason == "Conflict" && !causes.is_empty() {
        return WriteError::Conflict(
            causes
                .iter()
                .map(|c| (conflicting_manager(&c.message), c.field.clone()))
                .collect(),
        );
    }
    if s.code == 422
        && (s.message.contains("field is immutable")
            || causes
                .iter()
                .any(|c| c.message.contains("field is immutable")))
    {
        return WriteError::Immutable(s.message.clone());
    }
    if s.code == 404 {
        return WriteError::NotFound(s.message.clone());
    }
    WriteError::Other(anyhow!("{} ({})", s.message, s.code))
}

/// `s` as a URL's query value: every byte but the unreserved ones
/// percent-encoded.
fn query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A cluster's OpenAPI document and its hash (of the cache file's text,
/// which holds the server's index and the document).
pub struct Document {
    pub hash: String,
    /// The cache file's text, not parsed yet.
    text: Option<String>,
    parsed: Option<Json>,
}

impl Document {
    fn cached(text: String) -> Document {
        Document {
            hash: dform_core::zset::file::fnv64(text.as_bytes()),
            text: Some(text),
            parsed: None,
        }
    }

    /// The document, `{"paths": {GV: DOC}}`.
    pub fn parse(self) -> Result<Json> {
        match (self.parsed, self.text) {
            (Some(doc), _) => Ok(doc),
            (None, Some(text)) => {
                let mut cached: Json =
                    serde_json::from_str(&text).context("parse the cached OpenAPI document")?;
                Ok(cached["document"].take())
            }
            (None, None) => Err(anyhow!("no OpenAPI document")),
        }
    }
}

/// The documents of the OpenAPI index `index` that hold kinds, by the URL
/// the index names each at (its hash in the query): `api/v1` and
/// `apis/GROUP/VERSION`. Its other entries (a group's `apis/GROUP`, which
/// only lists versions; `apis`; `version`) are never fetched: k3s answers
/// another hash for `apis` and `version` from one ask to the next.
fn served(index: &Json) -> Result<BTreeMap<String, String>> {
    let paths = index
        .get("paths")
        .and_then(Json::as_object)
        .ok_or_else(|| anyhow!("the /openapi/v3 index has no paths"))?;
    Ok(paths
        .iter()
        .filter(|(gv, _)| {
            *gv == "api/v1" || gv.starts_with("apis/") && gv.matches('/').count() == 2
        })
        .filter_map(|(gv, e)| {
            let url = e.get("serverRelativeURL")?.as_str()?;
            Some((gv.clone(), url.to_string()))
        })
        .collect())
}

/// The cache file of a cluster's OpenAPI documents: the URL each was
/// fetched from ([`served`]), and the documents, `{"paths": {GV: DOC}}`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    server: String,
    index: BTreeMap<String, String>,
    document: Json,
}

impl Cached {
    /// Whether the cache file `text` holds the documents `index` names,
    /// its documents skipped, not parsed.
    fn names(text: &str, index: &BTreeMap<String, String>) -> bool {
        #[derive(serde::Deserialize)]
        struct Head {
            index: BTreeMap<String, String>,
            #[allow(dead_code)]
            document: serde::de::IgnoredAny,
        }
        serde_json::from_str::<Head>(text).is_ok_and(|h| h.index == *index)
    }

    /// Its document of `gv`, taken, if it was fetched from `url`.
    fn take(&mut self, gv: &str, url: &str) -> Option<Json> {
        if self.index.get(gv)? != url {
            return None;
        }
        self.document.get_mut("paths")?.get_mut(gv).map(Json::take)
    }
}

impl Cluster {
    /// The cluster the kubeconfig credential `name` reaches, which the
    /// host opens for this provider (the program's, or the environment's
    /// where dform.toml grants it); `namespace` (the setting) before its
    /// context's. Nothing is contacted yet. Its error is the host's: why
    /// the kubeconfig does not load (not granted, or `no kubeconfig at
    /// ~/.kube/config and not in a cluster`).
    pub fn open(name: &str, namespace: Option<&str>) -> Result<Cluster> {
        let credential = host()
            .secrets
            .open(name)
            .map_err(|e| anyhow!("{}", e.message))?;
        let url = credential
            .endpoint()
            .ok_or_else(|| anyhow!("the kubeconfig names no server"))?
            .trim_end_matches('/')
            .to_string();
        let namespace = namespace
            .or(credential.namespace())
            .unwrap_or("default")
            .to_string();
        Ok(Cluster {
            credential,
            namespace,
            url,
        })
    }

    /// The cluster the program's `use k8s { .. }` names, if it names one:
    /// its kubeconfig credential, by the name each of its credential
    /// settings (`kubeconfig`, or `host` and the rest) was configured with
    /// in their place, and its `namespace`. A setting that is no
    /// credential's name is refused without quoting it.
    pub fn configured(settings: &Json) -> Result<Option<Cluster>> {
        let named = SETTINGS
            .iter()
            .filter(|s| s.credential)
            .find_map(|s| settings.get(s.name).map(|v| (s.name, v)));
        let Some((setting, name)) = named else {
            return Ok(None);
        };
        let name = match name.as_str() {
            Some(n) if n.starts_with(&format!("{CREDENTIAL}:")) => n,
            _ => bail!(
                "use k8s: {setting} is not the name of the credential dform holds for it \
                 (the provider is configured by a dform that holds none)"
            ),
        };
        let namespace = match settings.get("namespace") {
            None | Some(Json::Null) => None,
            Some(Json::String(n)) => Some(n.as_str()),
            Some(_) => bail!("use k8s: namespace is a string"),
        };
        Cluster::open(name, namespace).map(Some)
    }

    /// Send `req` through the host with the session's credential, in the
    /// call it is awaited in (its session's window, its span). A status the
    /// server answered that is not a success is [`Refused::Api`].
    async fn send(&self, req: Request) -> std::result::Result<Response, Refused> {
        let answer = host()
            .http
            .request(req.auth(&self.credential))
            .await
            .map_err(|e| Refused::Transport(e.into()))?;
        if (200..300).contains(&answer.status) {
            return Ok(answer);
        }
        let status = serde_json::from_slice::<Status>(&answer.body)
            .ok()
            .filter(|s| s.code != 0)
            .unwrap_or_else(|| Status {
                code: answer.status,
                reason: "Failed to parse error data".into(),
                message: String::from_utf8_lossy(&answer.body).into_owned(),
                details: None,
            });
        Err(Refused::Api(status))
    }

    /// A request for `path` (and query) on the server, its bucket `verb`
    /// and `resource`.
    fn request(&self, method: &str, path: &str, verb: &str, resource: &str) -> Request {
        Request::new(method, format!("{}{path}", self.url))
            .header("accept", "application/json")
            .bucket(format!("{verb} {resource}"))
    }

    /// `req`'s answer as JSON.
    async fn json(&self, req: Request) -> std::result::Result<Json, Refused> {
        let answer = self.send(req).await?;
        serde_json::from_slice(&answer.body)
            .map_err(|e| Refused::Transport(anyhow!("the server's answer is not JSON: {e}")))
    }

    async fn get_json(&self, uri: &str, timeout: Option<Duration>) -> Result<Json> {
        let mut req = self.request("GET", uri, "get", "openapi");
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        self.json(req)
            .await
            .with_context(|| format!("GET {}{uri}", self.url))
    }

    /// The OpenAPI v3 document, `{"paths": {GV: DOC}}`: each group-version's
    /// document the index names ([`served`]). The run's copy (`crate::cache`)
    /// is used as it is, unless `fresh` (a kind the cluster serves since,
    /// R-126): the deployments of a run on one cluster fetch it once. Else
    /// the account's copy is used as is when the index names each of its
    /// documents at the URL it was fetched from (the URL carries the
    /// document's hash); otherwise each document whose URL changed is
    /// fetched again, the rest kept, and the cache rewritten. A cached
    /// document is parsed only if it is needed ([`Document::parse`]): its
    /// derived schema may be cached too.
    pub async fn openapi(&self, fresh: bool) -> Result<Document> {
        let key = format!("openapi {}", self.url);
        if !fresh && let Some(text) = cache::read(Scope::Run, &key) {
            return Ok(Document::cached(text));
        }
        let index = served(&self.get_json("/openapi/v3", Some(REACH)).await?)?;
        let mut kept = None;
        if let Some(text) = cache::read(Scope::Account, &key) {
            if Cached::names(&text, &index) {
                cache::write(Scope::Run, &key, &text);
                return Ok(Document::cached(text));
            }
            kept = serde_json::from_str::<Cached>(&text).ok();
        }
        let mut paths = serde_json::Map::new();
        let mut asked = Vec::new();
        for (gv, url) in &index {
            match kept.as_mut().and_then(|k| k.take(gv, url)) {
                Some(doc) => {
                    paths.insert(gv.clone(), doc);
                }
                None => asked.push((gv, url)),
            }
        }
        // Every document not kept, asked for at once: the host's window
        // paces them, and each is parsed as it arrives.
        let fetched = join_all(asked.into_iter().map(|(gv, url)| async move {
            Ok::<_, anyhow::Error>((gv.clone(), self.get_json(url, None).await?))
        }))
        .await;
        for got in fetched {
            let (gv, doc) = got?;
            paths.insert(gv, doc);
        }
        let cached = Cached {
            server: self.url.clone(),
            index,
            document: json!({ "paths": paths }),
        };
        let text = serde_json::to_string(&cached)?;
        cache::write(Scope::Account, &key, &text);
        cache::write(Scope::Run, &key, &text);
        Ok(Document {
            hash: dform_core::zset::file::fnv64(text.as_bytes()),
            text: None,
            parsed: Some(cached.document),
        })
    }

    /// The object, or `None` if there is none.
    pub async fn get(&self, kind: &Kind, ns: &str, name: &str) -> Result<Option<Json>> {
        let path = format!("{}/{name}", kind.collection(ns));
        match self
            .json(self.request("GET", &path, "get", &kind.plural))
            .await
        {
            Ok(o) => Ok(Some(o)),
            Err(e) if e.is(404) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "GET {} {}",
                kind.kind,
                crate::object::remote(kind, ns, name)
            ))),
        }
    }

    /// The objects of `kind` in every namespace whose labels match
    /// `selector` (all of them without one), [`PAGE`] at a time.
    pub async fn list(&self, kind: &Kind, selector: Option<&str>) -> Result<Vec<Json>> {
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut path = format!("{}?limit={PAGE}", kind.collection_all());
            if let Some(s) = selector {
                path += &format!("&labelSelector={}", query(s));
            }
            if let Some(c) = &next {
                path += &format!("&continue={}", query(c));
            }
            let mut page = self
                .json(self.request("GET", &path, "list", &kind.plural))
                .await
                .with_context(|| format!("LIST {}", kind.kind))?;
            if let Some(Json::Array(more)) = page.get_mut("items").map(Json::take) {
                items.extend(more);
            }
            next = page
                .pointer("/metadata/continue")
                .and_then(Json::as_str)
                .filter(|c| !c.is_empty())
                .map(String::from);
            if next.is_none() {
                return Ok(items);
            }
        }
    }

    /// Server-side apply `obj` as `dform`, not forced; `dry_run` persists
    /// nothing. The object as the server has it (or would).
    pub async fn apply(
        &self,
        kind: &Kind,
        ns: &str,
        name: &str,
        obj: &Json,
        dry_run: bool,
    ) -> std::result::Result<Json, WriteError> {
        let mut path = format!(
            "{}/{name}?fieldManager={}",
            kind.collection(ns),
            query(MANAGER)
        );
        if dry_run {
            path += "&dryRun=All";
        }
        let body = serde_json::to_vec(obj).map_err(|e| WriteError::Other(e.into()))?;
        // A dry run persists nothing: the host may send it again.
        let req = self
            .request("PATCH", &path, "patch", &kind.plural)
            .header("content-type", "application/apply-patch+yaml")
            .body(body)
            .idempotent(dry_run);
        self.json(req).await.map_err(write_error)
    }

    /// Delete with background propagation (the garbage collector removes
    /// what the object owns). Gone already is not an error.
    pub async fn delete(&self, kind: &Kind, ns: &str, name: &str) -> Result<()> {
        let path = format!("{}/{name}", kind.collection(ns));
        let options = json!({"apiVersion": "v1", "kind": "DeleteOptions",
                             "propagationPolicy": "Background"});
        let req = self
            .request("DELETE", &path, "delete", &kind.plural)
            .json(&options);
        match self.send(req).await {
            Ok(_) => Ok(()),
            Err(e) if e.is(404) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "DELETE {} {}",
                kind.kind,
                crate::object::remote(kind, ns, name)
            ))),
        }
    }

    /// Wait until the object is gone (a finalizer can hold it), polling,
    /// up to `timeout`. Whether it went.
    pub async fn gone(&self, kind: &Kind, ns: &str, name: &str, timeout: Duration) -> Result<bool> {
        let start = std::time::Instant::now();
        loop {
            if self.get(kind, ns, name).await?.is_none() {
                return Ok(true);
            }
            if start.elapsed() >= timeout {
                return Ok(false);
            }
            dform_sdk::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_names_the_manager_and_the_field() {
        let s = Status {
            code: 409,
            reason: "Conflict".into(),
            message: "Apply failed with 1 conflict: conflict with \"kubectl\": .spec.replicas"
                .into(),
            details: Some(StatusDetails {
                causes: vec![StatusCause {
                    reason: "FieldManagerConflict".into(),
                    message: "conflict with \"kubectl\" using apps/v1".into(),
                    field: ".spec.replicas".into(),
                }],
            }),
        };
        let e = api_write_error(&s);
        assert!(
            matches!(&e, WriteError::Conflict(cs) if cs == &[("kubectl".to_string(), ".spec.replicas".to_string())]),
            "{e:?}"
        );
        assert!(
            e.to_string()
                .contains(".spec.replicas is owned by field manager \"kubectl\""),
            "{e}"
        );
    }

    #[test]
    fn a_missing_namespace_is_not_found() {
        let s = Status {
            code: 404,
            reason: "NotFound".into(),
            message: "namespaces \"shop\" not found".into(),
            ..Default::default()
        };
        assert!(matches!(api_write_error(&s), WriteError::NotFound(_)));
    }

    #[test]
    fn an_immutable_field_is_its_own_error() {
        let s = Status {
            code: 422,
            reason: "Invalid".into(),
            message: "Deployment.apps \"web\" is invalid: spec.selector: Invalid value: \
                      ...: field is immutable"
                .into(),
            ..Default::default()
        };
        assert!(matches!(api_write_error(&s), WriteError::Immutable(_)));
    }

    /// A label selector is a query value, its `=`, `,` and `/` encoded.
    #[test]
    fn a_selector_is_encoded() {
        assert_eq!(
            query("dform.dev/stack=app,x!=y"),
            "dform.dev%2Fstack%3Dapp%2Cx%21%3Dy"
        );
    }
}
