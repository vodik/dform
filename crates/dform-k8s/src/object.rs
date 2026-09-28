//! Kubernetes objects as the protocol's documents, both ways.
//!
//! Out: a desired document (`metadata`, `spec`, ... as the program sets
//! them) becomes the object dform applies, with `apiVersion` and `kind` from
//! its type and its namespace and name filled in (`manifest`).
//!
//! Back: a live object splits into the configured attributes and the
//! computed values (`attrs`, `computed`). The configured attributes are the
//! fields dform owns: the entry of `metadata.managedFields` for the field
//! manager `dform` says which (server-side apply records each manager's
//! fields as a `fieldsV1` set), so a default the server filled in or a field
//! another manager set is not configuration and never a diff. The computed
//! values are `status`, the server-written metadata, the name and
//! namespace, and every other field the server defaults (Optional+Computed:
//! the engine compares them only where the program sets them, and a ref to
//! one resolves to the cluster's value). Neither side carries JSON nulls: Kubernetes reads a
//! null as absent. A Secret's write-only `stringData` reads back from the
//! `data` the server folded it into.

use crate::openapi::Kind;
use anyhow::{Result, bail};
use dform_core::provider::{get_path, set_path};
use serde_json::{Map, Value as Json, json};

/// The field manager dform applies as.
pub const MANAGER: &str = "dform";

/// The annotation a Create's idempotency key rides on: a Create again with
/// the same key finds the object it made by name and answers with it. Not
/// configuration: `attrs` leaves it out, and the next update drops it.
pub const KEY_ANNOTATION: &str = "dform.io/idempotency-key";

/// The label every object dform applies carries: the deployment it is
/// of (`stack_label`). A Create whose answer was lost is found by it and
/// its `KEY_ANNOTATION` (`provider.created`). Not configuration: `attrs`
/// leaves it out.
pub const STACK_LABEL: &str = "dform.io/stack";

/// A deployment's name as a label value: at most 63 of `[A-Za-z0-9-_.]`,
/// alphanumeric at both ends (`app[env=prod]` is `app_env_prod`). Two
/// deployments may share one; the idempotency key tells their objects
/// apart.
pub fn stack_label(stack: &str) -> String {
    let v: String = stack
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() || "-_.".contains(c) {
            true => c,
            false => '_',
        })
        .take(63)
        .collect();
    v.trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// Mark `obj` (a manifest) with `STACK_LABEL` = `label`.
pub fn stamp(obj: &mut Json, label: &str) {
    if let Some(meta) = obj.get_mut("metadata").and_then(Json::as_object_mut)
        && let Json::Object(labels) = meta.entry("labels").or_insert_with(|| json!({}))
    {
        labels.insert(STACK_LABEL.into(), json!(label));
    }
}

/// The idempotency key the Create that made `live` carried, if any.
pub fn idempotency_key(live: &Json) -> Option<&str> {
    live.pointer("/metadata/annotations")?
        .get(KEY_ANNOTATION)?
        .as_str()
}

/// The server-written metadata dform returns as computed values.
const COMPUTED_META: [&str; 6] = [
    "uid",
    "resourceVersion",
    "generation",
    "creationTimestamp",
    "deletionTimestamp",
    "deletionGracePeriodSeconds",
];

/// The object `doc` describes: `apiVersion` and `kind` from its kind,
/// `metadata.name` and `metadata.namespace` set to `name` and `ns`. Never a
/// `status` or `metadata.managedFields`: server-side apply would make dform
/// their manager.
pub fn manifest(kind: &Kind, doc: &Json, ns: &str, name: &str) -> Result<Json> {
    let Json::Object(m) = doc else {
        bail!("a {} document is an object, not {doc}", kind.kind);
    };
    let mut out = m.clone();
    out.remove("status");
    if let Some(Json::Object(meta)) = out.get_mut("metadata") {
        meta.remove("managedFields");
    }
    for (k, want) in [
        ("apiVersion", kind.api_version()),
        ("kind", kind.kind.clone()),
    ] {
        match out.get(k).and_then(Json::as_str) {
            Some(v) if v != want => bail!("{k} is {v:?}; the type says {want:?}"),
            _ => {
                out.insert(k.into(), Json::String(want));
            }
        }
    }
    let meta = out
        .entry("metadata")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("metadata is an object"))?;
    meta.insert("name".into(), Json::String(name.into()));
    if kind.namespaced {
        meta.insert("namespace".into(), Json::String(ns.into()));
    }
    Ok(Json::Object(out))
}

