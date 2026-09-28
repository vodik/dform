//! `dform-provider-k8s`: the plugin protocol over a Kubernetes API server.
//!
//! Configure finds the cluster (`KUBECONFIG`, `~/.kube/config`, or the
//! pod's service account) and derives the schema from its `/openapi/v3`,
//! cached as `k8s-openapi.json` in the stack's state directory (beside the
//! world file Configure names). With no cluster to reach, or
//! `DFORM_K8S_OFFLINE` set, the provider is offline: the schema is the
//! checked-in snapshot's, Plan diffs locally, and Read, Apply and Import
//! fail naming why.
//!
//! Remote ids are `NAMESPACE/NAME` (`NAME` for a cluster-scoped kind).
//! Read and Import GET the object. Plan validates the document, then
//! server-side applies it with `dryRun=All` and diffs the fields it would
//! own against the world's; a field the server will not change in place is
//! a replacement. Apply server-side applies as the field manager `dform`
//! with force off, so a field another manager owns fails the action naming
//! the manager and the field. A generated name (`metadata.generateName`) is
//! picked here, since server-side apply needs a name. Delete propagates in
//! the background and waits (bounded) for the object to go.

use crate::cluster::{Cluster, WriteError};
use crate::object::{
    KEY_ANNOTATION, attrs, computed, idempotency_key, manifest, parse_remote, remote,
};
use crate::openapi::{self, Derived, Kind};
use anyhow::{Result, anyhow, bail};
use dform_core::plugin::wire;
use dform_core::provider::{self, diff, get_path, marker, set_path};
use dform_core::schema::Schema;
use dform_grpc::pb;
use serde_json::{Value as Json, json};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tonic::{Request, Response, Status};

/// The cluster's OpenAPI document, cached in the stack's state directory.
const OPENAPI_CACHE: &str = "k8s-openapi.json";

/// How long a delete waits for the object to go before it answers.
const DELETE_WAIT: Duration = Duration::from_secs(60);
/// How long a destroy-first replacement waits for the old object to go.
const REPLACE_WAIT: Duration = Duration::from_secs(120);

/// A configured provider.
pub struct K8s {
    pub derived: Derived,
    /// The cluster, or why there is none.
    pub cluster: std::result::Result<Cluster, String>,
}

impl K8s {
    /// Configure: the cluster the environment names and its schema, else
    /// offline on the snapshot. `cache` is the directory the OpenAPI
    /// document and the schema derived from it are cached in.
    pub async fn configure(cache: Option<PathBuf>) -> Result<K8s> {
        let why = if std::env::var_os("DFORM_K8S_OFFLINE").is_some_and(|v| !v.is_empty()) {
            "DFORM_K8S_OFFLINE is set".to_string()
        } else {
            match Cluster::infer().await {
                Err(e) => format!("{e:#}"),
                Ok(c) => match c
                    .openapi(cache.as_deref().map(|d| d.join(OPENAPI_CACHE)).as_deref())
                    .await
                {
                    Ok(doc) => {
                        let derived = openapi::cached(&doc.hash.clone(), cache.as_deref(), || {
                            openapi::derive(&doc.parse()?, &openapi::aliases()?)
                        })?;
                        return Ok(K8s {
                            derived,
                            cluster: Ok(c),
                        });
                    }
                    Err(e) => format!("{}: {e:#}", c.url),
                },
            }
        };
        if std::env::var_os("DFORM_K8S_OFFLINE").is_none() {
            eprintln!(
                "dform-provider-k8s: offline ({why}): the schema is the snapshot's \
                 (providers/k8s/openapi-snapshot.json); Read, Apply and Import need a cluster"
            );
        }
        Ok(K8s {
            derived: openapi::snapshot_cached(cache.as_deref())?,
            cluster: Err(why),
        })
    }

    fn schema(&self) -> &Schema {
        &self.derived.schema
    }

