//! The Kubernetes provider's schema, derived from the API server's OpenAPI
//! v3 document (`/openapi/v3`: an index of group-versions, one document
//! each), in the shape `{"paths": {"apis/apps/v1": DOC, ...}}` the provider
//! caches it in and `crates/dform-k8s/openapi-snapshot.json` holds.
//!
//! A kind is a component schema with `x-kubernetes-group-version-kind` whose
//! object path (`.../{name}`) can be patched; the path gives its plural and
//! whether it lives in a namespace. It becomes the type
//! `k8s.<group>.<version>.<kind>` (the core group as `core`, the kind in
//! snake_case: a type name is a lowercase qualified name), and every
//! property path of its schema a `type_attr`, dotted through objects and
//! list elements:
//!
//! - `x-kubernetes-list-map-keys` are the list's `type_list_key`; a list
//!   whose `x-kubernetes-list-type` is `set` is a `set`;
//! - the leaves of `status` and `metadata.uid`, `resourceVersion`,
//!   `generation`, `creationTimestamp`, `deletionTimestamp`,
//!   `deletionGracePeriodSeconds` are `computed`, `uid` an identity (`id`);
//!   `managedFields` and `selfLink` are left out;
//! - `metadata.name` is `optional_computed` (with `id`): a program may set
//!   `metadata.generateName` instead and let Apply pick the name;
//!   `metadata.namespace` is `optional_computed` (the kubeconfig's
//!   namespace); both are `force_new`, since another name is another
//!   object;
//! - a property the server defaults (its schema has a `default`, or it is
//!   one of [`SERVER_DEFAULTED`]) and that is not an object is
//!   `optional_computed`: a program may set it, and a ref to it is a null
//!   the cluster's value resolves;
//! - a property in its object's `required` list is `required`: Plan refuses
//!   a document that sets the object but not the property;
//! - a Secret's `data` and `stringData` are `sensitive`;
//! - a property's `description` is its path's `type_doc`, the kind's its
//!   type's (path `""`).
//!
//! Every type gets `type_retry(T, 5)` and a `type_replace` order: a
//! Deployment, Service or ConfigMap `create_first`, a Namespace
//! `destroy_first`, the rest `either`. The short names of the mock
//! (`crates/dform-mock/schemas/k8s.df`'s `type_alias` facts, `k8s.deployment`) are
//! the same types under a second name.

use anyhow::{Context, Result, anyhow};
use dform_core::ast::{Atom, Term};
use dform_core::schema::Schema;
use dform_core::value::Value;
use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::path::Path;

/// The OpenAPI document of a recent Kubernetes release, trimmed to every
/// kind of its stable groups: the static schema, the provider's when no
/// cluster is reachable or before the program's is configured (R-110).
pub const SNAPSHOT: &str = include_str!("../openapi-snapshot.json");

/// The mock Kubernetes schema, for its `type_alias` facts.
const MOCK: &str = include_str!("../../dform-mock/schemas/k8s.df");

/// The provider's name: `type_provider` and state record it.
pub const PROVIDER: &str = dform_core::plugin::backend::KUBERNETES;

/// Read attempts before an object state maps is taken as gone.
pub const RETRY: i64 = 5;

/// Properties the API server defaults that the OpenAPI document does not
/// mark with a `default` (it marks only a few, list elements' mostly), by
/// path, whatever the kind: a list element's properties are dotted through
/// the list. A fallback beside the document's own defaults. Leaves only:
/// the value of an unset Optional+Computed path is the cluster's, so an
/// object would carry the server's whole object into the document
/// (`spec.strategy`'s `rollingUpdate` beside a program's `type: Recreate`,
/// which the server refuses). `metadata.uid` is `computed` already.
pub const SERVER_DEFAULTED: [&str; 44] = [
    // Service
    "spec.clusterIP",
    "spec.clusterIPs",
    "spec.type",
    "spec.sessionAffinity",
    "spec.ipFamilies",
    "spec.ipFamilyPolicy",
    "spec.internalTrafficPolicy",
    // Deployment, StatefulSet, DaemonSet, ReplicaSet
    "spec.progressDeadlineSeconds",
    "spec.revisionHistoryLimit",
    "spec.strategy.type",
    "spec.updateStrategy.type",
    "spec.podManagementPolicy",
    "spec.persistentVolumeClaimRetentionPolicy.whenDeleted",
    "spec.persistentVolumeClaimRetentionPolicy.whenScaled",
    // Job, CronJob
    "spec.backoffLimit",
    "spec.completions",
    "spec.parallelism",
    "spec.completionMode",
    "spec.podReplacementPolicy",
    "spec.suspend",
    "spec.concurrencyPolicy",
    "spec.successfulJobsHistoryLimit",
    "spec.failedJobsHistoryLimit",
    // A pod template
    "spec.template.spec.dnsPolicy",
    "spec.template.spec.restartPolicy",
    "spec.template.spec.schedulerName",
    "spec.template.spec.terminationGracePeriodSeconds",
    "spec.template.spec.containers.imagePullPolicy",
    "spec.template.spec.containers.terminationMessagePath",
    "spec.template.spec.containers.terminationMessagePolicy",
    // A CronJob's pod template
    "spec.jobTemplate.spec.template.spec.dnsPolicy",
    "spec.jobTemplate.spec.template.spec.restartPolicy",
    "spec.jobTemplate.spec.template.spec.schedulerName",
    "spec.jobTemplate.spec.template.spec.terminationGracePeriodSeconds",
    "spec.jobTemplate.spec.template.spec.containers.imagePullPolicy",
    "spec.jobTemplate.spec.template.spec.containers.terminationMessagePath",
    "spec.jobTemplate.spec.template.spec.containers.terminationMessagePolicy",
    // A Pod
    "spec.dnsPolicy",
    "spec.restartPolicy",
    "spec.schedulerName",
    "spec.terminationGracePeriodSeconds",
    "spec.containers.imagePullPolicy",
    "spec.containers.terminationMessagePath",
    "spec.containers.terminationMessagePolicy",
];