/// Remove JSON nulls, at every depth.
pub fn strip_nulls(v: &Json) -> Json {
    match v {
        Json::Object(m) => Json::Object(
            m.iter()
                .filter(|(_, x)| !x.is_null())
                .map(|(k, x)| (k.clone(), strip_nulls(x)))
                .collect(),
        ),
        Json::Array(xs) => Json::Array(xs.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

/// The `fieldsV1` set the field manager `dform` applied, if the object
/// records one.
pub fn managed(live: &Json) -> Option<&Json> {
    live.pointer("/metadata/managedFields")?
        .as_array()?
        .iter()
        .find(|e| {
            e.get("manager").and_then(Json::as_str) == Some(MANAGER)
                && e.get("operation").and_then(Json::as_str) == Some("Apply")
        })?
        .get("fieldsV1")
}

/// The part of `live` a `fieldsV1` set names: `f:NAME` a field, `k:{KEYS}`
/// the list element with those keys, `v:VALUE` a set's value, `i:N` a list
/// index, `.` the element itself. A field whose set is empty is owned
/// whole.
pub fn owned(live: &Json, set: &Json) -> Option<Json> {
    let set = set.as_object()?;
    if set.keys().all(|k| k == ".") {
        return Some(live.clone());
    }
    match live {
        Json::Object(m) => {
            let mut out = Map::new();
            for (k, sub) in set {
                if let Some(name) = k.strip_prefix("f:")
                    && let Some(v) = m.get(name)
                    && let Some(x) = owned(v, sub)
                {
                    out.insert(name.to_string(), x);
                }
            }
            Some(Json::Object(out))
        }
        Json::Array(xs) => {
            let mut out = Vec::new();
            for (i, x) in xs.iter().enumerate() {
                let sub = set.iter().find_map(|(k, sub)| {
                    let hit = if let Some(keys) = k.strip_prefix("k:") {
                        serde_json::from_str::<Map<String, Json>>(keys)
                            .is_ok_and(|keys| keys.iter().all(|(kk, kv)| x.get(kk) == Some(kv)))
                    } else if let Some(v) = k.strip_prefix("v:") {
                        serde_json::from_str::<Json>(v).is_ok_and(|v| &v == x)
                    } else if let Some(n) = k.strip_prefix("i:") {
                        n.parse::<usize>() == Ok(i)
                    } else {
                        false
                    };
                    hit.then_some(sub)
                });
                if let Some(sub) = sub
                    && let Some(v) = owned(x, sub)
                {
                    out.push(v);
                }
            }
            Some(Json::Array(out))
        }
        other => Some(other.clone()),
    }
}

/// The configured attributes of a live object: the fields `dform` owns (all
/// but the server's, if the object records no managed fields), without
/// what its type and computed values say, and `metadata.generateName`.
pub fn attrs(live: &Json) -> Json {
    let live = strip_nulls(live);
    let mut out = match managed(&live).and_then(|set| owned(&live, set)) {
        Some(o) => o,
        None => live.clone(),
    };
    let Json::Object(m) = &mut out else {
        return json!({});
    };
    for k in ["apiVersion", "kind", "status"] {
        m.remove(k);
    }
    if let Some(Json::Object(meta)) = m.get_mut("metadata") {
        for k in COMPUTED_META
            .iter()
            .chain(&["name", "namespace", "managedFields", "selfLink"])
        {
            meta.remove(*k);
        }
        if let Some(g) = live.pointer("/metadata/generateName") {
            meta.insert("generateName".into(), g.clone());
        }
        for (field, k) in [("annotations", KEY_ANNOTATION), ("labels", STACK_LABEL)] {
            if let Some(Json::Object(a)) = meta.get_mut(field) {
                a.remove(k);
                if a.is_empty() {
                    meta.remove(field);
                }
            }
        }
    }
    if m.get("metadata").is_some_and(|x| x == &json!({})) {
        m.remove("metadata");
    }
    // A Secret's `stringData` is write-only: the server folds it into
    // `data`, base64-encoded. The keys dform applied there read back
    // decoded from `data`.
    if live.get("kind").and_then(Json::as_str) == Some("Secret")
        && let Some(Json::Object(keys)) = managed(&live).and_then(|set| set.get("f:stringData"))
    {
        let mut sd = Map::new();
        for k in keys.keys().filter_map(|k| k.strip_prefix("f:")) {
            if let Some(v) = live.pointer(&format!("/data/{k}")).and_then(Json::as_str)
                && let Some(text) = base64(v).and_then(|b| String::from_utf8(b).ok())
            {
                sd.insert(k.to_string(), Json::String(text));
            }
        }
        if !sd.is_empty() {
            m.insert("stringData".into(), Json::Object(sd));
        }
    }
    out
}

/// Standard base64, padded.
fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Standard base64, padded.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            });
        }
    }
    out
}