    /// The computed values of a live object of `typ`.
    fn computed(&self, typ: &str, live: &Json) -> Json {
        let schema = self.schema();
        let defaulted: Vec<String> = schema
            .optional_computed_of(typ)
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !p.starts_with("metadata.") && !schema.in_list(typ, p))
            .collect();
        computed(live, &defaulted)
    }

    fn cluster(&self, what: &str) -> Result<&Cluster> {
        self.cluster
            .as_ref()
            .map_err(|why| anyhow!("{what}: no cluster ({why})"))
    }

    pub async fn read(
        &self,
        typ: &str,
        remote_id: &str,
        name: &str,
    ) -> Result<Option<(Json, Json)>> {
        let kind = self.derived.kind(typ)?;
        let c = self.cluster(&format!("read {typ}/{name}"))?;
        let (ns, n) = parse_remote(kind, remote_id, &c.namespace);
        Ok(c.get(kind, ns, n)
            .await?
            .map(|o| (attrs(&o), self.computed(typ, &o))))
    }

    /// Plan one resource: validate, diff, and whether it replaces. The dry
    /// run names the object by the document, else by `remote` (a generated
    /// name, or a namespace the program leaves to the kubeconfig, is not in
    /// the document once the object exists).
    pub async fn plan(
        &self,
        typ: &str,
        name: &str,
        remote_id: &str,
        prior: Option<&Json>,
        desired: Option<&Json>,
    ) -> Result<(Vec<provider::Change>, bool)> {
        let kind = self.derived.kind(typ)?;
        let schema = self.schema();
        let at = format!("plan {typ}/{name}");
        let Some(d) = desired else {
            return Ok((diff(schema, typ, prior, None), false));
        };
        if let Some(p) = missing_required(schema, typ, d) {
            bail!("{at}: required attribute {p} is not set");
        }
        if get_path(d, "metadata.name").is_none() && get_path(d, "metadata.generateName").is_none()
        {
            bail!("{at}: metadata.name is not set, nor metadata.generateName");
        }
        let replaces = |changes: &[provider::Change]| {
            prior.is_some()
                && changes
                    .iter()
                    .any(|c| schema.forces_new(typ, &provider::norm_path(&c.path)))
        };
        let local = diff(schema, typ, prior, Some(d));
        // A dry run needs a known document with a name.
        let Ok(c) = &self.cluster else {
            let r = replaces(&local);
            return Ok((local, r));
        };
        let (remote_ns, remote_name) = match remote_id {
            "" => (None, None),
            r => {
                let (ns, n) = parse_remote(kind, r, &c.namespace);
                (Some(ns), Some(n))
            }
        };
        let n = match get_path(d, "metadata.name") {
            Some(Json::String(n)) => Some(n.as_str()),
            None => remote_name,
            Some(_) => None,
        };
        let Some(n) = n.filter(|_| !has_marker(d)) else {
            let r = replaces(&local);
            return Ok((local, r));
        };
        let ns = match (get_path(d, "metadata.namespace"), remote_ns) {
            (None, Some(ns)) => ns.to_string(),
            _ => namespace(d, &c.namespace),
        };
        let obj = manifest(kind, d, &ns, n)?;
        match c.apply(kind, &ns, n, &obj, true).await {
            Ok(live) => {
                let after = world_doc(schema, typ, &attrs(&live), &self.computed(typ, &live), d);
                let changes = diff(schema, typ, prior, Some(&after));
                let r = replaces(&changes);
                Ok((changes, r))
            }
            Err(WriteError::Immutable(_)) if prior.is_some() => Ok((local, true)),
            // Apply fails on it, naming the manager and the field.
            Err(WriteError::Conflict(_)) => {
                let r = replaces(&local);
                Ok((local, r))
            }
            Err(e) => bail!("{at}: the API server refuses it: {e}"),
        }
    }

    pub async fn import(&self, typ: &str, remote_id: &str) -> Result<Option<(String, Json, Json)>> {
        let kind = self.derived.kind(typ)?;
        let c = self.cluster(&format!("import {typ} {remote_id}"))?;
        let (ns, n) = parse_remote(kind, remote_id, &c.namespace);
        Ok(c.get(kind, ns, n).await?.map(|o| {
            let name = get_path(&o, "metadata.name")
                .and_then(Json::as_str)
                .unwrap_or(n)
                .to_string();
            (name, attrs(&o), self.computed(typ, &o))
        }))
    }

    /// Apply one planned action.
    pub async fn apply(
        &self,
        req: &pb::ApplyRequest,
        config: &Json,
    ) -> std::result::Result<pb::ApplyResponse, Failed> {
        let op = pb::Op::try_from(req.op).unwrap_or(pb::Op::Unspecified);
        if op == pb::Op::EndTick {
            return Ok(pb::ApplyResponse::default());
        }
        let typ = req.r#type.as_str();
        let at = format!("apply {typ}/{}", req.name);
        let kind = self
            .derived
            .kind(typ)
            .map_err(|e| Failed::Refused(format!("{at}: {e}")))?;
        let c = self
            .cluster(&at)
            .map_err(|e| Failed::Refused(format!("{e:#}")))?;
        let mut notes = Vec::new();
        let live = match op {
            pb::Op::Create => {
                self.create(c, kind, typ, &at, config, &req.idempotency_key)
                    .await?
            }
            pb::Op::Update | pb::Op::Adopt => {
                let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
                let obj = manifest(kind, config, ns, n).map_err(|e| refused(&at, e))?;
                self.write(c, kind, ns, n, &obj, &at).await?
            }
            pb::Op::Delete => {
                let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
                c.delete(kind, ns, n).await.map_err(|e| maybe(&at, e))?;
                if !c
                    .gone(kind, ns, n, DELETE_WAIT)
                    .await
                    .map_err(|e| maybe(&at, e))?
                {
                    notes.push(format!(
                        "{at}: {} is still terminating after {}s",
                        req.remote,
                        DELETE_WAIT.as_secs()
                    ));
                }
                return Ok(pb::ApplyResponse {
                    notes,
                    ..Default::default()
                });
            }
            pb::Op::Replace => {
                let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
                let new_ns = namespace(config, &c.namespace);
                let same = get_path(config, "metadata.name").and_then(Json::as_str) == Some(n)
                    && (!kind.namespaced || new_ns == ns);
                if req.create_first && same {
                    notes.push(format!(
                        "{at}: the replacement has the old object's name, so the old one goes \
                         first"
                    ));
                }
                if !req.create_first || same {
                    c.delete(kind, ns, n).await.map_err(|e| maybe(&at, e))?;
                    if !c
                        .gone(kind, ns, n, REPLACE_WAIT)
                        .await
                        .map_err(|e| maybe(&at, e))?
                    {
                        return Err(Failed::MaybeApplied(format!(
                            "{at}: {} is still terminating after {}s; its replacement is not \
                             created",
                            req.remote,
                            REPLACE_WAIT.as_secs()
                        )));
                    }
                }
                self.create(c, kind, typ, &at, config, &req.idempotency_key)
                    .await?
            }
            _ => return Err(Failed::Refused(format!("{at}: no operation"))),
        };
        let name = get_path(&live, "metadata.name")
            .and_then(Json::as_str)
            .unwrap_or("");
        let ns = get_path(&live, "metadata.namespace")
            .and_then(Json::as_str)
            .unwrap_or("");
        Ok(pb::ApplyResponse {
            remote: remote(kind, ns, name),
            attrs: Some(wire::doc(&attrs(&live))),
            computed: Some(wire::doc(&self.computed(typ, &live))),
            elapsed_ms: 0,
            notes,
        })
    }

    /// A new object: named by the document, or by its `generateName` and a
    /// random suffix. An object of that name is not taken over, unless the
    /// Create that made it carried the same idempotency key `key` (its
    /// `KEY_ANNOTATION`): then it is the answer. A generated name's first
    /// suffix comes from the key, so a Create again finds it by name.
    async fn create(
        &self,
        c: &Cluster,
        kind: &Kind,
        typ: &str,
        at: &str,
        config: &Json,
        key: &str,
    ) -> std::result::Result<Json, Failed> {
        let ns = namespace(config, &c.namespace);
        let named = get_path(config, "metadata.name").and_then(Json::as_str);
        let generate = get_path(config, "metadata.generateName").and_then(Json::as_str);
        let mut name = None;
        for i in 0..5 {
            let n = match (named, generate) {
                (Some(n), _) => n.to_string(),
                (None, Some(g)) if i == 0 && !key.is_empty() => format!("{g}{}", keyed_suffix(key)),
                (None, Some(g)) => format!("{g}{}", suffix()),
                (None, None) => {
                    return Err(Failed::Refused(format!(
                        "{at}: metadata.name is not set, nor metadata.generateName"
                    )));
                }
            };
            match c.get(kind, &ns, &n).await.map_err(|e| refused(at, e))? {
                None => {
                    name = Some(n);
                    break;
                }
                Some(live) if !key.is_empty() && idempotency_key(&live) == Some(key) => {
                    return Ok(live);
                }
                Some(_) if named.is_some() => {
                    return Err(Failed::Refused(format!(
                        "{at}: {typ} {} already exists; adopt it or name another",
                        remote(kind, &ns, &n)
                    )));
                }
                Some(_) => {}
            }
        }
        let Some(name) = name else {
            return Err(Failed::Refused(format!(
                "{at}: no free name from generateName after 5 tries"
            )));
        };
        let mut obj = manifest(kind, config, &ns, &name).map_err(|e| refused(at, e))?;
        if !key.is_empty()
            && let Some(meta) = obj.get_mut("metadata").and_then(Json::as_object_mut)
        {
            let annotations = meta.entry("annotations").or_insert_with(|| json!({}));
            if let Json::Object(a) = annotations {
                a.insert(KEY_ANNOTATION.into(), json!(key));
            }
        }
        self.write(c, kind, &ns, &name, &obj, at).await
    }

    async fn write(
        &self,
        c: &Cluster,
        kind: &Kind,
        ns: &str,
        name: &str,
        obj: &Json,
        at: &str,
    ) -> std::result::Result<Json, Failed> {
        c.apply(kind, ns, name, obj, false)
            .await
            .map_err(|e| match e {
                WriteError::Transport(_) => Failed::MaybeApplied(format!("{at}: {e}")),
                _ => Failed::Refused(format!("{at}: {e}")),
            })
    }

    /// Documents of the kinds for `dform provider check`.
    pub fn examples(&self) -> Vec<pb::Example> {
        let ex = |typ: String, create: Json, update: Json, required: &str| pb::Example {
            r#type: typ,
            create: Some(wire::doc(&create)),
            update: Some(wire::doc(&update)),
            required: required.into(),
        };
        let meta = json!({"name": "dform-check", "labels": {"app": "dform-check"}});
        let deployment = |image: &str| {
            json!({"metadata": meta, "spec": {
                "selector": {"matchLabels": {"app": "dform-check"}},
                "template": {"metadata": {"labels": {"app": "dform-check"}},
                             "spec": {"containers": [{"name": "app", "image": image}]}}}})
        };
        let service = |port: i64| {
            json!({"metadata": meta, "spec": {"selector": {"app": "dform-check"},
                   "ports": [{"name": "http", "port": port, "protocol": "TCP"}]}})
        };
        let t = openapi::type_name;
        vec![
            ex(
                t("", "v1", "ConfigMap"),
                json!({"metadata": meta, "data": {"mode": "a"}}),
                json!({"metadata": meta, "data": {"mode": "b"}}),
                "",
            ),
            ex(t("", "v1", "Service"), service(80), service(8080), ""),
            ex(
                t("", "v1", "Secret"),
                json!({"metadata": meta, "stringData": {"token": "a"}}),
                json!({"metadata": meta, "stringData": {"token": "b"}}),
                "",
            ),
            ex(
                t("apps", "v1", "Deployment"),
                deployment("nginx:1"),
                deployment("nginx:2"),
                "spec.selector",
            ),
        ]
        .into_iter()
        .filter(|e| self.derived.kinds.contains_key(&e.r#type))
        .collect()
    }
}

/// Why an Apply failed: `Refused` changed nothing; `MaybeApplied` may have
/// taken effect (the call's outcome is unknown).
#[derive(Debug)]
pub enum Failed {
    Refused(String),
    MaybeApplied(String),
}

impl From<Failed> for Status {
    fn from(f: Failed) -> Status {
        match f {
            Failed::Refused(m) => Status::failed_precondition(m),
            Failed::MaybeApplied(m) => Status::deadline_exceeded(m),
        }
    }
}

fn refused(at: &str, e: anyhow::Error) -> Failed {
    Failed::Refused(format!("{at}: {e:#}"))
}

fn maybe(at: &str, e: anyhow::Error) -> Failed {
    Failed::MaybeApplied(format!("{at}: {e:#}"))
}

/// The document's namespace, else the kubeconfig's.
fn namespace(doc: &Json, default: &str) -> String {
    get_path(doc, "metadata.namespace")
        .and_then(Json::as_str)
        .unwrap_or(default)
        .to_string()
}

fn has_marker(v: &Json) -> bool {
    marker(v).is_some()
        || match v {
            Json::Object(m) => m.values().any(has_marker),
            Json::Array(xs) => xs.iter().any(has_marker),
            _ => false,
        }
}

/// Five characters as the API server picks them for `generateName`.
/// A generated name's suffix from an idempotency key: the same key, the
/// same name.
fn keyed_suffix(key: &str) -> String {
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let mut n = key.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    (0..5)
        .map(|_| {
            let c = ALPHABET[(n % ALPHABET.len() as u64) as usize] as char;
            n /= ALPHABET.len() as u64;
            c
        })
        .collect()
}

fn suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let mut n = h.finish();
    (0..5)
        .map(|_| {
            let c = ALPHABET[(n % ALPHABET.len() as u64) as usize] as char;
            n /= ALPHABET.len() as u64;
            c
        })
        .collect()
}

