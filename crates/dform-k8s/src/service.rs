//! `dform-provider-k8s`: the plugin protocol over a Kubernetes API server.
//!
//! Configure finds the cluster (`KUBECONFIG`, `~/.kube/config`, or the
//! pod's service account) and derives the schema from its `/openapi/v3`,
//! cached as `k8s-openapi.json` in the cache directory Configure names
//! (`dform.state/cache/`; else beside the world file). With no cluster to reach, or
//! `DFORM_K8S_OFFLINE` set, the provider is offline: the schema is the
//! checked-in snapshot's, Plan diffs locally, and Read, Apply and Import
//! fail naming why. A program that names its cluster
//! (`provider_config("k8s", ...)`) is configured `deferred` first
//! (the snapshot's schema, the static schema of every stable kind,
//! extended by the CRDs its cluster served at the deployment's last run,
//! the environment ignored) and again with its `settings` once they are
//! known (`Cluster::configured`), when the cluster's CRDs are cached
//! (`dform.state/cache/schema/<deployment>/k8s.json`) and served from then
//! on: dform plans a CRD-typed object against them at that tick, untyped
//! in its own schema until the next run loads the cache. A Configure that
//! names `kinds` (a CRD the program made at the last tick defines them,
//! R-126) fetches the cluster's document again until it serves them,
//! within [`KIND_WAIT`].
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
    HELD_ANNOTATION, KEY_ANNOTATION, STACK_LABEL, attrs, attrs_raw, base64, computed,
    idempotency_key, inventory, manifest, parse_remote, remote, stack_label, stamp,
};
use crate::openapi::{self, Derived, Kind};
use anyhow::{Result, anyhow, bail};
use dform_core::plugin::providers::{CREATED, INVENTORY};
use dform_core::plugin::wire;
use dform_core::provider::{self, diff, get_path, marker, set_path};
use dform_core::schema::Schema;
use dform_core::value::Value;
use dform_grpc::pb;
use serde_json::{Value as Json, json};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tonic::{Request, Response, Status};

/// The cluster's OpenAPI document, cached in the run's cache directory.
const OPENAPI_CACHE: &str = "k8s-openapi.json";

/// How long a delete waits for the object to go before it answers.
const DELETE_WAIT: Duration = Duration::from_secs(60);
/// How long a destroy-first replacement waits for the old object to go.
const REPLACE_WAIT: Duration = Duration::from_secs(120);
/// How long a Configure that names the kinds dform expects (a CRD made at
/// the last tick, R-126) waits for the cluster to serve them.
const KIND_WAIT: Duration = Duration::from_secs(30);

