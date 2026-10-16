//! A fake Kubernetes API server, enough of one for the provider
//! (`/openapi/v3` and its documents, GET and list, server-side apply with
//! field managers and dry runs, a merge patch, DELETE): tests/k8s_offline.rs
//! runs the native provider against it, tests/host_wasm.rs the wasm
//! provider.

use crate::common::{Scratch, repo};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Objects by URL path, and the requests seen.
#[derive(Default)]
pub struct Api {
    pub objects: Mutex<BTreeMap<String, Json>>,
    pub requests: Mutex<Vec<String>>,
    /// Each request's `Authorization` header.
    pub auth: Mutex<Vec<String>>,
    serial: Mutex<u64>,
    /// How many of the next writes (a PATCH not a dry run) take effect
    /// but close the connection instead of answering: a lost answer.
    pub lose_answers: Mutex<usize>,
    /// How many times the OpenAPI index was asked for: the hash of its
    /// `apis` and `version` entries, which k3s's change from one ask to
    /// the next.
    indexes: Mutex<u64>,
    /// The group-version whose document changed, its hash in the index
    /// another.
    pub changed: Mutex<Option<String>>,
}

fn snapshot() -> Json {
    serde_json::from_str(
        &std::fs::read_to_string(repo().join("crates/dform-k8s/openapi-snapshot.json")).unwrap(),
    )
    .unwrap()
}

/// A Status response.
pub fn status(code: u16, reason: &str, message: &str, causes: Json) -> (u16, Json) {
    (
        code,
        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": code,
               "reason": reason, "message": message, "details": {"causes": causes}}),
    )
}

/// The `fieldsV1` set of an applied object: every field, lists whole.
fn fields_of(v: &Json) -> Json {
    match v {
        Json::Object(m) => Json::Object(
            m.iter()
                .map(|(k, x)| (format!("f:{k}"), fields_of(x)))
                .collect(),
        ),
        _ => json!({}),
    }
}