/// A `required` attribute the document leaves out where its parent is set:
/// the schema path of the first one.
pub fn missing_required(schema: &Schema, typ: &str, doc: &Json) -> Option<String> {
    fn lacks(v: &Json, segs: &[&str]) -> bool {
        if marker(v).is_some() {
            return false;
        }
        if let Json::Array(xs) = v {
            return xs.iter().any(|x| lacks(x, segs));
        }
        match segs {
            [] => false,
            [last] => v.is_object() && v.get(*last).is_none(),
            [first, rest @ ..] => v.get(*first).is_some_and(|x| lacks(x, rest)),
        }
    }
    schema
        .attrs
        .iter()
        .filter(|((t, _), a)| t == typ && a.has("required"))
        .map(|((_, p), _)| p)
        .find(|p| lacks(doc, &p.split('.').collect::<Vec<_>>()))
        .cloned()
}

/// The world's side as the engine compares it: the configured attributes,
/// plus each Optional+Computed value the desired document sets.
fn world_doc(schema: &Schema, typ: &str, attrs: &Json, computed: &Json, desired: &Json) -> Json {
    let mut out = attrs.clone();
    for (p, _) in schema.optional_computed_of(typ) {
        if get_path(desired, &p).is_some()
            && get_path(&out, &p).is_none()
            && let Some(v) = get_path(computed, &p)
        {
            set_path(&mut out, &p, v.clone());
        }
    }
    out
}

