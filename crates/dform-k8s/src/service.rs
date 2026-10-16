//! `dform-provider-k8s`: the plugin protocol over a Kubernetes API server.
//!
//! Configure finds the cluster (the environment's kubeconfig, which the
//! host opens for it where dform.toml grants it, `credentials =
//! ["kubeconfig:environment"]`: `KUBECONFIG`, `~/.kube/config`, or the
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
//!
//! Each session (a deployment's Configure) is a [`K8s`] of its own: its
//! cluster, its schema, its deployment's label. A call is answered by its
//! session's, awaiting the cluster's answers: several sent at once where
//! they do not depend on each other (the OpenAPI documents, a health's
//! objects, an inventory's kinds), a poll's interval slept, so a wasm
//! provider answers its other calls meanwhile.

use crate::cluster::{Cluster, WriteError};
use crate::object::{
    HELD_ANNOTATION, KEY_ANNOTATION, STACK_LABEL, attrs, attrs_raw, base64, computed,
    idempotency_key, inventory, manifest, parse_remote, remote, stack_label, stamp,
};
use crate::openapi::{self, Derived, Kind};
use anyhow::{Result, anyhow, bail};
use dform_core::plugin::backend::{
    BUILD, Call, CallError, Configured, Progress, Reply, Session, VERSION,
};
use dform_core::plugin::pb;
use dform_core::plugin::providers::{CREATED, INVENTORY};
use dform_core::plugin::wire;
use dform_core::provider::{self, diff, get_path, marker, set_path};
use dform_core::schema::Schema;
use dform_core::value::Value;
use dform_sdk::{Handler, join_all};
use serde_json::{Value as Json, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

/// What a Configure asks of its session, read from its document.
struct Asked {
    /// The run's cache directory (`dform.state/cache/`), else beside the
    /// world file.
    cache: Option<PathBuf>,
    /// The deployment: what its objects are labelled with, and what its
    /// cluster's extension of the schema is cached under.
    stack: Option<String>,
    /// The program's own configuration, a second Configure: the cluster it
    /// names, its kubeconfig held by the host.
    settings: Json,
    /// The program configures it and has not yet: no cluster now.
    deferred: bool,
    /// The environment's kubeconfig, by the name the host opens it by
    /// where dform.toml grants it, for a provider the program does not
    /// configure.
    kubeconfig: Option<String>,
    /// The kinds dform expects the cluster to serve now (R-126): a CRD the
    /// program made at the last tick defines them, and the API server
    /// publishes a new kind a moment after it accepts the CRD.
    kinds: Vec<String>,
}

impl Asked {
    fn of(config: &Json) -> Asked {
        let text = |k: &str| config.pointer(k).and_then(Json::as_str);
        Asked {
            cache: text("/cache").map(PathBuf::from).or_else(|| {
                text("/world").and_then(|w| std::path::Path::new(w).parent().map(PathBuf::from))
            }),
            stack: text("/stack").map(stack_label).filter(|s| !s.is_empty()),
            settings: config.get("settings").cloned().unwrap_or(Json::Null),
            deferred: config.get("deferred") == Some(&Json::Bool(true)),
            kubeconfig: text(&format!("/credentials/{}", crate::cluster::CREDENTIAL))
                .map(String::from),
            kinds: config
                .get("kinds")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(Json::as_str)
                .map(String::from)
                .collect(),
        }
    }

    /// Whether `k8s` serves every kind asked for.
    fn served(&self, k8s: &K8s) -> bool {
        self.kinds.iter().all(|t| k8s.derived.kinds.contains_key(t))
    }
}

/// Say `line` on stderr, one sentence of dform's; the client's own chain
/// of errors behind it (`e`) only under `DFORM_LOG=debug`, the provider
/// being told nothing of the run's `-v`.
fn say(line: &str, e: &anyhow::Error) {
    eprintln!("{line}");
    tracing::debug!(target: dform_core::timing::TARGET, "k8s: {e:#}");
}

impl K8s {
    /// Configure: the cluster the environment names (its kubeconfig, the
    /// credential `kubeconfig:environment`, which the host opens for a
    /// provider the program does not configure where dform.toml grants it)
    /// and its schema, else offline on the snapshot, saying why (the grant
    /// it lacks). `cache` is the directory the OpenAPI document and the
    /// schema derived from it are cached in.
    pub async fn configure(cache: Option<PathBuf>, kubeconfig: Option<&str>) -> Result<K8s> {
        let why = if std::env::var_os("DFORM_K8S_OFFLINE").is_some_and(|v| !v.is_empty()) {
            "DFORM_K8S_OFFLINE is set".to_string()
        } else {
            let opened = kubeconfig
                .ok_or_else(|| anyhow!("dform holds no kubeconfig for it"))
                .and_then(|name| Cluster::open(name, None));
            let (why, e) = match opened {
                Err(e) => (e.to_string(), e),
                Ok(c) => {
                    let cached = cache.as_deref().map(|d| d.join(OPENAPI_CACHE));
                    match c.openapi(cached.as_deref()).await {
                        Ok(doc) => {
                            let derived =
                                openapi::cached(&doc.hash.clone(), cache.as_deref(), || {
                                    openapi::derive(&doc.parse()?, &openapi::aliases()?)
                                })?;
                            return Ok(K8s {
                                derived: Arc::new(derived),
                                cluster: Ok(c),
                                stack: None,
                            });
                        }
                        Err(e) => (format!("the cluster at {} did not answer", c.url), e),
                    }
                }
            };
            say(
                &format!(
                    "k8s: offline, {why}; planning against the snapshot schema \
                     (crates/dform-k8s/openapi-snapshot.json)"
                ),
                &e,
            );
            why
        };
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

    /// The provider `asked` configures: the program's cluster
    /// ([`K8s::program`]), none yet (deferred), or the environment's,
    /// fetched again until it serves the kinds dform expects (R-126).
    async fn asked(asked: &Asked) -> Result<K8s> {
        let k8s = match Cluster::configured(&asked.settings)? {
            Some(c) => K8s::program(asked, c).await?,
            None if asked.deferred => K8s::deferred(asked.cache.clone(), asked.stack.as_deref())?,
            None => {
                let deadline = std::time::Instant::now() + KIND_WAIT;
                loop {
                    let k =
                        K8s::configure(asked.cache.clone(), asked.kubeconfig.as_deref()).await?;
                    if k.cluster.is_err()
                        || asked.served(&k)
                        || std::time::Instant::now() >= deadline
                    {
                        break k;
                    }
                    dform_sdk::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        Ok(K8s {
            stack: asked.stack.clone().or(k8s.stack),
            ..k8s
        })
    }

    /// The program's cluster `c`: its kinds beyond the static schema
    /// cached ([`K8s::extend`]), again until it serves the kinds dform
    /// expects, and the static schema extended by them, so a CRD the
    /// run's schema lacks is planned against it at this tick (R-45).
    async fn program(asked: &Asked, c: Cluster) -> Result<K8s> {
        if let (Some(dir), Some(s)) = (&asked.cache, &asked.stack) {
            let deadline = std::time::Instant::now() + KIND_WAIT;
            loop {
                if let Err(e) = K8s::extend(&c, dir, s).await {
                    say(
                        &format!("k8s: the schema of the cluster at {} is not cached", c.url),
                        &e,
                    );
                }
                if asked.kinds.is_empty()
                    || asked.served(&K8s::deferred(asked.cache.clone(), Some(s))?)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                dform_sdk::sleep(Duration::from_secs(1)).await;
            }
        }
        Ok(K8s::deferred(asked.cache.clone(), asked.stack.as_deref())?.with_cluster(c))
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
    /// short name (`k8s.service`), under that name, every kind's list
    /// asked for at once.
    pub async fn inventory(&self, pred: &str, typ: Option<&str>) -> Result<Vec<Vec<Value>>> {
        let c = self.cluster(&format!("answer {pred}"))?;
        let types: Vec<String> = match typ {
            Some(t) => vec![t.to_string()],
            None => openapi::aliases()?.into_iter().map(|(a, _)| a).collect(),
        };
        let kinds: Vec<(String, &Kind)> = types
            .into_iter()
            .filter_map(|t| Some((t.clone(), self.derived.kind(&t).ok()?)))
            .collect();
        let lists = join_all(kinds.iter().map(|(_, kind)| c.list(kind, None))).await;
        let s = |x: &str| Value::Str(x.to_string());
        let mut rows = Vec::new();
        for ((t, kind), objects) in kinds.iter().zip(lists) {
            for o in objects? {
                let at = |p: &str| get_path(&o, p).and_then(Json::as_str).unwrap_or("");
                let e = remote(kind, at("metadata.namespace"), at("metadata.name"));
                let (attrs, status) = inventory(&o);
                match pred {
                    "cloud_exists" => rows.push(vec![s(t), s(&e)]),
                    "cloud_attr" | "cloud_computed" => {
                        let leaves = if pred == "cloud_attr" { attrs } else { status };
                        rows.extend(leaves.into_iter().map(|(p, v)| vec![s(t), s(&e), s(&p), v]));
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

    /// Health (R-203): each object GET and judged from its status now
    /// (`crate::health`), every object asked for at once; one of a kind
    /// not judged is `unknown`.
    pub async fn health(&self, objects: &[pb::Identity]) -> Result<Vec<pb::Health>> {
        join_all(objects.iter().map(|o| self.health_of(o)))
            .await
            .into_iter()
            .collect()
    }

    async fn health_of(&self, o: &pb::Identity) -> Result<pb::Health> {
        let kind = self.derived.kind(&o.r#type)?;
        if !crate::health::judged(kind) {
            return Ok(dform_core::plugin::backend::health(
                pb::HealthState::Unknown,
                format!("the provider judges no {}", kind.kind),
            ));
        }
        let c = self.cluster(&format!("health {}", address(&o.r#type, &o.name)))?;
        let (ns, n) = parse_remote(kind, &o.remote, &c.namespace);
        let got = c.get(kind, ns, n).await?;
        Ok(crate::health::judge(kind, got.as_ref()))
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
            pb::Op::Update | pb::Op::Adopt => self.update(c, kind, req, &at, config).await?,
            pb::Op::Delete => {
                return Ok(pb::ApplyResponse {
                    notes: delete(c, kind, &req.remote, &at).await?,
                    ..Default::default()
                });
            }
            pb::Op::Replace => self.replace(c, kind, req, &at, config, &mut notes).await?,
            _ => return Err(Failed::Refused(format!("{at}: no operation"))),
        };
        self.applied(kind, typ, &at, &live, notes)
    }

    /// An Apply's answer: the object as the server has it after the write.
    fn applied(
        &self,
        kind: &Kind,
        typ: &str,
        at: &str,
        live: &Json,
        notes: Vec<String>,
    ) -> std::result::Result<pb::ApplyResponse, Failed> {
        let s = |p: &str| get_path(live, p).and_then(Json::as_str).unwrap_or("");
        Ok(pb::ApplyResponse {
            remote: remote(kind, s("metadata.namespace"), s("metadata.name")),
            attrs: Some(wire::doc(&attrs(live))),
            computed: Some(wire::doc(
                &self
                    .computed(typ, live)
                    .map_err(|e| Failed::MaybeApplied(format!("{at}: {e:#}")))?,
            )),
            elapsed_ms: 0,
            notes,
        })
    }

    /// An update (or an adoption) in place: server-side apply of the
    /// document, each `keep` path (R-164) applied again as the object has
    /// it (a Secret's `stringData` key from its `data`), so the apply
    /// leaves it untouched.
    async fn update(
        &self,
        c: &Cluster,
        kind: &Kind,
        req: &pb::ApplyRequest,
        at: &str,
        config: &Json,
    ) -> std::result::Result<Json, Failed> {
        let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
        let kept;
        let config = match req.keep.is_empty() {
            true => config,
            false => {
                let live = c
                    .get(kind, ns, n)
                    .await
                    .map_err(|e| refused(at, e))?
                    .ok_or_else(|| Failed::Refused(format!("{at}: {} is not there", req.remote)))?;
                kept = keep(config, &attrs(&live), &req.keep)
                    .map_err(|e| Failed::Refused(format!("{at}: {e}")))?;
                &kept
            }
        };
        let obj = self
            .manifest(kind, config, ns, n)
            .map_err(|e| refused(at, e))?;
        self.write(c, kind, ns, n, &obj, at).await
    }

    /// A replacement: the old object deleted first (destroy-first, or
    /// when the new one takes its name) and waited for, then the new one
    /// created.
    async fn replace(
        &self,
        c: &Cluster,
        kind: &Kind,
        req: &pb::ApplyRequest,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
    ) -> std::result::Result<Json, Failed> {
        let (ns, n) = parse_remote(kind, &req.remote, &c.namespace);
        let new_ns = namespace(config, &c.namespace);
        let same = get_path(config, "metadata.name").and_then(Json::as_str) == Some(n)
            && (!kind.namespaced || new_ns == ns);
        if req.create_first && same {
            notes.push(format!(
                "{at}: the replacement has the old object's name, so the old one goes first"
            ));
        }
        if !req.create_first || same {
            c.delete(kind, ns, n).await.map_err(|e| maybe(at, e))?;
            if !c
                .gone(kind, ns, n, REPLACE_WAIT)
                .await
                .map_err(|e| maybe(at, e))?
            {
                return Err(Failed::MaybeApplied(format!(
                    "{at}: {} is still terminating after {}s; its replacement is not created",
                    req.remote,
                    REPLACE_WAIT.as_secs()
                )));
            }
        }
        self.create(c, kind, &req.r#type, at, config, &req.idempotency_key)
            .await
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
            let v = self.held_value(c, &h).await.map_err(refuse)?;
            set_path(&mut out, &path, v);
        }
        let mut annotations = match get_path(&out, "metadata.annotations") {
            Some(Json::Object(a)) => a.clone(),
            _ => serde_json::Map::new(),
        };
        annotations.insert(
            HELD_ANNOTATION.into(),
            Json::String(Json::Object(markers).to_string()),
        );
        set_path(&mut out, "metadata.annotations", Json::Object(annotations));
        Ok(out)
    }

    /// The value of the secret `h` holds: the object this provider manages
    /// read from the cluster (a Secret's `stringData` key from its `data`,
    /// decoded), else why not.
    async fn held_value(
        &self,
        c: &Cluster,
        h: &provider::Held,
    ) -> std::result::Result<Json, String> {
        if h.provider != openapi::PROVIDER {
            return Err(format!(
                "it is held by the provider {}, not this one",
                h.provider
            ));
        }
        let kind = self.derived.kind(&h.typ).map_err(|e| format!("{e:#}"))?;
        let (ns, n) = parse_remote(kind, &h.remote, &c.namespace);
        let live = c
            .get(kind, ns, n)
            .await
            .map_err(|e| format!("{e:#}"))?
            .ok_or_else(|| format!("{} {} is not in the cluster", h.typ, h.remote))?;
        let v = get_path(&attrs_raw(&live), &h.path).cloned().or_else(|| {
            let k = h.path.strip_prefix("stringData.")?;
            let text = base64(live.get("data")?.get(k)?.as_str()?)?;
            String::from_utf8(text).ok().map(Json::String)
        });
        v.filter(|v| marker(v).is_none())
            .ok_or_else(|| format!("{} {} does not set {}", h.typ, h.remote, h.path))
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

impl From<Failed> for CallError {
    fn from(f: Failed) -> CallError {
        match f {
            Failed::Refused(m) => CallError::Refused(m),
            Failed::MaybeApplied(m) => CallError::MaybeApplied(m),
        }
    }
}

/// A delete: background propagation, then the object waited for (bounded:
/// still terminating after [`DELETE_WAIT`] is a note, not a failure).
async fn delete(
    c: &Cluster,
    kind: &Kind,
    remote: &str,
    at: &str,
) -> std::result::Result<Vec<String>, Failed> {
    let (ns, n) = parse_remote(kind, remote, &c.namespace);
    c.delete(kind, ns, n).await.map_err(|e| maybe(at, e))?;
    let gone = c
        .gone(kind, ns, n, DELETE_WAIT)
        .await
        .map_err(|e| maybe(at, e))?;
    Ok(match gone {
        true => Vec::new(),
        false => vec![format!(
            "{at}: {remote} is still terminating after {}s",
            DELETE_WAIT.as_secs()
        )],
    })
}

/// Each secret another object holds in `v` (its marker, `provider::Held`)
/// under the path `path`: where it is, where it is held, its label.
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

/// A refusal saying why: the provider's error, its chain of causes.
fn refusal(e: anyhow::Error) -> CallError {
    CallError::Refused(format!("{e:#}"))
}

fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, CallError> {
    v.map(wire::from_doc).transpose().map_err(refusal)
}

/// The provider behind the protocol (the SDK's `Handler`): a [`K8s`] per
/// session, once its Configure has run, each call answered by its
/// session's.
#[derive(Default)]
pub struct Provider {
    k8s: Configured<K8s>,
}

impl Handler for Provider {
    async fn handle(
        &self,
        session: Session,
        call: Call,
        _: Progress<'_>,
    ) -> std::result::Result<Reply, CallError> {
        match call {
            Call::Handshake(req) => Ok(Reply::Handshake(handshake(&req)?)),
            Call::Configure(req) => Ok(Reply::Configure(self.configure(session, &req).await?)),
            call => {
                let k8s = self.k8s.get(session).ok_or_else(|| {
                    CallError::Refused(format!(
                        "the provider is not configured: no session {session}"
                    ))
                })?;
                k8s.answer(call).await
            }
        }
    }
}

/// The provider's name, protocol, capabilities and settings, to a host
/// that speaks its protocol version.
fn handshake(req: &pb::HandshakeRequest) -> std::result::Result<pb::HandshakeResponse, CallError> {
    let v = req.protocol_version;
    if v != VERSION {
        return Err(CallError::Refused(format!(
            "this provider speaks protocol version {VERSION}, not {v}"
        )));
    }
    Ok(pb::HandshakeResponse {
        protocol_version: VERSION,
        name: openapi::PROVIDER.into(),
        // `offline`: deferred, it plans against its OpenAPI snapshot
        // (R-188).
        capabilities: ["resource", "managed", "inventory", "keep", "offline"]
            .map(String::from)
            .to_vec(),
        version: BUILD.into(),
        settings: crate::cluster::SETTINGS
            .iter()
            .map(|s| pb::SettingDecl {
                name: s.name.into(),
                sensitive: s.sensitive,
                credential: s.credential.then(|| crate::cluster::CREDENTIAL.into()),
            })
            .collect(),
        health: crate::health::types(),
    })
}

impl Provider {
    /// Configure `session`: its [`K8s`], and the cluster it reaches as its
    /// account, so the host paces each cluster's requests in one window,
    /// whichever session sends them.
    async fn configure(
        &self,
        session: Session,
        req: &pb::ConfigureRequest,
    ) -> std::result::Result<pb::ConfigureResponse, CallError> {
        let config = doc_of(req.config.as_ref())?.unwrap_or(json!({}));
        let k8s = K8s::asked(&Asked::of(&config)).await.map_err(refusal)?;
        let account = k8s.cluster.as_ref().ok().map(|c| c.url.clone());
        self.k8s.set(session, k8s);
        Ok(pb::ConfigureResponse {
            session: session.0,
            account,
            ..Default::default()
        })
    }
}

impl K8s {
    /// The answer to `call`, a call of this session but its Handshake and
    /// Configure.
    async fn answer(&self, call: Call) -> std::result::Result<Reply, CallError> {
        Ok(match call {
            Call::Handshake(_) | Call::Configure(_) => {
                return Err(CallError::Refused(format!(
                    "a {} call is the provider's, not its session's",
                    call.method()
                )));
            }
            Call::Schema(r) => Reply::Schema(self.schema_reply(&r)?),
            Call::Query(q) => Reply::Query(self.query_reply(&q).await?),
            Call::Read(r) => Reply::Read(self.read_reply(&r).await?),
            Call::Plan(r) => Reply::Plan(self.plan_reply(&r).await?),
            // The result alone: a Kubernetes object is there once the API
            // server takes it (R-130's events are for a provider that
            // waits).
            Call::Apply(r) => {
                let config = doc_of(r.config.as_ref())?.unwrap_or(Json::Null);
                Reply::Apply(self.apply(&r, &config).await?)
            }
            Call::Import(r) => Reply::Import(self.import_reply(r).await?),
            // No kind of this provider is held for another to read: a
            // secret in an object is the program's, sent in its document.
            Call::Reveal(r) => {
                let h = r.held.unwrap_or_default();
                return Err(CallError::Refused(format!(
                    "reveal {} {}#{}: the k8s provider holds no secret",
                    h.r#type, h.remote, h.path
                )));
            }
            Call::Health(r) => Reply::Health(pb::HealthResponse {
                answers: self.health(&r.objects).await.map_err(refusal)?,
            }),
        })
    }

    fn schema_reply(
        &self,
        req: &pb::SchemaRequest,
    ) -> std::result::Result<pb::SchemaResponse, CallError> {
        let schema = match &req.types {
            Some(t) => self.derived.schema_of(t.names.iter().map(String::as_str)),
            None => self.derived.schema(),
        }
        .map_err(refusal)?;
        let mut facts = wire::schema_facts(&schema, req).map_err(refusal)?;
        // The settings that reach the cluster are its connection (R-193):
        // while they wait, dform plans against the snapshot, provisionally.
        // `namespace` says what an object is.
        for s in crate::cluster::SETTINGS.iter().filter(|s| s.credential) {
            let flags = match s.sensitive {
                true => vec!["sensitive", "connection"],
                false => vec!["connection"],
            };
            let flags = Value::List(flags.into_iter().map(|f| Value::Str(f.into())).collect());
            let args = [
                Value::Str(openapi::PROVIDER.into()),
                Value::Str(s.name.into()),
                flags,
            ];
            facts.push(pb::Fact {
                pred: dform_core::schema::PROVIDER_SETTING.into(),
                args: args.iter().map(pb::Value::from).collect(),
            });
        }
        Ok(pb::SchemaResponse {
            facts,
            externs: Vec::new(),
            checks_refinements: false,
            examples: self.examples(),
        })
    }

    /// The inventory's rows (offline, none: a plan reads no world), or
    /// `provider.created`'s.
    async fn query_reply(
        &self,
        q: &pb::QueryRequest,
    ) -> std::result::Result<Vec<pb::Row>, CallError> {
        let row = |values: Vec<Value>| pb::Row {
            values: values.iter().map(pb::Value::from).collect(),
        };
        if INVENTORY.iter().any(|(p, _)| *p == q.pred) {
            if self.cluster.is_err() {
                return Ok(Vec::new());
            }
            let typ = match (q.input.first(), q.inputs.first().map(Value::try_from)) {
                (Some(true), Some(Ok(Value::Str(t)))) => Some(t),
                _ => None,
            };
            let rows = self
                .inventory(&q.pred, typ.as_deref())
                .await
                .map_err(refusal)?;
            return Ok(rows.into_iter().map(row).collect());
        }
        if q.pred != CREATED {
            return Err(CallError::Refused(format!(
                "the Kubernetes provider answers no extern ({})",
                q.pred
            )));
        }
        let args = q
            .inputs
            .iter()
            .map(Value::try_from)
            .collect::<Result<Vec<_>>>()
            .map_err(refusal)?;
        let [Value::Str(typ), Value::Str(name), Value::Str(key)] = args.as_slice() else {
            return Err(CallError::Refused(format!(
                "{CREATED} is asked with Type, Name and Key bound"
            )));
        };
        let found = self.created(typ, name, key).await.map_err(refusal)?;
        let s = |x: &str| Value::Str(x.to_string());
        Ok(found
            .map(|remote| row(vec![s(typ), s(name), s(key), s(&remote)]))
            .into_iter()
            .collect())
    }

    async fn read_reply(
        &self,
        r: &pb::ReadRequest,
    ) -> std::result::Result<pb::ReadResponse, CallError> {
        let found = self
            .read(&r.r#type, &r.remote, &r.name)
            .await
            .map_err(refusal)?;
        Ok(match found {
            Some((attrs, computed)) => pb::ReadResponse {
                found: true,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ReadResponse::default(),
        })
    }

    async fn plan_reply(
        &self,
        r: &pb::PlanRequest,
    ) -> std::result::Result<pb::PlanResponse, CallError> {
        let prior = doc_of(r.prior.as_ref())?;
        let desired = doc_of(r.desired.as_ref())?;
        let (changes, requires_replace) = self
            .plan(
                &r.r#type,
                &r.name,
                &r.remote,
                prior.as_ref(),
                desired.as_ref(),
            )
            .await
            .map_err(refusal)?;
        Ok(pb::PlanResponse {
            changes: changes.iter().map(pb::Change::from).collect(),
            requires_replace,
            reason: None,
        })
    }

    async fn import_reply(
        &self,
        r: pb::ImportRequest,
    ) -> std::result::Result<pb::ImportResponse, CallError> {
        let found = self.import(&r.r#type, &r.remote).await.map_err(refusal)?;
        Ok(match found {
            Some((name, attrs, computed)) => pb::ImportResponse {
                found: true,
                r#type: r.r#type,
                name,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ImportResponse::default(),
        })
    }
}

/// Serve as a provider through the SDK (over gRPC natively), and exit when
/// stdin closes; `DFORM_LOG`'s lines on stderr. It uses the host's HTTP
/// and its credentials, nothing else.
#[cfg(not(target_family = "wasm"))]
pub fn serve() -> Result<()> {
    dform_core::log::install();
    dform_sdk::native::serve(
        Provider::default(),
        &["dform:host/http", "dform:host/secrets"],
    )
}

/// A resource's address as dform prints it, `T["N"]`.
fn address(typ: &str, name: &str) -> dform_core::address::Address {
    dform_core::address::Address {
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