/// A kind the API serves: where its objects live and what they are called.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Kind {
    /// Empty for the core group.
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub namespaced: bool,
}

impl Kind {
    /// `apps/v1`, or `v1` for the core group.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    /// The URL path of the kind's objects in `ns` (ignored for a
    /// cluster-scoped kind).
    pub fn collection(&self, ns: &str) -> String {
        let base = self.base();
        if self.namespaced {
            format!("{base}/namespaces/{ns}/{}", self.plural)
        } else {
            format!("{base}/{}", self.plural)
        }
    }

    /// The URL path of the kind's objects in every namespace.
    pub fn collection_all(&self) -> String {
        format!("{}/{}", self.base(), self.plural)
    }

    fn base(&self) -> String {
        if self.group.is_empty() {
            format!("/api/{}", self.version)
        } else {
            format!("/apis/{}/{}", self.group, self.version)
        }
    }
}

/// `k8s.apps.v1.deployment`, `k8s.core.v1.config_map`.
pub fn type_name(group: &str, version: &str, kind: &str) -> String {
    let group = if group.is_empty() { "core" } else { group };
    format!(
        "k8s.{}.{}.{}",
        group.replace('-', "_"),
        version,
        snake(kind)
    )
}

/// `HorizontalPodAutoscaler` -> `horizontal_pod_autoscaler`,
/// `CSIDriver` -> `csi_driver`.
fn snake(kind: &str) -> String {
    let cs: Vec<char> = kind.chars().collect();
    let mut out = String::new();
    for (i, c) in cs.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = cs[i - 1];
            let next_lower = cs.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// The short names and the types they stand for: the mock's, and
/// `k8s.<kind>` for every other kind of the snapshot whose kind no other
/// group-version of it has (`k8s.storage_class` is
/// `k8s.storage.k8s.io.v1.storage_class`). A short name is for the stable
/// kinds the provider serves without a cluster; a cluster's own kinds (its
/// CRDs) keep their full names, so a CRD never takes or shadows one.
pub fn aliases() -> Result<Vec<(String, String)>> {
    let schema = Schema::parse(MOCK, "crates/dform-mock/schemas/k8s.df")?;
    let mut out: Vec<(String, String)> = schema
        .facts
        .iter()
        .filter(|f| f.pred == "type_alias")
        .filter_map(|f| match f.args.as_slice() {
            [Term::Val(Value::Str(a)), Term::Val(Value::Str(t))] => Some((a.clone(), t.clone())),
            _ => None,
        })
        .collect();
    let doc: Json = serde_json::from_str(SNAPSHOT).context("parse the OpenAPI snapshot")?;
    let mut by_kind: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in doc
        .get("paths")
        .and_then(Json::as_object)
        .into_iter()
        .flat_map(|m| m.values())
    {
        for item in d
            .get("paths")
            .and_then(Json::as_object)
            .into_iter()
            .flat_map(|m| m.values())
        {
            let Some(gvk) = item.pointer("/patch/x-kubernetes-group-version-kind") else {
                continue;
            };
            let s = |k: &str| gvk.get(k).and_then(Json::as_str).unwrap_or("");
            let typ = type_name(s("group"), s("version"), s("kind"));
            let typs = by_kind.entry(snake(s("kind"))).or_default();
            if !typs.contains(&typ) {
                typs.push(typ);
            }
        }
    }
    let taken: std::collections::BTreeSet<String> = out
        .iter()
        .flat_map(|(a, t)| [a.clone(), t.clone()])
        .collect();
    for (kind, typs) in by_kind {
        let alias = format!("k8s.{kind}");
        if let [typ] = typs.as_slice()
            && !taken.contains(typ)
            && !taken.contains(&alias)
        {
            out.push((alias, typ.clone()));
        }
    }
    Ok(out)
}

/// The derived schema: the kinds by type name (aliases included), and the
/// schema facts.
#[derive(Debug, Clone)]
pub struct Derived {
    pub kinds: BTreeMap<String, Kind>,
    pub schema: Schema,
}

impl Derived {
    pub fn kind(&self, typ: &str) -> Result<&Kind> {
        self.kinds
            .get(typ)
            .ok_or_else(|| anyhow!("{typ} is not a kind this cluster serves"))
    }
}

/// Derive the schema from a document `{"paths": {GV: DOC}}`, and serve the
/// `aliases` whose type it has under their short name too.
pub fn derive(doc: &Json, aliases: &[(String, String)]) -> Result<Derived> {
    let mut kinds = BTreeMap::new();
    let mut facts = Vec::new();
    let empty = Map::new();
    let gvs = doc
        .get("paths")
        .and_then(Json::as_object)
        .ok_or_else(|| anyhow!("an OpenAPI index has `paths`"))?;
    for d in gvs.values() {
        let schemas = d
            .pointer("/components/schemas")
            .and_then(Json::as_object)
            .unwrap_or(&empty);
        let paths = d.get("paths").and_then(Json::as_object).unwrap_or(&empty);
        for (path, item) in paths {
            let Some(prefix) = path.strip_suffix("/{name}") else {
                continue;
            };
            let Some(gvk) = item.pointer("/patch/x-kubernetes-group-version-kind") else {
                continue;
            };
            let s = |k: &str| gvk.get(k).and_then(Json::as_str).unwrap_or("").to_string();
            let kind = Kind {
                group: s("group"),
                version: s("version"),
                kind: s("kind"),
                plural: prefix.rsplit('/').next().unwrap_or("").to_string(),
                namespaced: prefix.contains("/namespaces/{namespace}/"),
            };
            let Some(root) = schemas.values().find(|v| {
                v.get("x-kubernetes-group-version-kind")
                    .and_then(Json::as_array)
                    .is_some_and(|gs| gs.iter().any(|g| g == gvk))
            }) else {
                continue;
            };
            let typ = type_name(&kind.group, &kind.version, &kind.kind);
            let mut w = Walk {
                schemas,
                kind: &kind,
                attrs: Vec::new(),
                keys: Vec::new(),
                docs: Vec::new(),
            };
            if let Some(d) = root.get("description").and_then(Json::as_str) {
                w.docs.push((String::new(), d.to_string()));
            }
            w.object(root, "", Ctx::default(), &mut Vec::new());
            facts.extend(type_facts(&typ, &kind, &w));
            kinds.insert(typ, kind);
        }
    }
    let mut alias_facts = Vec::new();
    for (alias, target) in aliases {
        let Some(kind) = kinds.get(target).cloned() else {
            continue;
        };
        alias_facts.push(atom("type_alias", vec![sym(alias), sym(target)]));
        for f in facts
            .iter()
            .filter(|f: &&Atom| f.args.first() == Some(&sym(target)))
        {
            let mut f = f.clone();
            f.args[0] = sym(alias);
            alias_facts.push(f);
        }
        kinds.insert(alias.clone(), kind);
    }
    facts.extend(alias_facts);
    let schema = Schema::from_facts(&facts).context("the schema derived from OpenAPI")?;
    Ok(Derived { kinds, schema })
}

/// The derived schema of the checked-in snapshot.
pub fn snapshot() -> Result<Derived> {
    let doc: Json = serde_json::from_str(SNAPSHOT).context("parse the OpenAPI snapshot")?;
    derive(&doc, &aliases()?)
}

/// The derived schema of the checked-in snapshot, from the cache in `dir`
/// when it holds the snapshot's ([`cached`]).
pub fn snapshot_cached(dir: Option<&Path>) -> Result<Derived> {
    cached(
        &dform_core::zset::file::fnv64(SNAPSHOT.as_bytes()),
        dir,
        snapshot,
    )
}

/// This module's source: a cached derivation is of this code.
const SOURCE: &str = include_str!("openapi.rs");

/// Where the derived schema is cached, beside the OpenAPI document's cache.
pub const DERIVED_CACHE: &str = "k8s-schema.json";

/// The schema derived from the OpenAPI document whose hash is `doc_hash`:
/// read from `DIR/k8s-schema.json` when that is keyed by the document's
/// hash (and this derivation's source and aliases), else `derive`d and
/// written there. Deriving is most of what the provider does at Configure;
/// the cache skips parsing the document too.
pub fn cached(
    doc_hash: &str,
    dir: Option<&Path>,
    derive: impl FnOnce() -> Result<Derived>,
) -> Result<Derived> {
    use dform_core::zset::file::fnv64;
    let key = fnv64(
        format!(
            "{doc_hash} {} {} {}",
            fnv64(SOURCE.as_bytes()),
            fnv64(MOCK.as_bytes()),
            fnv64(SNAPSHOT.as_bytes())
        )
        .as_bytes(),
    );
    let path = dir.map(|d| d.join(DERIVED_CACHE));
    if let Some(p) = &path
        && let Ok(text) = std::fs::read_to_string(p)
        && let Some(d) = decode(&text, &key)
    {
        return Ok(d);
    }
    let d = derive()?;
    if let Some(p) = &path
        && let Some(text) = encode(&d, &key)
    {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(p, text).with_context(|| format!("write {}", p.display()))?;
    }
    Ok(d)
}

/// The version of the derivation: a cache written by another one is
/// derived again.
fn derivation() -> String {
    use dform_core::zset::file::fnv64;
    fnv64(
        format!(
            "{} {} {}",
            fnv64(SOURCE.as_bytes()),
            fnv64(MOCK.as_bytes()),
            fnv64(SNAPSHOT.as_bytes())
        )
        .as_bytes(),
    )
}

/// Where a deployment's extension of the static schema is cached (R-110):
/// `DIR/schema/<deployment>/k8s.json`, the deployment as its objects'
/// label has it.
pub fn extension_path(dir: &Path, stack: &str) -> std::path::PathBuf {
    dir.join("schema").join(stack).join("k8s.json")
}

/// The kinds `full` (a cluster's derivation) has and `base` (the static
/// schema) has not, its CRDs and aggregated APIs, with their schema rows.
pub fn extension(full: &Derived, base: &Derived) -> Result<Derived> {
    let kinds: BTreeMap<String, Kind> = full
        .kinds
        .iter()
        .filter(|(t, _)| !base.kinds.contains_key(*t))
        .map(|(t, k)| (t.clone(), k.clone()))
        .collect();
    let facts: Vec<Atom> = full
        .schema
        .facts
        .iter()
        .filter(|f| match f.args.first() {
            Some(Term::Val(Value::Str(t))) => kinds.contains_key(t),
            _ => false,
        })
        .cloned()
        .collect();
    let schema = Schema::from_facts(&facts).context("a cluster's extension of the schema")?;
    Ok(Derived { kinds, schema })
}

/// `base` with the kinds of `ext`.
pub fn extended(base: &Derived, ext: &Derived) -> Result<Derived> {
    let mut kinds = base.kinds.clone();
    kinds.extend(ext.kinds.iter().map(|(t, k)| (t.clone(), k.clone())));
    let facts: Vec<Atom> = base
        .schema
        .facts
        .iter()
        .chain(&ext.schema.facts)
        .cloned()
        .collect();
    let schema = Schema::from_facts(&facts).context("the schema and a cluster's extension")?;
    Ok(Derived { kinds, schema })
}

/// The cached extension at `path` ([`extension_path`]) of this derivation:
/// of the cluster document `doc_hash`, or of any when `None` (the
/// deployment's cluster is not known yet: its last one's).
pub fn read_extension(path: &Path, doc_hash: Option<&str>) -> Option<Derived> {
    let text = std::fs::read_to_string(path).ok()?;
    let key = serde_json::from_str::<Json>(&text)
        .ok()?
        .get("key")?
        .as_str()?
        .to_string();
    let (version, hash) = key.split_once(' ')?;
    if version != derivation() || doc_hash.is_some_and(|h| h != hash) {
        return None;
    }
    decode(&text, &key)
}

/// Cache `ext`, the extension of the cluster document `doc_hash`, at
/// `path`.
pub fn write_extension(path: &Path, doc_hash: &str, ext: &Derived) -> Result<()> {
    let key = format!("{} {doc_hash}", derivation());
    let text = encode(ext, &key).ok_or_else(|| anyhow!("the extension does not encode"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// A cached derivation: `{"key", "kinds", "facts": [[PRED, ARG...]]}`,
/// each argument a string, an integer or a list of them. `None` for a
/// schema with anything else.
fn encode(d: &Derived, key: &str) -> Option<String> {
    fn value(v: &Value) -> Option<Json> {
        Some(match v {
            Value::Str(s) => Json::String(s.clone()),
            Value::Int(n) => Json::from(*n),
            Value::List(xs) => Json::Array(xs.iter().map(value).collect::<Option<_>>()?),
            _ => return None,
        })
    }
    fn term(t: &Term) -> Option<Json> {
        match t {
            Term::Val(v) => value(v),
            _ => None,
        }
    }
    let facts = d
        .schema
        .facts
        .iter()
        .map(|f| {
            let mut row = vec![Json::String(f.pred.clone())];
            for a in &f.args {
                row.push(term(a)?);
            }
            Some(Json::Array(row))
        })
        .collect::<Option<Vec<_>>>()?;
    serde_json::to_string(&serde_json::json!({"key": key, "kinds": d.kinds, "facts": facts})).ok()
}

/// The cached derivation in `text`, if it is keyed `key` and whole.
fn decode(text: &str, key: &str) -> Option<Derived> {
    fn value(v: &Json) -> Option<Value> {
        Some(match v {
            Json::String(s) => Value::Str(s.clone()),
            Json::Number(n) => Value::Int(n.as_i64()?),
            Json::Array(xs) => Value::List(xs.iter().map(value).collect::<Option<_>>()?),
            _ => return None,
        })
    }
    let term = |v: &Json| value(v).map(Term::Val);
    #[derive(serde::Deserialize)]
    struct Cached {
        key: String,
        kinds: BTreeMap<String, Kind>,
        facts: Vec<Vec<Json>>,
    }
    let c: Cached = serde_json::from_str(text).ok()?;
    if c.key != key {
        return None;
    }
    let facts = c
        .facts
        .iter()
        .map(|row| {
            let (pred, args) = row.split_first()?;
            Some(atom(
                pred.as_str()?,
                args.iter().map(term).collect::<Option<_>>()?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let schema = Schema::from_facts(&facts).ok()?;
    Some(Derived {
        kinds: c.kinds,
        schema,
    })
}

/// Whether a property's `default` is one the server applies: not the zero
/// value (`""`, `0`, `false`, `{}`, `[]`), which the generator writes for
/// every field Go serializes even when empty (a container's `name`).
fn server_default(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64() != Some(0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(xs) => !xs.is_empty(),
        Json::Object(m) => !m.is_empty(),
    }
}

fn sym(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

fn atom(pred: &str, args: Vec<Term>) -> Atom {
    Atom {
        pred: pred.to_string(),
        args,
        record: None,
        span: Default::default(),
    }
}

fn type_facts(typ: &str, kind: &Kind, w: &Walk) -> Vec<Atom> {
    let order = match (kind.group.as_str(), kind.kind.as_str()) {
        ("apps", "Deployment") | ("", "Service") | ("", "ConfigMap") => "create_first",
        ("", "Namespace") => "destroy_first",
        _ => "either",
    };
    let mut out = vec![
        atom("type_provider", vec![sym(typ), sym(PROVIDER)]),
        atom("type_retry", vec![sym(typ), Term::Val(Value::Int(RETRY))]),
        atom("type_replace", vec![sym(typ), sym(order)]),
    ];
    for (path, ty, flags) in &w.attrs {
        let flags = flags.iter().map(|f| Term::Val(Value::Str(f.to_string())));
        out.push(atom(
            "type_attr",
            vec![sym(typ), sym(path), sym(ty), Term::List(flags.collect())],
        ));
    }
    for (path, text) in &w.docs {
        out.push(atom("type_doc", vec![sym(typ), sym(path), sym(text)]));
    }
    for (path, keys) in &w.keys {
        let keys = keys.iter().map(|k| sym(k)).collect();
        out.push(atom(
            "type_list_key",
            vec![sym(typ), sym(path), Term::List(keys)],
        ));
    }
    out
}

/// What a path inherits from its ancestors.
#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    /// Under `status`: the server writes it.
    computed: bool,
}

/// One kind's schema, walked to `type_attr` rows.
struct Walk<'a> {
    schemas: &'a Map<String, Json>,
    kind: &'a Kind,
    attrs: Vec<(String, &'static str, Vec<&'static str>)>,
    keys: Vec<(String, Vec<String>)>,
    /// Each path's description.
    docs: Vec<(String, String)>,
}

impl<'a> Walk<'a> {
    /// The component a node refers to (`$ref`, or `allOf: [{$ref}]`).
    fn target(&self, node: &'a Json) -> Option<(&'a str, &'a Json)> {
        let r = node
            .get("$ref")
            .or_else(|| node.pointer("/allOf/0/$ref"))
            .and_then(Json::as_str)?;
        let name = r.rsplit('/').next()?;
        self.schemas.get(name).map(|s| (name, s))
    }

    /// A key of the node, else of the component it refers to.
    fn get(&self, node: &'a Json, key: &str) -> Option<&'a Json> {
        node.get(key)
            .or_else(|| self.target(node).and_then(|(_, t)| t.get(key)))
    }

    fn ty(&self, node: &'a Json) -> &'static str {
        let format = self.get(node, "format").and_then(Json::as_str);
        if self.get(node, "x-kubernetes-int-or-string") == Some(&Json::Bool(true))
            || format == Some("int-or-string")
            || self.get(node, "oneOf").is_some()
        {
            return "int_or_string";
        }
        match self.get(node, "type").and_then(Json::as_str) {
            Some("object") if self.get(node, "properties").is_some() => "object",
            Some("object") if self.get(node, "additionalProperties").is_some() => "map",
            Some("array") => {
                match self
                    .get(node, "x-kubernetes-list-type")
                    .and_then(Json::as_str)
                {
                    Some("set") => "set",
                    _ => "list",
                }
            }
            Some("string") => "string",
            Some("integer") => "int",
            Some("boolean") => "bool",
            Some("number") => "number",
            _ => "any",
        }
    }

    /// The properties of an object node, under `path` (empty at the top).
    fn object(&mut self, node: &'a Json, path: &str, ctx: Ctx, stack: &mut Vec<&'a str>) {
        let Some(props) = self.get(node, "properties").and_then(Json::as_object) else {
            return;
        };
        let required: Vec<&str> = self
            .get(node, "required")
            .and_then(Json::as_array)
            .map(|r| r.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default();
        for (k, child) in props {
            let p = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            if self.skip(&p) {
                continue;
            }
            let ctx = Ctx {
                computed: ctx.computed || p == "status",
            };
            self.attr(child, &p, ctx, required.contains(&k.as_str()), stack);
        }
    }

    /// Paths no document carries: the type says them, or only the server
    /// writes them and dform never reads them.
    fn skip(&self, p: &str) -> bool {
        matches!(
            p,
            "apiVersion" | "kind" | "metadata.managedFields" | "metadata.selfLink"
        ) || (p == "metadata.namespace" && !self.kind.namespaced)
    }

    fn flags(&self, p: &str, ctx: Ctx, required: bool, defaulted: bool) -> Vec<&'static str> {
        match p {
            "metadata.name" => return vec!["optional_computed", "id", "force_new"],
            "metadata.namespace" => return vec!["optional_computed", "force_new"],
            "metadata.uid" => return vec!["computed", "id"],
            "metadata.resourceVersion"
            | "metadata.generation"
            | "metadata.creationTimestamp"
            | "metadata.deletionTimestamp"
            | "metadata.deletionGracePeriodSeconds" => return vec!["computed"],
            _ => {}
        }
        let mut out = Vec::new();
        if ctx.computed {
            out.push("computed");
        } else if defaulted || SERVER_DEFAULTED.contains(&p) {
            out.push("optional_computed");
        } else if required {
            out.push("required");
        }
        if self.kind.group.is_empty()
            && self.kind.kind == "Secret"
            && (p == "data" || p == "stringData")
        {
            out.push("sensitive");
        }
        out
    }

    fn attr(
        &mut self,
        node: &'a Json,
        p: &str,
        ctx: Ctx,
        required: bool,
        stack: &mut Vec<&'a str>,
    ) {
        let target = self.target(node).map(|(n, _)| n);
        if let Some(n) = target {
            if stack.contains(&n) {
                // A recursive schema (a CRD's JSONSchemaProps): the rest is
                // any value.
                self.attrs.push((
                    p.to_string(),
                    "any",
                    self.flags(
                        p,
                        ctx,
                        required,
                        self.get(node, "default").is_some_and(server_default),
                    ),
                ));
                return;
            }
            stack.push(n);
        }
        let ty = self.ty(node);
        let defaulted = self.get(node, "default").is_some_and(server_default) && ty != "object";
        // A computed value is minted per path, so computed paths do not
        // nest: an object the server writes is its leaves.
        if !(ctx.computed && ty == "object") {
            self.attrs
                .push((p.to_string(), ty, self.flags(p, ctx, required, defaulted)));
            if let Some(d) = self.get(node, "description").and_then(Json::as_str) {
                self.docs.push((p.to_string(), d.to_string()));
            }
        }
        // A map of quantities (`resources.limits`, a claim's
        // `resources.requests`): its usual keys are typed (R-66), so a
        // policy compares `limits.memory > 2Gi` in bytes; each is sent as
        // the quantity string the API takes.
        if ty == "map"
            && self
                .get(node, "additionalProperties")
                .and_then(|a| a.get("$ref"))
                .and_then(Json::as_str)
                .is_some_and(|r| r.ends_with("api.resource.Quantity"))
        {
            for (k, t) in [
                ("cpu", "cpu(quantity)"),
                ("memory", "bytes(quantity)"),
                ("storage", "bytes(quantity)"),
                ("ephemeral-storage", "bytes(quantity)"),
            ] {
                let q = format!("{p}.{k}");
                let flags = self.flags(&q, ctx, false, false);
                self.attrs.push((q, t, flags));
            }
        }
        match ty {
            "object" => self.object(node, p, ctx, stack),
            "list" | "set" => {
                if let Some(keys) = self
                    .get(node, "x-kubernetes-list-map-keys")
                    .and_then(Json::as_array)
                    && ty == "list"
                {
                    let keys = keys.iter().filter_map(Json::as_str).map(String::from);
                    self.keys.push((p.to_string(), keys.collect()));
                }
                if let Some(items) = self.get(node, "items") {
                    let inner = self.target(items).map(|(n, _)| n);
                    if let Some(n) = inner.filter(|n| !stack.contains(n)) {
                        stack.push(n);
                        self.object(items, p, ctx, stack);
                        stack.pop();
                    } else if inner.is_none() {
                        self.object(items, p, ctx, stack);
                    }
                }
            }
            _ => {}
        }
        if target.is_some() {
            stack.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kind_names_are_snake_case() {
        assert_eq!(
            type_name("apps", "v1", "Deployment"),
            "k8s.apps.v1.deployment"
        );
        assert_eq!(type_name("", "v1", "ConfigMap"), "k8s.core.v1.config_map");
        assert_eq!(
            type_name("autoscaling", "v2", "HorizontalPodAutoscaler"),
            "k8s.autoscaling.v2.horizontal_pod_autoscaler"
        );
        assert_eq!(
            type_name("storage.k8s.io", "v1", "CSIDriver"),
            "k8s.storage.k8s.io.v1.csi_driver"
        );
        assert_eq!(
            type_name("cert-manager.io", "v1", "ClusterIssuer"),
            "k8s.cert_manager.io.v1.cluster_issuer"
        );
    }

    #[test]
    fn the_snapshot_derives_classes_keys_and_aliases() {
        use dform_core::value::NullClass;
        let d = snapshot().unwrap();
        let s = &d.schema;
        let dep = "k8s.apps.v1.deployment";
        assert_eq!(s.class_of(dep, "metadata.uid"), Some(NullClass::Fresh));
        assert_eq!(
            s.class_of(dep, "status.readyReplicas"),
            Some(NullClass::Open)
        );
        assert_eq!(
            s.class_of(dep, "status"),
            None,
            "computed paths do not nest"
        );
        assert_eq!(
            s.optional_computed_class(dep, "metadata.name"),
            Some(NullClass::Fresh)
        );
        assert_eq!(
            s.list_key(dep, "spec.template.spec.containers.ports"),
            Some(&["containerPort".to_string(), "protocol".to_string()][..])
        );
        assert!(s.attr(dep, "spec.selector").unwrap().has("required"));
        assert_eq!(s.attr(dep, "metadata.finalizers").unwrap().ty, "set");
        assert!(s.is_sensitive("k8s.core.v1.secret", "data.token"));
        assert!(!s.is_sensitive("k8s.core.v1.config_map", "data.token"));
        assert!(
            s.attr("k8s.core.v1.namespace", "metadata.namespace")
                .is_none()
        );
        assert_eq!(
            d.kind("k8s.networking.k8s.io.v1.ingress")
                .unwrap()
                .collection("shop"),
            "/apis/networking.k8s.io/v1/namespaces/shop/ingresses"
        );
        // The mock's short names are the same types.
        assert_eq!(d.kind("k8s.deployment").unwrap(), d.kind(dep).unwrap());
        assert_eq!(
            s.attr("k8s.deployment", "spec.selector"),
            s.attr(dep, "spec.selector")
        );
        assert_eq!(s.read_attempts("k8s.deployment"), 5);
        assert_eq!(
            s.replace_order("k8s.namespace"),
            dform_core::schema::ReplaceOrder::DestroyFirst
        );
        assert_eq!(
            s.replace_order(dep),
            dform_core::schema::ReplaceOrder::CreateFirst
        );
    }

    /// A cluster's kinds beyond the static schema (a CRD) are cached per
    /// deployment, and a deferred run serves them from there (R-110).
    #[test]
    fn a_clusters_crds_extend_the_static_schema() {
        let base = snapshot().unwrap();
        let mut doc: Json = serde_json::from_str(SNAPSHOT).unwrap();
        // A CRD: the Lease's shape, as traefik.io/v1alpha1 Middleware.
        let mut crd = doc["paths"]["apis/coordination.k8s.io/v1"].clone();
        let gvk = json!({"group": "traefik.io", "version": "v1alpha1", "kind": "Middleware"});
        let item = json!({"patch": {"x-kubernetes-action": "patch",
            "x-kubernetes-group-version-kind": gvk}});
        crd["paths"] = json!({
            "/apis/traefik.io/v1alpha1/namespaces/{namespace}/middlewares/{name}": item
        });
        crd["components"]["schemas"]["io.k8s.api.coordination.v1.Lease"]["x-kubernetes-group-version-kind"] =
            json!([gvk]);
        doc["paths"]["apis/traefik.io/v1alpha1"] = crd;
        let full = derive(&doc, &aliases().unwrap()).unwrap();
        let ext = extension(&full, &base).unwrap();
        let mw = "k8s.traefik.io.v1alpha1.middleware";
        assert_eq!(ext.kinds.keys().collect::<Vec<_>>(), vec![mw]);
        assert!(ext.schema.attr(mw, "spec.holderIdentity").is_some());
        assert!(base.kind(mw).is_err());

        let dir = std::env::temp_dir().join(format!("dform-k8s-ext-{}", std::process::id()));
        let path = extension_path(&dir, "platform");
        write_extension(&path, "h1", &ext).unwrap();
        assert!(
            read_extension(&path, Some("h2")).is_none(),
            "another document"
        );
        let cached = read_extension(&path, None).unwrap();
        let both = extended(&base, &cached).unwrap();
        assert!(both.kind(mw).is_ok() && both.kind("k8s.storage_class").is_ok());
        assert!(both.schema.attr(mw, "spec.holderIdentity").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The snapshot is the static schema (R-110): every kind of the stable
    /// groups, each under a short name when its kind is unambiguous.
    #[test]
    fn the_snapshot_serves_the_stable_groups_by_short_name() {
        let d = snapshot().unwrap();
        let sc = "k8s.storage.k8s.io.v1.storage_class";
        assert_eq!(d.kind("k8s.storage_class").unwrap(), d.kind(sc).unwrap());
        assert!(d.schema.attr("k8s.storage_class", "provisioner").is_some());
        assert!(!d.kind(sc).unwrap().namespaced);
        for short in [
            "k8s.ingress_class",
            "k8s.priority_class",
            "k8s.persistent_volume",
            "k8s.custom_resource_definition",
            "k8s.validating_webhook_configuration",
            "k8s.lease",
        ] {
            assert!(d.kind(short).is_ok(), "{short}");
        }
    }

    /// OpenAPI descriptions are `type_doc` facts: the kind's and each
    /// property's, the short names' too.
    #[test]
    fn descriptions_are_type_docs() {
        let s = snapshot().unwrap().schema;
        let docs = s.docs();
        let dep = "k8s.apps.v1.deployment";
        assert!(docs[&(dep, "")].starts_with("Deployment enables declarative updates"));
        assert!(docs[&(dep, "spec.replicas")].contains("Number of desired pods"));
        assert_eq!(
            docs[&("k8s.deployment", "spec.replicas")],
            docs[&(dep, "spec.replicas")]
        );
    }

    /// What the server defaults is Optional+Computed: a `default` in the
    /// document, else the known paths; an object never is.
    #[test]
    fn server_defaulted_fields_are_optional_computed() {
        let s = snapshot().unwrap().schema;
        let (svc, dep) = ("k8s.core.v1.service", "k8s.apps.v1.deployment");
        for (t, p) in [
            (svc, "spec.clusterIP"),
            (svc, "spec.type"),
            (dep, "spec.strategy.type"),
            (dep, "spec.template.spec.containers.imagePullPolicy"),
            // The snapshot's own `default`s.
            (svc, "spec.ports.protocol"),
            (dep, "spec.template.spec.containers.ports.protocol"),
            ("k8s.batch.v1.cron_job", "spec.concurrencyPolicy"),
            (
                "k8s.batch.v1.cron_job",
                "spec.jobTemplate.spec.template.spec.restartPolicy",
            ),
            (
                "k8s.batch.v1.cron_job",
                "spec.jobTemplate.spec.template.spec.containers.imagePullPolicy",
            ),
            ("k8s.apps.v1.stateful_set", "spec.updateStrategy.type"),
            ("k8s.apps.v1.stateful_set", "spec.podManagementPolicy"),
            ("k8s.apps.v1.daemon_set", "spec.updateStrategy.type"),
            ("k8s.batch.v1.job", "spec.backoffLimit"),
        ] {
            assert!(s.attr(t, p).unwrap().has("optional_computed"), "{t} {p}");
        }
        // A zero-value default is the generator's, not the server's: a
        // container's name stays required.
        let name = s.attr(dep, "spec.template.spec.containers.name").unwrap();
        assert!(name.has("required") && !name.has("optional_computed"));
        assert!(
            !s.attr(dep, "spec.strategy")
                .unwrap()
                .has("optional_computed")
        );
        assert!(s.attr(dep, "metadata.uid").unwrap().has("computed"));

        let doc = serde_json::json!({"paths": {"apis/x.io/v1": {
            "paths": {"/apis/x.io/v1/namespaces/{namespace}/things/{name}": {"patch": {
                "x-kubernetes-group-version-kind":
                    {"group": "x.io", "version": "v1", "kind": "Thing"}}}},
            "components": {"schemas": {"Thing": {
                "x-kubernetes-group-version-kind":
                    [{"group": "x.io", "version": "v1", "kind": "Thing"}],
                "properties": {"spec": {"type": "object", "properties": {
                    "mode": {"type": "string", "default": "fast"},
                    "size": {"type": "integer"},
                    "count": {"type": "integer", "default": 0},
                    "opts": {"type": "object", "default": {},
                             "properties": {"a": {"type": "string"}}}}}}}}}}}});
        let s = derive(&doc, &[]).unwrap().schema;
        let t = "k8s.x.io.v1.thing";
        assert!(s.attr(t, "spec.mode").unwrap().has("optional_computed"));
        assert!(!s.attr(t, "spec.size").unwrap().has("optional_computed"));
        assert!(!s.attr(t, "spec.count").unwrap().has("optional_computed"));
        assert!(!s.attr(t, "spec.opts").unwrap().has("optional_computed"));
    }
}