type Reply<T> = std::result::Result<Response<T>, Status>;

fn invalid(e: anyhow::Error) -> Status {
    Status::invalid_argument(format!("{e:#}"))
}

#[allow(clippy::result_large_err)] // tonic's own error type
fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, Status> {
    v.map(wire::from_doc).transpose().map_err(invalid)
}

/// The gRPC service: the provider once Configure has run.
#[derive(Default)]
pub struct Service {
    k8s: RwLock<Option<Arc<K8s>>>,
}

impl Service {
    #[allow(clippy::result_large_err)] // tonic's own error type
    fn k8s(&self) -> std::result::Result<Arc<K8s>, Status> {
        self.k8s
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Status::failed_precondition("the provider is not configured"))
    }
}

#[tonic::async_trait]
#[allow(clippy::result_large_err)] // tonic's own error type
impl pb::provider_server::Provider for Service {
    async fn handshake(&self, req: Request<pb::HandshakeRequest>) -> Reply<pb::HandshakeResponse> {
        let v = req.into_inner().protocol_version;
        if v != dform_grpc::spawn::VERSION {
            return Err(Status::failed_precondition(format!(
                "this provider speaks protocol version {}, not {v}",
                dform_grpc::spawn::VERSION
            )));
        }
        Ok(Response::new(pb::HandshakeResponse {
            protocol_version: dform_grpc::spawn::VERSION,
            name: openapi::PROVIDER.into(),
            capabilities: vec!["resource".into()],
        }))
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Reply<pb::ConfigureResponse> {
        let config = doc_of(req.into_inner().config.as_ref())?.unwrap_or(json!({}));
        let cache = config
            .get("world")
            .and_then(Json::as_str)
            .and_then(|w| std::path::Path::new(w).parent().map(PathBuf::from));
        let k8s = K8s::configure(cache).await.map_err(invalid)?;
        *self.k8s.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(k8s));
        Ok(Response::new(pb::ConfigureResponse {}))
    }