/// The computed values of a live object: its server-written metadata, its
/// name and namespace, `status`, and its value at each path of `defaulted`
/// (the type's other Optional+Computed paths outside a list element).
pub fn computed(live: &Json, defaulted: &[String]) -> Json {
    let live = strip_nulls(live);
    let mut meta = Map::new();
    for k in COMPUTED_META.iter().chain(&["name", "namespace"]) {
        if let Some(v) = live.pointer(&format!("/metadata/{k}")) {
            meta.insert(k.to_string(), v.clone());
        }
    }
    let mut out = Map::new();
    out.insert("metadata".into(), Json::Object(meta));
    if let Some(s) = live.get("status") {
        out.insert("status".into(), s.clone());
    }
    let mut out = Json::Object(out);
    for p in defaulted {
        if let Some(v) = get_path(&live, p) {
            set_path(&mut out, p, v.clone());
        }
    }
    out
}

/// A remote id: `NAMESPACE/NAME`, or `NAME` for a cluster-scoped kind (or
/// an object in the default namespace `default_ns`).
pub fn remote(kind: &Kind, ns: &str, name: &str) -> String {
    if kind.namespaced {
        format!("{ns}/{name}")
    } else {
        name.to_string()
    }
}

/// A remote id's namespace and name.
pub fn parse_remote<'a>(kind: &Kind, remote: &'a str, default_ns: &'a str) -> (&'a str, &'a str) {
    match remote.split_once('/') {
        Some((ns, name)) if kind.namespaced => (ns, name),
        _ => (default_ns, remote),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment() -> Kind {
        Kind {
            group: "apps".into(),
            version: "v1".into(),
            kind: "Deployment".into(),
            plural: "deployments".into(),
            namespaced: true,
        }
    }

    #[test]
    fn manifest_names_the_kind_and_the_object() {
        let m = manifest(
            &deployment(),
            &json!({"metadata": {"labels": {"a": "b"}}, "spec": {"replicas": 2}}),
            "shop",
            "web",
        )
        .unwrap();
        assert_eq!(
            m,
            json!({"apiVersion": "apps/v1", "kind": "Deployment",
                   "metadata": {"name": "web", "namespace": "shop", "labels": {"a": "b"}},
                   "spec": {"replicas": 2}})
        );
        let e = manifest(&deployment(), &json!({"kind": "Service"}), "a", "b").unwrap_err();
        assert!(e.to_string().contains("kind is \"Service\""), "{e}");
    }

    #[test]
    fn manifest_never_carries_status_or_managed_fields() {
        let m = manifest(
            &deployment(),
            &json!({"metadata": {"managedFields": [{"manager": "x"}]},
                    "spec": {"replicas": 2}, "status": {"readyReplicas": 2}}),
            "shop",
            "web",
        )
        .unwrap();
        assert_eq!(
            m,
            json!({"apiVersion": "apps/v1", "kind": "Deployment",
                   "metadata": {"name": "web", "namespace": "shop"},
                   "spec": {"replicas": 2}})
        );
    }

    #[test]
    fn owned_fields_follow_the_fields_v1_set() {
        let live = json!({
            "spec": {
                "replicas": 3,
                "strategy": {"type": "RollingUpdate"},
                "template": {"spec": {"containers": [
                    {"name": "web", "image": "nginx", "imagePullPolicy": "Always",
                     "ports": [{"containerPort": 80, "protocol": "TCP"}]},
                    {"name": "sidecar", "image": "envoy"}
                ]}},
            },
            "metadata": {"finalizers": ["a", "b"], "labels": {"x": "1", "y": "2"}}
        });
        let set = json!({
            "f:metadata": {"f:finalizers": {"v:\"b\"": {}}, "f:labels": {"f:x": {}}},
            "f:spec": {
                "f:replicas": {},
                "f:template": {"f:spec": {"f:containers": {
                    "k:{\"name\":\"web\"}": {".": {}, "f:name": {}, "f:image": {},
                                             "f:ports": {}}
                }}}
            }
        });
        assert_eq!(
            owned(&live, &set).unwrap(),
            json!({
                "metadata": {"finalizers": ["b"], "labels": {"x": "1"}},
                "spec": {"replicas": 3, "template": {"spec": {"containers": [
                    {"name": "web", "image": "nginx",
                     "ports": [{"containerPort": 80, "protocol": "TCP"}]}
                ]}}}
            })
        );
        assert_eq!(
            owned(&json!(["a", "b", "c"]), &json!({"i:2": {}})).unwrap(),
            json!(["c"])
        );
    }

    #[test]
    fn a_live_object_splits_into_attrs_and_computed() {
        let live = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {
                "name": "web-config-x7k2p", "generateName": "web-config-",
                "namespace": "shop", "uid": "u-1", "resourceVersion": "42",
                "creationTimestamp": "2026-09-28T00:00:00Z",
                "annotations": null,
                "labels": {"team": "a", "other": "b"},
                "managedFields": [
                    {"manager": "kubectl", "operation": "Update",
                     "fieldsV1": {"f:metadata": {"f:labels": {"f:other": {}}}}},
                    {"manager": "dform", "operation": "Apply",
                     "fieldsV1": {"f:data": {"f:MODE": {}},
                                  "f:metadata": {"f:labels": {"f:team": {}}}}}
                ]
            },
            "data": {"MODE": "production", "EXTRA": "x"}
        });
        assert_eq!(
            attrs(&live),
            json!({"data": {"MODE": "production"},
                   "metadata": {"generateName": "web-config-", "labels": {"team": "a"}}})
        );
        assert_eq!(
            computed(&live, &[]),
            json!({"metadata": {"name": "web-config-x7k2p", "namespace": "shop", "uid": "u-1",
                                "resourceVersion": "42",
                                "creationTimestamp": "2026-09-28T00:00:00Z"}})
        );
    }

    #[test]
    fn the_stack_label_is_not_configuration() {
        assert_eq!(stack_label("app[env=prod]"), "app_env_prod");
        assert_eq!(stack_label("k8s_demo"), "k8s_demo");
        assert_eq!(stack_label(&"x".repeat(80)).len(), 63);
        let mut m = json!({"metadata": {"name": "a", "labels": {"app": "a"}}, "data": {}});
        stamp(&mut m, "p");
        assert_eq!(m["metadata"]["labels"][STACK_LABEL], "p");
        let live = json!({"kind": "ConfigMap", "metadata": {"name": "a",
            "labels": {STACK_LABEL: "p"}, "annotations": {KEY_ANNOTATION: "dform-1"}},
            "data": {"A": "b"}});
        assert_eq!(attrs(&live), json!({"data": {"A": "b"}}));
    }

    #[test]
    fn a_secrets_string_data_reads_back_from_data() {
        let live = json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "token", "managedFields": [
                {"manager": "dform", "operation": "Apply",
                 "fieldsV1": {"f:stringData": {"f:password": {}}}}]},
            "data": {"password": "aHVudGVyMg==", "other": "eA=="},
            "type": "Opaque"
        });
        assert_eq!(attrs(&live), json!({"stringData": {"password": "hunter2"}}));
        assert_eq!(base64("aGk=").unwrap(), b"hi");
        assert_eq!(base64("YWJj").unwrap(), b"abc");
        for s in ["", "h", "hi", "abc", "abcd"] {
            assert_eq!(base64(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(base64_encode(b"hi"), "aGk=");
    }

    #[test]
    fn without_managed_fields_everything_but_the_servers_is_configuration() {
        let live = json!({"apiVersion": "v1", "kind": "Namespace",
                          "metadata": {"name": "shop", "uid": "u"},
                          "spec": {"finalizers": ["kubernetes"]},
                          "status": {"phase": "Active"}});
        assert_eq!(
            attrs(&live),
            json!({"spec": {"finalizers": ["kubernetes"]}})
        );
        assert_eq!(
            computed(&live, &[]),
            json!({"metadata": {"name": "shop", "uid": "u"}, "status": {"phase": "Active"}})
        );
    }

    #[test]
    fn remote_ids_carry_the_namespace() {
        let d = deployment();
        assert_eq!(remote(&d, "shop", "web"), "shop/web");
        assert_eq!(parse_remote(&d, "shop/web", "default"), ("shop", "web"));
        assert_eq!(parse_remote(&d, "web", "default"), ("default", "web"));
        let ns = Kind {
            group: String::new(),
            version: "v1".into(),
            kind: "Namespace".into(),
            plural: "namespaces".into(),
            namespaced: false,
        };
        assert_eq!(remote(&ns, "default", "shop"), "shop");
        assert_eq!(parse_remote(&ns, "shop", "default"), ("default", "shop"));
    }
}