/// A configured provider.
pub struct K8s {
    pub derived: Arc<Derived>,
    /// The cluster, or why there is none.
    pub cluster: std::result::Result<Cluster, String>,
    /// The `STACK_LABEL` value every object applied carries: the
    /// deployment's (Configure's `stack`), if it names one.
    pub stack: Option<String>,
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
                            derived: Arc::new(derived),
                            cluster: Ok(c),
                            stack: None,
                        });
                    }
                    Err(e) => format!("{}: {e:#}", c.url),
                },
            }
        };
        if std::env::var_os("DFORM_K8S_OFFLINE").is_none() {
            eprintln!(
                "dform-provider-k8s: offline ({why}): the schema is the snapshot's \
                 (crates/dform-k8s/openapi-snapshot.json); Read, Apply and Import need a cluster"
            );
        }
        Ok(K8s {
            derived: Arc::new(openapi::snapshot_cached(cache.as_deref())?),
            cluster: Err(why),
            stack: None,
        })
    }

    /// Configure for a program that names its cluster itself
    /// (`provider_config("k8s", ...)`): the schema is the snapshot's, the
    /// static schema, extended by the kinds the deployment's cluster served
    /// when it was last configured (its CRDs, `openapi::read_extension`),
    /// until then, and nothing in the environment is contacted.
    pub fn deferred(cache: Option<PathBuf>, stack: Option<&str>) -> Result<K8s> {
        let base = openapi::snapshot_cached(cache.as_deref())?;
        let ext = cache
            .as_deref()
            .zip(stack)
            .and_then(|(c, s)| openapi::read_extension(&openapi::extension_path(c, s), None));
        let derived = match ext {
            Some(ext) => openapi::extended(&base, &ext)?,
            None => base,
        };
        Ok(K8s {
            derived: Arc::new(derived),
            cluster: Err("the program configures it (provider_config) and has not yet".into()),
            stack: None,
        })
    }

    /// The program's cluster answered: cache the kinds it serves beyond the
    /// static schema (its CRDs), for this provider from now on and for the
    /// deployment's next run, which starts `deferred` again. Once per
    /// change of the cluster's document; a cluster that does not answer
    /// keeps the cache it had.
    async fn extend(cluster: &Cluster, cache: &std::path::Path, stack: &str) -> Result<()> {
        let path = openapi::extension_path(cache, stack);
        let doc = cluster.openapi(Some(&cache.join(OPENAPI_CACHE))).await?;
        if openapi::read_extension(&path, Some(&doc.hash)).is_some() {
            return Ok(());
        }
        let hash = doc.hash.clone();
        let full = openapi::derive(&doc.parse()?, &openapi::aliases()?)?;
        let base = openapi::snapshot_cached(Some(cache))?;
        openapi::write_extension(&path, &hash, &openapi::extension(&full, &base)?)
    }

    /// The program's configuration arrived (`Cluster::configured`): this
    /// schema, that cluster.
    pub fn with_cluster(&self, cluster: Cluster) -> K8s {
        K8s {
            derived: self.derived.clone(),
            cluster: Ok(cluster),
            stack: self.stack.clone(),
        }
    }

    /// The schema with `typ`'s rows read.
    fn schema_of(&self, typ: &str) -> Result<Arc<Schema>> {
        self.derived.schema_of([typ])
    }

    /// The computed values of a live object of `typ`.
    fn computed(&self, typ: &str, live: &Json) -> Result<Json> {
        let schema = self.schema_of(typ)?;
        let defaulted: Vec<String> = schema
            .optional_computed_of(typ)
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !p.starts_with("metadata.") && !schema.in_list(typ, p))
            .collect();
        Ok(computed(live, &defaulted))
    }

    /// The object `doc` describes (`object::manifest`), marked with the
    /// deployment it is of.
    fn manifest(&self, kind: &Kind, doc: &Json, ns: &str, name: &str) -> Result<Json> {
        let mut obj = manifest(kind, doc, ns, name)?;
        if let Some(label) = &self.stack {
            stamp(&mut obj, label);
        }
        Ok(obj)
    }

    /// `provider.created(Type, Name, Key)`: the remote id of the object a
    /// Create with idempotency key `key` made, if there is one: of the
    /// objects of the type that carry this deployment's label (every one,
    /// with no label configured), the one whose `KEY_ANNOTATION` is `key`.
    /// A message names the object by its address (`name`), never by its
    /// key, which is dform's own (R-177).
    pub async fn created(&self, typ: &str, name: &str, key: &str) -> Result<Option<String>> {
        let kind = self.derived.kind(typ)?;
        let c = self.cluster(&format!(
            "find what the create of {} made",
            address(typ, name)
        ))?;
        let selector = self.stack.as_ref().map(|l| format!("{STACK_LABEL}={l}"));
        Ok(c.list(kind, selector.as_deref())
            .await?
            .iter()
            .find(|o| idempotency_key(o) == Some(key))
            .map(|o| {
                let s = |p: &str| get_path(o, p).and_then(Json::as_str).unwrap_or("");
                remote(kind, s("metadata.namespace"), s("metadata.name"))
            }))
    }

    /// The inventory rows of `pred` (`cloud_exists`, `cloud_attr`,
    /// `cloud_computed`) for the objects of type `typ` in every namespace,
    /// each named by its remote id; with no type, of every kind that has a
    /// short name (`k8s.service`), under that name.
    pub async fn inventory(&self, pred: &str, typ: Option<&str>) -> Result<Vec<Vec<Value>>> {
        let c = self.cluster(&format!("answer {pred}"))?;
        let types: Vec<String> = match typ {
            Some(t) => vec![t.to_string()],
            None => openapi::aliases()?.into_iter().map(|(a, _)| a).collect(),
        };
        let s = |x: &str| Value::Str(x.to_string());
        let mut rows = Vec::new();
        for t in types {
            let Ok(kind) = self.derived.kind(&t) else {
                continue;
            };
            for o in c.list(kind, None).await? {
                let at = |p: &str| get_path(&o, p).and_then(Json::as_str).unwrap_or("");
                let e = remote(kind, at("metadata.namespace"), at("metadata.name"));
                let (attrs, status) = inventory(&o);
                match pred {
                    "cloud_exists" => rows.push(vec![s(&t), s(&e)]),
                    "cloud_attr" | "cloud_computed" => {
                        let leaves = if pred == "cloud_attr" { attrs } else { status };
                        rows.extend(
                            leaves
                                .into_iter()
                                .map(|(p, v)| vec![s(&t), s(&e), s(&p), v]),
                        );
                    }
                    _ => {}
                }
            }
        }
        Ok(rows)
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
        let c = self.cluster(&format!("read {}", address(typ, name)))?;
        let (ns, n) = parse_remote(kind, remote_id, &c.namespace);
        let Some(o) = c.get(kind, ns, n).await? else {
            return Ok(None);
        };
        Ok(Some((attrs(&o), self.computed(typ, &o)?)))
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
        let schema = &*self.schema_of(typ)?;
        let at = format!("plan {}", address(typ, name));
        let Some(d) = desired else {
            return Ok((diff(schema, typ, prior, None), false));
        };
        if let Some(p) = missing_required(schema, typ, d) {
            bail!("{at}: {}", schema.required(typ, &p));
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
        let obj = self.manifest(kind, d, &ns, n)?;
        match c.apply(kind, &ns, n, &obj, true).await {
            Ok(live) => {
                let after = world_doc(schema, typ, &attrs(&live), &self.computed(typ, &live)?, d);
                let changes = diff(schema, typ, prior, Some(&after));
                let r = replaces(&changes);
                Ok((changes, r))
            }
            Err(WriteError::Immutable(_)) if prior.is_some() => Ok((local, true)),
            // Its namespace does not exist yet (the same apply makes it
            // first): the diff is local.
            Err(WriteError::NotFound(_)) if prior.is_none() => {
                let r = replaces(&local);
                Ok((local, r))
            }
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
        let Some(o) = c.get(kind, ns, n).await? else {
            return Ok(None);
        };
        let name = get_path(&o, "metadata.name")
            .and_then(Json::as_str)
            .unwrap_or(n)
            .to_string();
        Ok(Some((name, attrs(&o), self.computed(typ, &o)?)))
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
        let at = format!("apply {}", address(typ, &req.name));
        let kind = self
            .derived
            .kind(typ)
            .map_err(|e| Failed::Refused(format!("{at}: {e}")))?;
        let c = self
            .cluster(&at)
            .map_err(|e| Failed::Refused(format!("{e:#}")))?;
        let config = &match op {
            pb::Op::Delete => config.clone(),
            _ => self.materialize(c, &at, config).await?,
        };
        let mut notes = Vec::new();
        let live = match op {
            pb::Op::Create => {
                self.create(c, kind, typ, &at, config, &req.idempotency_key)
                    .await?
            }
            pb::Op::Update | pb::Op::Adopt => {
                let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
                // `keep` (R-164): each path applied again as the object has
                // it (a Secret's `stringData` key from its `data`), so the
                // server-side apply leaves it untouched.
                let kept;
                let config = match req.keep.is_empty() {
                    true => config,
                    false => {
                        let live = c
                            .get(kind, ns, n)
                            .await
                            .map_err(|e| refused(&at, e))?
                            .ok_or_else(|| {
                                Failed::Refused(format!("{at}: {} is not there", req.remote))
                            })?;
                        kept = keep(config, &attrs(&live), &req.keep)
                            .map_err(|e| Failed::Refused(format!("{at}: {e}")))?;
                        &kept
                    }
                };
                let obj = self
                    .manifest(kind, config, ns, n)
                    .map_err(|e| refused(&at, e))?;
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
            computed: Some(wire::doc(
                &self
                    .computed(typ, &live)
                    .map_err(|e| Failed::MaybeApplied(format!("{at}: {e:#}")))?,
            )),
            elapsed_ms: 0,
            notes,
        })
    }

    /// The document with every secret another stack's object holds
    /// (`{"$secret": L, "held": ..}`, `provider::Held`) read where it is
    /// held, inside this call: an object this provider manages, read from
    /// the cluster (a Secret's `stringData` key from its `data`, decoded).
    /// One held by another provider, or not found, refuses the call naming
    /// the path; nothing is written.
    async fn materialize(
        &self,
        c: &Cluster,
        at: &str,
        doc: &Json,
    ) -> std::result::Result<Json, Failed> {
        fn held_at(v: &Json, path: &str, out: &mut Vec<(String, provider::Held, String)>) {
            if let Some(h) = provider::held(v) {
                let l = marker(v).map(|(_, l)| l.to_string()).unwrap_or_default();
                out.push((path.to_string(), h, l));
                return;
            }
            let join = |k: &str| match path {
                "" => k.to_string(),
                p => format!("{p}.{k}"),
            };
            match v {
                Json::Object(m) => m.iter().for_each(|(k, x)| held_at(x, &join(k), out)),
                Json::Array(xs) => xs
                    .iter()
                    .enumerate()
                    .for_each(|(i, x)| held_at(x, &join(&i.to_string()), out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        held_at(doc, "", &mut found);
        let mut out = doc.clone();
        if found.is_empty() {
            return Ok(out);
        }
        // Where each was, for `attrs` to read back as the marker.
        let markers: serde_json::Map<String, Json> = found
            .iter()
            .filter_map(|(p, _, _)| Some((p.clone(), get_path(doc, p)?.clone())))
            .collect();
        for (path, h, label) in found {
            let refuse = |why: String| {
                Failed::Refused(format!(
                    "{at}: {path}: the secret {label} cannot be read: {why}"
                ))
            };
            if h.provider != openapi::PROVIDER {
                return Err(refuse(format!(
                    "it is held by the provider {}, not this one",
                    h.provider
                )));
            }
            let kind = self
                .derived
                .kind(&h.typ)
                .map_err(|e| refuse(format!("{e:#}")))?;
            let (ns, n) = parse_remote(kind, &h.remote, &c.namespace);
            let live = c
                .get(kind, ns, n)
                .await
                .map_err(|e| refuse(format!("{e:#}")))?
                .ok_or_else(|| refuse(format!("{} {} is not in the cluster", h.typ, h.remote)))?;
            let v = get_path(&attrs_raw(&live), &h.path).cloned().or_else(|| {
                let k = h.path.strip_prefix("stringData.")?;
                let text = base64(live.get("data")?.get(k)?.as_str()?)?;
                String::from_utf8(text).ok().map(Json::String)
            });
            let Some(v) = v.filter(|v| marker(v).is_none()) else {
                return Err(refuse(format!(
                    "{} {} does not set {}",
                    h.typ, h.remote, h.path
                )));
            };
            set_path(&mut out, &path, v);
        }
        let annotations = match get_path(&out, "metadata.annotations") {
            Some(Json::Object(a)) => a.clone(),
            _ => serde_json::Map::new(),
        };
        let mut annotations = annotations;
        annotations.insert(
            HELD_ANNOTATION.into(),
            Json::String(Json::Object(markers).to_string()),
        );
        set_path(&mut out, "metadata.annotations", Json::Object(annotations));
        Ok(out)
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
        let mut obj = self
            .manifest(kind, config, &ns, &name)
            .map_err(|e| refused(at, e))?;
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
/// `config` with each `keep` path as the object has it (`live`, its
/// attributes as Read answers them); an error naming a path the object
/// has no value at.
fn keep(config: &Json, live: &Json, keep: &[String]) -> std::result::Result<Json, String> {
    let mut out = config.clone();
    for p in keep {
        match get_path(live, p) {
            Some(v) => set_path(&mut out, p, v.clone()),
            None => return Err(format!("keep {p}: the object has no value there to keep")),
        }
    }
    Ok(out)
}

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
            capabilities: vec![
                "resource".into(),
                "managed".into(),
                "inventory".into(),
                "keep".into(),
            ],
            version: dform_core::plugin::backend::BUILD.into(),
            settings: crate::cluster::SETTINGS
                .iter()
                .map(|&(name, sensitive)| pb::SettingDecl {
                    name: name.into(),
                    sensitive,
                })
                .collect(),
        }))
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Reply<pb::ConfigureResponse> {
        let config = doc_of(req.into_inner().config.as_ref())?.unwrap_or(json!({}));
        // The run's cache directory (`dform.state/cache/`), else beside the
        // world file.
        let cache = config
            .get("cache")
            .and_then(Json::as_str)
            .map(PathBuf::from)
            .or_else(|| {
                config
                    .get("world")
                    .and_then(Json::as_str)
                    .and_then(|w| std::path::Path::new(w).parent().map(PathBuf::from))
            });
        // The deployment: what its objects are labelled with, and what its
        // cluster's extension of the schema is cached under.
        let stack = config
            .get("stack")
            .and_then(Json::as_str)
            .map(stack_label)
            .filter(|s| !s.is_empty());
        // The program's own configuration, a second Configure: the schema
        // the run already has, the cluster it names.
        let settings = config.get("settings").cloned().unwrap_or(Json::Null);
        let configured = Cluster::configured(&settings).await.map_err(invalid)?;
        // The kinds dform expects the cluster to serve now (R-126): a CRD
        // the program made at the last tick defines them, and the API
        // server publishes a new kind a moment after it accepts the CRD.
        let kinds: Vec<&str> = config
            .get("kinds")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        if let (Some(c), Some(dir), Some(s)) = (&configured, &cache, &stack) {
            let deadline = std::time::Instant::now() + KIND_WAIT;
            loop {
                if let Err(e) = K8s::extend(c, dir, s).await {
                    eprintln!("dform-provider-k8s: the cluster's schema is not cached: {e:#}");
                }
                let served = || -> Result<bool> {
                    let k = K8s::deferred(cache.clone(), Some(s))?;
                    Ok(kinds.iter().all(|t| k.derived.kinds.contains_key(*t)))
                };
                if kinds.is_empty()
                    || served().map_err(invalid)?
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        // The cluster reached: the static schema extended by the kinds it
        // serves (just cached), so a CRD the run's schema lacks is planned
        // against it at this tick (R-45).
        let k8s = match configured {
            Some(c) => K8s::deferred(cache, stack.as_deref())
                .map_err(invalid)?
                .with_cluster(c),
            None if config.get("deferred") == Some(&Json::Bool(true)) => {
                K8s::deferred(cache, stack.as_deref()).map_err(invalid)?
            }
            // The cluster the environment names: its document fetched
            // again until it serves the kinds dform expects (R-126).
            None => {
                let deadline = std::time::Instant::now() + KIND_WAIT;
                loop {
                    let k = K8s::configure(cache.clone()).await.map_err(invalid)?;
                    if k.cluster.is_err()
                        || kinds.iter().all(|t| k.derived.kinds.contains_key(*t))
                        || std::time::Instant::now() >= deadline
                    {
                        break k;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let k8s = K8s {
            stack: stack.or(k8s.stack),
            ..k8s
        };
        *self.k8s.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(k8s));
        Ok(Response::new(pb::ConfigureResponse::default()))
    }

    async fn schema(&self, req: Request<pb::SchemaRequest>) -> Reply<pb::SchemaResponse> {
        let k8s = self.k8s()?;
        let schema = match &req.get_ref().types {
            Some(t) => k8s.derived.schema_of(t.names.iter().map(String::as_str)),
            None => k8s.derived.schema(),
        }
        .map_err(invalid)?;
        let facts = wire::schema_facts(&schema, req.get_ref()).map_err(invalid)?;
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
        let r = req.into_inner();
        if INVENTORY.iter().any(|(p, _)| *p == r.pred) {
            let k8s = self.k8s()?;
            // Offline, the inventory is empty: a plan reads no world.
            if k8s.cluster.is_err() {
                return Ok(Response::new(
                    tonic::codegen::tokio_stream::iter(Vec::new()),
                ));
            }
            let typ = match (r.input.first(), r.inputs.first().map(wire::from_value)) {
                (Some(true), Some(Ok(Value::Str(t)))) => Some(t),
                _ => None,
            };
            let rows = k8s
                .inventory(&r.pred, typ.as_deref())
                .await
                .map_err(|e| Status::unavailable(format!("{e:#}")))?
                .into_iter()
                .map(|row| {
                    Ok(pb::Row {
                        values: row.iter().map(wire::value).collect(),
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Response::new(tonic::codegen::tokio_stream::iter(rows)));
        }
        if r.pred != CREATED {
            return Err(Status::unimplemented(format!(
                "the Kubernetes provider answers no extern ({})",
                r.pred
            )));
        }
        let k8s = self.k8s()?;
        let args = r
            .inputs
            .iter()
            .map(wire::from_value)
            .collect::<Result<Vec<_>>>()
            .map_err(invalid)?;
        let [Value::Str(typ), Value::Str(name), Value::Str(key)] = args.as_slice() else {
            return Err(Status::invalid_argument(format!(
                "{CREATED} is asked with Type, Name and Key bound"
            )));
        };
        let found = k8s
            .created(typ, name, key)
            .await
            .map_err(|e| Status::unavailable(format!("{e:#}")))?;
        let s = |x: &str| wire::value(&Value::Str(x.to_string()));
        let rows = found
            .map(|remote| {
                Ok(pb::Row {
                    values: vec![s(typ), s(name), s(key), s(&remote)],
                })
            })
            .into_iter()
            .collect::<Vec<_>>();
        Ok(Response::new(tonic::codegen::tokio_stream::iter(rows)))
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

    type ApplyStream = dform_grpc::server::ApplyStream;

    /// The result alone: a Kubernetes object is there once the API server
    /// takes it (R-130's events are for a provider that waits).
    async fn apply(&self, req: Request<pb::ApplyRequest>) -> Reply<Self::ApplyStream> {
        let k8s = self.k8s()?;
        let r = req.into_inner();
        let config = doc_of(r.config.as_ref())?.unwrap_or(Json::Null);
        let result = k8s.apply(&r, &config).await.map_err(Status::from);
        Ok(Response::new(dform_grpc::server::apply_answer(result)))
    }

    /// No kind of this provider is held for another to read: a secret in
    /// an object is the program's, sent in its document.
    async fn reveal(&self, req: Request<pb::RevealRequest>) -> Reply<pb::RevealResponse> {
        let h = req.into_inner().held.unwrap_or_default();
        Err(Status::failed_precondition(format!(
            "reveal {} {}#{}: the k8s provider holds no secret",
            h.r#type, h.remote, h.path
        )))
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

/// A resource's address as dform prints it, `T["N"]`.
fn address(typ: &str, name: &str) -> dform_core::ir::Address {
    dform_core::ir::Address {
        typ: typ.to_string(),
        name: name.to_string(),
    }
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
        assert_eq!(missing_required(&s.schema().unwrap(), typ, &ok), None);
        let mut no_selector = ok.clone();
        no_selector["spec"]
            .as_object_mut()
            .unwrap()
            .remove("selector");
        assert_eq!(
            missing_required(&s.schema().unwrap(), typ, &no_selector).as_deref(),
            Some("spec.selector")
        );
        let mut unnamed = ok.clone();
        unnamed["spec"]["template"]["spec"]["containers"][0]
            .as_object_mut()
            .unwrap()
            .remove("name");
        assert_eq!(
            missing_required(&s.schema().unwrap(), typ, &unnamed).as_deref(),
            Some("spec.template.spec.containers.name")
        );
        // No spec at all: nothing under it is checked here (the API server
        // refuses it at the dry run).
        assert_eq!(
            missing_required(
                &s.schema().unwrap(),
                typ,
                &json!({"metadata": {"name": "a"}})
            ),
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

    /// A Secret's key kept (R-164): applied again as the cluster has it,
    /// decoded from `data` (what `attrs` reads `stringData` as), the
    /// rest of the update as sent.
    #[test]
    fn a_kept_secret_key_is_applied_as_the_object_has_it() {
        let live = json!({"kind": "Secret", "apiVersion": "v1",
            "metadata": {"name": "db", "namespace": "shop", "managedFields": [{
                "manager": "dform", "operation": "Apply", "fieldsType": "FieldsV1",
                "fieldsV1": {"f:stringData": {"f:password": {}}}}]},
            "data": {"password": "aHVudGVyMg=="}});
        let update = json!({"metadata": {"name": "db", "namespace": "shop",
            "labels": {"team": "shop"}}});
        let kept = keep(&update, &attrs(&live), &["stringData.password".into()]).unwrap();
        assert_eq!(kept["stringData"]["password"], "hunter2");
        assert_eq!(kept["metadata"]["labels"]["team"], "shop");
        let e = keep(&update, &attrs(&live), &["stringData.token".into()]).unwrap_err();
        assert!(e.contains("keep stringData.token"), "{e}");
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
