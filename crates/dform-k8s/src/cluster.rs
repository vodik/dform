//! The API server: the kubeconfig's cluster (or the pod's, in a cluster),
//! reached with kube's client. Objects by kind, namespace and name; writes
//! are server-side apply as the field manager `dform`, never forced.

use crate::object::MANAGER;
use crate::openapi::Kind;
use anyhow::{Context, Result, anyhow, bail};
use kube::config::Kubeconfig;
use kube::core::Status;
use kube::core::params::{DeleteParams, GetParams, ListParams, Patch, PatchParams};
use kube::core::request::Request;
use serde_json::{Value as Json, json};
use std::path::Path;
use std::time::Duration;

/// How long connecting to the API server may take before the provider
/// works offline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Cluster {
    client: kube::Client,
    /// The kubeconfig context's namespace (`default` without one).
    pub namespace: String,
    /// The server's URL, for messages and the schema cache's name.
    pub url: String,
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

fn write_error(e: kube::Error) -> WriteError {
    match e {
        kube::Error::Api(s) => api_write_error(&s),
        other => WriteError::Transport(other.into()),
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

/// A kubeconfig of one context from separate fields: `host`, `ca`,
/// `token` or `client_certificate` and `client_key`, `namespace`.
fn fields_kubeconfig(settings: &Json) -> Result<Kubeconfig> {
    let field = |k: &str| -> Result<Option<&str>> {
        match settings.get(k) {
            None => Ok(None),
            Some(Json::String(s)) => Ok(Some(s)),
            Some(_) => bail!("provider_config kubernetes: {k} is a string"),
        }
    };
    // PEM as it is, or base64 of it (as a kubeconfig carries it).
    let data = |k: &str| -> Result<Option<String>> {
        Ok(
            field(k)?.map(|v| match v.trim_start().starts_with("-----BEGIN") {
                true => crate::object::base64_encode(v.as_bytes()),
                false => v.to_string(),
            }),
        )
    };
    let host = field("host")?.unwrap_or_default();
    let server = match host.contains("://") {
        true => host.to_string(),
        false => format!("https://{host}"),
    };
    let token = field("token")?;
    let (cert, key) = (data("client_certificate")?, data("client_key")?);
    if token.is_none() && (cert.is_none() || key.is_none()) {
        bail!(
            "provider_config kubernetes: host needs a token, or a client_certificate and a \
             client_key"
        );
    }
    let kc = json!({
        "clusters": [{"name": "dform", "cluster": {
            "server": server, "certificate-authority-data": data("ca")?}}],
        "users": [{"name": "dform", "user": {
            "token": token, "client-certificate-data": cert, "client-key-data": key}}],
        "contexts": [{"name": "dform", "context": {
            "cluster": "dform", "user": "dform", "namespace": field("namespace")?}}],
        "current-context": "dform",
    });
    serde_json::from_value(crate::object::strip_nulls(&kc))
        .map_err(|_| anyhow!("provider_config kubernetes: the fields make no kubeconfig"))
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

impl Cluster {
    /// The cluster the environment names: `KUBECONFIG` or `~/.kube/config`,
    /// else the pod's service account. Nothing is contacted yet.
    pub async fn infer() -> Result<Cluster> {
        let config = kube::Config::infer()
            .await
            .context("no kubeconfig and not in a cluster")?;
        Cluster::connect(config)
    }

    /// The cluster the program's `provider_config("kubernetes", ...)`
    /// names, if it names one: `kubeconfig` (the text of a kubeconfig, its
    /// current context), or `host` with `ca` and a `token` or a
    /// `client_certificate` and `client_key` (PEM, or base64 of it), and
    /// optionally `namespace`. The values stay in memory: nothing is
    /// written, and an error never quotes them.
    pub async fn configured(settings: &Json) -> Result<Option<Cluster>> {
        let kubeconfig = match settings.get("kubeconfig") {
            Some(Json::String(text)) => Kubeconfig::from_yaml(text).map_err(|_| {
                anyhow!("provider_config kubernetes: kubeconfig is not a kubeconfig (YAML)")
            })?,
            Some(_) => bail!("provider_config kubernetes: kubeconfig is a string"),
            None => match settings.get("host") {
                Some(_) => fields_kubeconfig(settings)?,
                None => return Ok(None),
            },
        };
        let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
            .await
            .map_err(|e| anyhow!("provider_config kubernetes: {e}"))?;
        Cluster::connect(config).map(Some)
    }

    fn connect(mut config: kube::Config) -> Result<Cluster> {
        config.connect_timeout = Some(CONNECT_TIMEOUT);
        let url = config.cluster_url.to_string();
        let namespace = config.default_namespace.clone();
        let client = kube::Client::try_from(config).context("a client for the kubeconfig")?;
        Ok(Cluster {
            client,
            namespace,
            url,
        })
    }

    async fn get_json(&self, uri: &str) -> Result<Json> {
        let mut req = Request::new("/openapi")
            .get("v3", &GetParams::default())
            .map_err(|e| anyhow!("{e}"))?;
        *req.uri_mut() = uri.parse().with_context(|| format!("the URL {uri}"))?;
        self.client
            .request::<Json>(req)
            .await
            .with_context(|| format!("GET {}{uri}", self.url.trim_end_matches('/')))
    }

    /// The OpenAPI v3 document, `{"paths": {GV: DOC}}`: the index, and each
    /// group-version's document. The copy cached in the file `cache` is used
    /// as is when its index is the server's (the index names each
    /// document's hash); otherwise the documents are fetched and the cache
    /// rewritten. A cached document is parsed only if it is needed
    /// ([`Document::parse`]): its derived schema may be cached too.
    pub async fn openapi(&self, cache: Option<&Path>) -> Result<Document> {
        /// A cache file's index, its document skipped.
        #[derive(serde::Deserialize)]
        struct Head {
            index: Json,
            #[allow(dead_code)]
            document: serde::de::IgnoredAny,
        }
        let index = self.get_json("/openapi/v3").await?;
        let urls = index
            .get("paths")
            .and_then(Json::as_object)
            .ok_or_else(|| anyhow!("the /openapi/v3 index has no paths"))?;
        if let Some(cache) = cache
            && let Ok(text) = std::fs::read_to_string(cache)
            && let Ok(head) = serde_json::from_str::<Head>(&text)
            && head.index == index
        {
            return Ok(Document::cached(text));
        }
        let mut paths = serde_json::Map::new();
        for (gv, entry) in urls {
            // Groups only (`apis/apps`) list versions; the kinds are in the
            // versioned documents.
            if !(gv == "api/v1" || gv.starts_with("apis/") && gv.matches('/').count() == 2) {
                continue;
            }
            let Some(url) = entry.get("serverRelativeURL").and_then(Json::as_str) else {
                continue;
            };
            paths.insert(gv.clone(), self.get_json(url).await?);
        }
        let document = json!({ "paths": paths });
        let cached = json!({"server": self.url, "index": index, "document": document});
        let text = serde_json::to_string(&cached)?;
        if let Some(cache) = cache {
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::write(cache, &text).with_context(|| format!("write {}", cache.display()))?;
        }
        Ok(Document {
            hash: dform_core::zset::file::fnv64(text.as_bytes()),
            text: None,
            parsed: Some(document),
        })
    }

    /// The object, or `None` if there is none.
    pub async fn get(&self, kind: &Kind, ns: &str, name: &str) -> Result<Option<Json>> {
        let req = Request::new(kind.collection(ns))
            .get(name, &GetParams::default())
            .map_err(|e| anyhow!("{e}"))?;
        match self.client.request::<Json>(req).await {
            Ok(o) => Ok(Some(o)),
            Err(kube::Error::Api(s)) if s.code == 404 => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "GET {} {}",
                kind.kind,
                crate::object::remote(kind, ns, name)
            ))),
        }
    }

    /// The objects of `kind` in every namespace whose labels match
    /// `selector` (all of them without one).
    pub async fn list(&self, kind: &Kind, selector: Option<&str>) -> Result<Vec<Json>> {
        let mut lp = ListParams::default();
        if let Some(s) = selector {
            lp = lp.labels(s);
        }
        let req = Request::new(kind.collection_all())
            .list(&lp)
            .map_err(|e| anyhow!("{e}"))?;
        let list = self
            .client
            .request::<Json>(req)
            .await
            .with_context(|| format!("LIST {}", kind.kind))?;
        Ok(match list.get("items") {
            Some(Json::Array(items)) => items.clone(),
            _ => Vec::new(),
        })
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
        let mut pp = PatchParams::apply(MANAGER);
        if dry_run {
            pp = pp.dry_run();
        }
        let req = Request::new(kind.collection(ns))
            .patch(name, &pp, &Patch::Apply(obj))
            .map_err(|e| WriteError::Other(anyhow!("{e}")))?;
        self.client.request::<Json>(req).await.map_err(write_error)
    }

    /// Delete with background propagation (the garbage collector removes
    /// what the object owns). Gone already is not an error.
    pub async fn delete(&self, kind: &Kind, ns: &str, name: &str) -> Result<()> {
        let req = Request::new(kind.collection(ns))
            .delete(name, &DeleteParams::background())
            .map_err(|e| anyhow!("{e}"))?;
        match self.client.request::<Json>(req).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
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
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::response::{StatusCause, StatusDetails};

    #[test]
    fn a_conflict_names_the_manager_and_the_field() {
        let s = Status {
            code: 409,
            reason: "Conflict".into(),
            message: "Apply failed with 1 conflict: conflict with \"kubectl\": .spec.replicas"
                .into(),
            details: Some(StatusDetails {
                name: "web".into(),
                group: "apps".into(),
                kind: "deployments".into(),
                uid: String::new(),
                causes: vec![StatusCause {
                    reason: "FieldManagerConflict".into(),
                    message: "conflict with \"kubectl\" using apps/v1".into(),
                    field: ".spec.replicas".into(),
                }],
                retry_after_seconds: 0,
            }),
            ..Default::default()
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
}