    async fn schema(&self, req: Request<pb::SchemaRequest>) -> Reply<pb::SchemaResponse> {
        let k8s = self.k8s()?;
        let facts = wire::schema_facts(k8s.schema(), req.get_ref()).map_err(invalid)?;
        Ok(Response::new(pb::SchemaResponse {
            facts,
            externs: Vec::new(),
            checks_refinements: false,
            examples: k8s.examples(),
        }))
    }

    type QueryStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<pb::Row, Status>>,
    >;

    async fn query(&self, req: Request<pb::QueryRequest>) -> Reply<Self::QueryStream> {
        Err(Status::unimplemented(format!(
            "the Kubernetes provider answers no extern ({})",
            req.into_inner().pred
        )))
    }

    async fn read(&self, req: Request<pb::ReadRequest>) -> Reply<pb::ReadResponse> {
        let k8s = self.k8s()?;
        let r = req.into_inner();
        let found = k8s
            .read(&r.r#type, &r.remote, &r.name)
            .await
            .map_err(|e| Status::unavailable(format!("{e:#}")))?;
        Ok(Response::new(match found {
            Some((attrs, computed)) => pb::ReadResponse {
                found: true,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ReadResponse::default(),
        }))
    }

    async fn plan(&self, req: Request<pb::PlanRequest>) -> Reply<pb::PlanResponse> {
        let k8s = self.k8s()?;
        let r = req.into_inner();
        let prior = doc_of(r.prior.as_ref())?;
        let desired = doc_of(r.desired.as_ref())?;
        let (changes, requires_replace) = k8s
            .plan(
                &r.r#type,
                &r.name,
                &r.remote,
                prior.as_ref(),
                desired.as_ref(),
            )
            .await
            .map_err(invalid)?;
        Ok(Response::new(pb::PlanResponse {
            changes: changes
                .iter()
                .map(|c| pb::Change {
                    path: c.path.clone(),
                    before: c.before.as_ref().map(wire::doc),
                    after: c.after.as_ref().map(wire::doc),
                    sensitive: c.sensitive,
                })
                .collect(),
            requires_replace,
        }))
    }

    async fn apply(&self, req: Request<pb::ApplyRequest>) -> Reply<pb::ApplyResponse> {
        let k8s = self.k8s()?;
        let r = req.into_inner();
        let config = doc_of(r.config.as_ref())?.unwrap_or(Json::Null);
        Ok(Response::new(k8s.apply(&r, &config).await?))
    }

    async fn import(&self, req: Request<pb::ImportRequest>) -> Reply<pb::ImportResponse> {
        let k8s = self.k8s()?;
        let r = req.into_inner();
        let found = k8s
            .import(&r.r#type, &r.remote)
            .await
            .map_err(|e| Status::unavailable(format!("{e:#}")))?;
        Ok(Response::new(match found {
            Some((name, attrs, computed)) => pb::ImportResponse {
                found: true,
                r#type: r.r#type,
                name,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ImportResponse::default(),
        }))
    }
}

/// Serve as a provider (`dform_grpc::transport`: TCP on the loopback, or a unix
/// socket), and exit when stdin closes.
pub fn serve() -> Result<()> {
    dform_grpc::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(Service::default())
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_holds_where_the_parent_is_set() {
        let s = openapi::snapshot().unwrap();
        let typ = "k8s.apps.v1.deployment";
        let ok = json!({"metadata": {"name": "a"}, "spec": {
            "selector": {"matchLabels": {"app": "a"}},
            "template": {"spec": {"containers": [{"name": "a", "image": "x"}]}}}});
        assert_eq!(missing_required(&s.schema, typ, &ok), None);
        let mut no_selector = ok.clone();
        no_selector["spec"]
            .as_object_mut()
            .unwrap()
            .remove("selector");
        assert_eq!(
            missing_required(&s.schema, typ, &no_selector).as_deref(),
            Some("spec.selector")
        );
        let mut unnamed = ok.clone();
        unnamed["spec"]["template"]["spec"]["containers"][0]
            .as_object_mut()
            .unwrap()
            .remove("name");
        assert_eq!(
            missing_required(&s.schema, typ, &unnamed).as_deref(),
            Some("spec.template.spec.containers.name")
        );
        // No spec at all: nothing under it is checked here (the API server
        // refuses it at the dry run).
        assert_eq!(
            missing_required(&s.schema, typ, &json!({"metadata": {"name": "a"}})),
            None
        );
    }

    #[test]
    fn a_keyed_suffix_is_the_same_for_the_same_key() {
        let a = keyed_suffix("dform-0123");
        assert_eq!(a, keyed_suffix("dform-0123"));
        assert_ne!(a, keyed_suffix("dform-0124"));
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn generated_suffixes_look_like_the_servers() {
        let a = suffix();
        assert_eq!(a.len(), 5);
        assert!(
            a.bytes()
                .all(|b| b"bcdfghjklmnpqrstvwxz2456789".contains(&b))
        );
    }
}