/// A `fieldsV1` set as the document shape `drop_owned` walks: each
/// `f:NAME` key as `f:f:NAME`, so dropping it from another set's document
/// drops the field.
fn fields_set(set: &Json) -> Json {
    match set {
        Json::Object(m) => Json::Object(
            m.iter()
                .map(|(k, x)| (format!("f:{k}"), fields_set(x)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Leaf paths (lists whole) of an object, as `.a.b`.
fn leaves(v: &Json, at: &str, out: &mut Vec<(String, Json)>) {
    match v {
        Json::Object(m) if !m.is_empty() => {
            for (k, x) in m {
                leaves(x, &format!("{at}.{k}"), out);
            }
        }
        _ => out.push((at.to_string(), v.clone())),
    }
}

/// Whether the `fieldsV1` set `set` owns the field at `path` (`.a.b`).
pub fn owns(set: &Json, path: &str) -> bool {
    let mut cur = set;
    for seg in path.trim_start_matches('.').split('.') {
        match cur.get(format!("f:{seg}")) {
            Some(x) if x.as_object().is_some_and(|m| m.is_empty()) => return true,
            Some(x) => cur = x,
            None => return false,
        }
    }
    true
}

fn pointer(path: &str) -> String {
    path.replace('.', "/")
}

/// Drop the fields a `fieldsV1` set names.
fn drop_owned(v: &mut Json, set: &Json) {
    let (Json::Object(m), Json::Object(s)) = (v, set) else {
        return;
    };
    for (k, sub) in s {
        let Some(name) = k.strip_prefix("f:") else {
            continue;
        };
        let whole = sub.as_object().is_none_or(|x| x.is_empty());
        if whole {
            m.remove(name);
        } else if let Some(x) = m.get_mut(name) {
            drop_owned(x, sub);
            if x.as_object().is_some_and(|o| o.is_empty()) {
                m.remove(name);
            }
        }
    }
}

fn merge(into: &mut Json, from: &Json) {
    match (into, from) {
        (Json::Object(a), Json::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(x) if x.is_object() && v.is_object() => merge(x, v),
                    _ => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (a, b) => *a = b.clone(),
    }
}

impl Api {
    pub fn start() -> (Arc<Api>, String) {
        let api = Arc::new(Api::default());
        // A cluster starts with its `default` namespace.
        api.objects.lock().unwrap().insert(
            "/api/v1/namespaces/default".into(),
            json!({"apiVersion": "v1", "kind": "Namespace",
                   "metadata": {"name": "default", "uid": "uid-default"}}),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let a = api.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let a = a.clone();
                std::thread::spawn(move || a.serve(stream));
            }
        });
        (api, url)
    }

    fn serve(&self, stream: TcpStream) {
        let mut r = BufReader::new(stream.try_clone().unwrap());
        let mut w = stream;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let (method, target) = (
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            );
            let (mut len, mut merge) = (0, false);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 {
                    return;
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap_or(0);
                    } else if k.eq_ignore_ascii_case("authorization") {
                        self.auth.lock().unwrap().push(v.trim().to_string());
                    } else if k.eq_ignore_ascii_case("content-type") {
                        merge = v.trim() == "application/merge-patch+json";
                    }
                }
            }
            let mut body = vec![0; len];
            if r.read_exact(&mut body).is_err() {
                return;
            }
            self.requests
                .lock()
                .unwrap()
                .push(format!("{method} {target}"));
            let (code, out) = match merge {
                true => self.merge_patch(&target, &body),
                false => self.route(&method, &target, &body),
            };
            if method == "PATCH" && !target.contains("dryRun=") {
                let mut lose = self.lose_answers.lock().unwrap();
                if *lose > 0 {
                    *lose -= 1;
                    return;
                }
            }
            let out = serde_json::to_vec(&out).unwrap();
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                out.len()
            );
            if w.write_all(head.as_bytes()).is_err() || w.write_all(&out).is_err() {
                return;
            }
        }
    }

    fn route(&self, method: &str, target: &str, body: &[u8]) -> (u16, Json) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let q = |k: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                .map(String::from)
        };
        if path == "/openapi/v3" {
            return (200, self.index());
        }
        if let Some(gv) = path.strip_prefix("/openapi/v3/") {
            return (200, snapshot()["paths"][gv].clone());
        }
        let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let rest = match segs.first() {
            Some(&"api") => &segs[2..],
            Some(&"apis") => &segs[3..],
            _ => return status(404, "NotFound", "no such path", json!([])),
        };
        // A list of a kind in every namespace, by label selector.
        if method == "GET" && rest.len() == 1 {
            let prefix = &path[..path.len() - rest[0].len()];
            let selector = q("labelSelector").map(|s| {
                s.replace("%2F", "/")
                    .replace("%3D", "=")
                    .replace("%2C", ",")
            });
            let matches = |o: &Json| {
                selector.iter().flat_map(|s| s.split(',')).all(|term| {
                    let (k, v) = term.split_once('=').unwrap_or((term, ""));
                    let label = o.pointer("/metadata/labels").and_then(|l| l.get(k));
                    match v {
                        "" => label.is_some(),
                        v => label.and_then(Json::as_str) == Some(v),
                    }
                })
            };
            let objects = self.objects.lock().unwrap();
            let items: Vec<Json> = objects
                .iter()
                .filter(|(p, o)| {
                    p.starts_with(prefix) && p.rsplit('/').nth(1) == Some(rest[0]) && matches(o)
                })
                .map(|(_, o)| o.clone())
                .collect();
            return (
                200,
                json!({"kind": "List", "apiVersion": "v1", "items": items}),
            );
        }
        if rest.len() != 2 && rest.len() != 4 {
            return status(404, "NotFound", "no such path", json!([]));
        }
        let name = rest[rest.len() - 1];
        let mut objects = self.objects.lock().unwrap();
        match method {
            "GET" => match objects.get(path) {
                Some(o) => (200, o.clone()),
                None => status(404, "NotFound", &format!("{name} not found"), json!([])),
            },
            "DELETE" => match objects.remove(path) {
                Some(_) => (200, json!({"kind": "Status", "status": "Success"})),
                None => status(404, "NotFound", &format!("{name} not found"), json!([])),
            },
            "PATCH" => {
                let applied: Json = serde_json::from_slice(body).unwrap();
                // A namespaced object needs its namespace, as a server's
                // admission does (a dry run too).
                if let ["namespaces", ns, _, _] = rest
                    && !objects.contains_key(&format!("/api/v1/namespaces/{ns}"))
                {
                    return status(
                        404,
                        "NotFound",
                        &format!("namespaces \"{ns}\" not found"),
                        json!([]),
                    );
                }
                if q("fieldManager").as_deref() != Some("dform") {
                    return status(400, "BadRequest", "fieldManager is required", json!([]));
                }
                // A named target port has a letter, as the server checks:
                // "8080" is not a port number.
                if applied["kind"] == "Service"
                    && let Some(Json::Array(ports)) = applied.pointer("/spec/ports")
                    && let Some(bad) = ports.iter().find_map(|p| {
                        p["targetPort"]
                            .as_str()
                            .filter(|t| !t.chars().any(|c| c.is_ascii_alphabetic()))
                    })
                {
                    return status(
                        422,
                        "Invalid",
                        &format!(
                            "spec.ports[0].targetPort: Invalid value: \"{bad}\": must contain at least one letter"
                        ),
                        json!([]),
                    );
                }
                let force = q("force").as_deref() == Some("true");
                let dry = q("dryRun").as_deref() == Some("All");
                let old = objects.get(path).cloned();
                let mut conflicts = Vec::new();
                let managers = old
                    .as_ref()
                    .and_then(|o| o.pointer("/metadata/managedFields"))
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut own = Vec::new();
                leaves(&applied, "", &mut own);
                for e in &managers {
                    let m = e["manager"].as_str().unwrap_or("");
                    if m == "dform" {
                        continue;
                    }
                    for (p, v) in &own {
                        let live = old.as_ref().and_then(|o| o.pointer(&pointer(p)));
                        if owns(&e["fieldsV1"], p) && live != Some(v) {
                            conflicts.push(json!({"reason": "FieldManagerConflict",
                                "message": format!("conflict with \"{m}\" using {}", applied["apiVersion"].as_str().unwrap_or("")),
                                "field": p}));
                        }
                    }
                }
                if !conflicts.is_empty() && !force {
                    return status(
                        409,
                        "Conflict",
                        &format!("Apply failed with {} conflict(s)", conflicts.len()),
                        Json::Array(conflicts),
                    );
                }
                let mut n = self.serial.lock().unwrap();
                *n += 1;
                let mut obj = old.clone().unwrap_or(json!({}));
                if let Some(prev) = managers.iter().find(|e| e["manager"] == "dform") {
                    drop_owned(&mut obj, &prev["fieldsV1"]);
                }
                merge(&mut obj, &applied);
                let meta = obj["metadata"].as_object_mut().unwrap();
                let first = old.is_none();
                meta.insert(
                    "uid".into(),
                    old.as_ref()
                        .and_then(|o| o.pointer("/metadata/uid").cloned())
                        .unwrap_or(json!(format!("uid-{}", *n))),
                );
                meta.insert("resourceVersion".into(), json!(n.to_string()));
                let generation = old
                    .as_ref()
                    .and_then(|o| o.pointer("/metadata/generation")?.as_i64())
                    .unwrap_or(0)
                    + 1;
                meta.insert("generation".into(), json!(generation));
                if first {
                    meta.insert("creationTimestamp".into(), json!("2026-09-28T00:00:00Z"));
                }
                let mut fields = fields_of(&applied);
                for k in ["f:apiVersion", "f:kind"] {
                    fields.as_object_mut().unwrap().remove(k);
                }
                if let Some(m) = fields.get_mut("f:metadata").and_then(Json::as_object_mut) {
                    m.remove("f:name");
                    m.remove("f:namespace");
                }
                let mut entries: Vec<Json> = managers
                    .into_iter()
                    .filter(|e| e["manager"] != "dform")
                    .collect();
                entries.push(json!({"manager": "dform", "operation": "Apply",
                                    "apiVersion": applied["apiVersion"], "fieldsV1": fields}));
                meta.insert("managedFields".into(), Json::Array(entries));
                // What a server defaults, and a status.
                if applied["kind"] == "Deployment" {
                    if obj.pointer("/spec/strategy").is_none() {
                        obj["spec"]["strategy"] = json!({"type": "RollingUpdate"});
                    }
                    let replicas = obj.pointer("/spec/replicas").cloned().unwrap_or(json!(1));
                    obj["status"] = json!({"observedGeneration": generation,
                                           "readyReplicas": replicas});
                }
                if applied["kind"] == "Service" && obj.pointer("/spec/clusterIP").is_none() {
                    obj["spec"]["clusterIP"] = json!(format!("10.96.0.{}", *n));
                }
                if !dry {
                    objects.insert(path.to_string(), obj.clone());
                }
                (if first { 201 } else { 200 }, obj)
            }
            _ => status(405, "MethodNotAllowed", method, json!([])),
        }
    }

    /// A JSON merge patch by another field manager, as `kubectl patch`
    /// sends one (an Update): the fields it sets become its own, taken
    /// from whoever owned them.
    /// The OpenAPI index: each group-version of the snapshot at its
    /// document's URL (its hash another once [`Api::changed`] names it),
    /// and, as k3s's, `apis` and `version` at a new hash each ask.
    fn index(&self) -> Json {
        let gvs = snapshot()["paths"].as_object().unwrap().clone();
        let changed = self.changed.lock().unwrap().clone();
        let mut index: serde_json::Map<String, Json> = gvs
            .keys()
            .map(|gv| {
                let hash = match changed.as_ref() == Some(gv) {
                    true => "4567",
                    false => "0123",
                };
                let url = format!("/openapi/v3/{gv}?hash={hash}");
                (gv.clone(), json!({ "serverRelativeURL": url }))
            })
            .collect();
        let mut n = self.indexes.lock().unwrap();
        *n += 1;
        for other in ["apis", "version"] {
            let url = format!("/openapi/v3/{other}?hash={n}");
            index.insert(other.into(), json!({ "serverRelativeURL": url }));
        }
        json!({ "paths": index })
    }

    fn merge_patch(&self, target: &str, body: &[u8]) -> (u16, Json) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let manager = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("fieldManager="))
            .unwrap_or("kubectl-patch")
            .to_string();
        let patch: Json = serde_json::from_slice(body).unwrap();
        let mut objects = self.objects.lock().unwrap();
        let Some(obj) = objects.get_mut(path) else {
            return status(404, "NotFound", "not found", json!([]));
        };
        merge(obj, &patch);
        let fields = fields_of(&patch);
        let entries = obj["metadata"]["managedFields"].as_array_mut().unwrap();
        for e in entries.iter_mut() {
            drop_owned(&mut e["fieldsV1"], &fields_set(&fields));
        }
        entries.push(json!({"manager": manager, "operation": "Update", "fieldsV1": fields}));
        (200, obj.clone())
    }

    pub fn get(&self, path: &str) -> Option<Json> {
        self.objects.lock().unwrap().get(path).cloned()
    }

    /// Requests starting with `prefix` and holding each of `parts`.
    pub fn count(&self, prefix: &str, parts: &[&str]) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with(prefix) && parts.iter().all(|p| r.contains(p)))
            .count()
    }
}

/// A kubeconfig naming the fake server, namespace `default`.
pub fn kubeconfig(s: &Scratch, url: &str) -> String {
    s.write(
        "kubeconfig",
        &format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: fake\n  cluster:\n    server: {url}\n\
             contexts:\n- name: fake\n  context:\n    cluster: fake\n    user: fake\n    namespace: default\n\
             current-context: fake\nusers:\n- name: fake\n  user: {{}}\n"
        ),
    )
    .display()
    .to_string()
}
