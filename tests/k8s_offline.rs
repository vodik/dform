//! The Kubernetes provider (`dform-provider-k8s`) without a cluster: offline
//! (the schema from the checked-in OpenAPI snapshot, Plan local), and
//! against a fake API server that speaks enough of the Kubernetes API
//! (`/openapi/v3`, GET, server-side apply with field managers and dry runs,
//! DELETE) for the conformance suite and a whole apply.

mod common;
use common::{Run, Scratch, repo};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn k8s() -> String {
    common::exe("dform-provider-k8s")
}

/// `dform ARGS` in the scratch directory with no cluster in reach but the
/// one `kubeconfig` names (none: offline).
fn dform<S: AsRef<std::ffi::OsStr>>(s: &Scratch, kubeconfig: Option<&str>, args: &[S]) -> Run {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dform"));
    c.args(args)
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT");
    match kubeconfig {
        Some(k) => c.env("KUBECONFIG", k).env_remove("DFORM_K8S_OFFLINE"),
        None => c.env("DFORM_K8S_OFFLINE", "1"),
    };
    Run::from(c.output().unwrap())
}

/// The demo with its provider statement pointed at the real provider:
/// `providers/k8s/` beside it holds the executable.
fn real_demo(s: &Scratch) {
    let src = std::fs::read_to_string(repo().join("examples/k8s/stacks/k8s_demo.df")).unwrap();
    let real = src.replace(
        "provider k8s {}",
        "provider k8s { source = \"./providers/k8s\" }",
    );
    assert_ne!(
        src, real,
        "the demo names its provider `provider k8s {{}}.`"
    );
    s.write("k8s_demo.df", &real);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
}

fn mock_demo(s: &Scratch) {
    s.write(
        "k8s_demo.df",
        &std::fs::read_to_string(repo().join("examples/k8s/stacks/k8s_demo.df")).unwrap(),
    );
}

/// The plan file's parts that come from the program and the providers.
fn planned(s: &Scratch) -> Json {
    let p: Json = serde_json::from_str(&s.read("plan.json")).unwrap();
    json!({"deformations": p["deformations"], "nulls": p["nulls"],
           "pending_groups": p["pending_groups"], "ticks": p["ticks"]})
}

/// Acceptance: examples/k8s/stacks/k8s_demo.df plans against the mock unchanged, and
/// the same program with `source = "./providers/k8s"` plans against the
/// real provider offline to the same plan: every leaf of every desired
/// document (a create's changes are all of them), the same nulls, ticks
/// and text.
#[test]
fn the_demo_plans_the_same_against_the_real_provider_offline() {
    let mock = Scratch::project("k8s-demo-mock");
    mock_demo(&mock);
    let m = dform(&mock, None, &["plan", "--out", "plan.json", "k8s_demo.df"]).success();
    let real = Scratch::project("k8s-demo-real");
    real_demo(&real);
    let r = dform(&real, None, &["plan", "--out", "plan.json", "k8s_demo.df"]).success();
    assert_eq!(r.stdout, m.stdout);
    assert_eq!(planned(&real), planned(&mock));
    assert!(
        r.stdout
            .contains("configMapRef.name = ?k8s.config_map/web_config#metadata.name"),
        "{}",
        r.stdout
    );
    // The long names are the same types.
    real.write(
        "long.df",
        "edition 2026\nprovider k8s { source = \"./providers/k8s\" }\nresource k8s.apps.v1.deployment api {\n  metadata.name = \"api\"\n  spec.selector.matchLabels = {app: \"api\"}\n  spec.template.spec.containers = [{name: \"api\", image: \"api:1\"}]\n}\n",
    );
    let r = dform(&real, None, &["plan", "long.df"]).success();
    assert!(
        r.stdout.contains("+ k8s.apps.v1.deployment.api")
            && r.stdout
                .contains("spec.template.spec.containers[name=api].image = \"api:1\""),
        "{}",
        r.stdout
    );
}

/// The derived schema is 16k facts; a run injects those of the types the
/// program names (and their aliases' targets), while a query of the schema,
/// or a rule reading the schema of a type it does not name, sees all of it.
#[test]
fn a_run_injects_the_schema_of_the_types_it_names() {
    let s = Scratch::new("k8s-schema-scope");
    real_demo(&s);
    let count = |r: &Run| -> usize {
        let last = r.stdout.lines().last().unwrap_or_default();
        last.rsplit(' ').next().unwrap().parse().expect(&r.stdout)
    };
    let r = dform(&s, None, &["dev", "eval", "k8s_demo.df"]).success();
    let facts: usize = r.stdout.lines().next().unwrap()["facts: ".len()..]
        .parse()
        .unwrap();
    assert!(
        facts < 5_000,
        "{facts} facts: the whole schema was injected"
    );
    let all = count(&dform(&s, None, &["query", "type_attr", "k8s_demo.df"]).success());
    assert!(all > 15_000, "query type_attr lists {all}");
    let r = dform(
        &s,
        None,
        &[
            "query",
            "type_attr(k8s.batch.v1.job, P, T, F)",
            "k8s_demo.df",
        ],
    )
    .success();
    assert!(r.stdout.contains("spec.completions"), "{}", r.stdout);
    let src = s.read("k8s_demo.df");
    s.write(
        "reads.df",
        &format!("{src}\ncompletes(t) if type_attr(t, \"spec.completions\", _, _)\n"),
    );
    let r = dform(&s, None, &["query", "completes", "reads.df"]).success();
    assert!(r.stdout.contains("k8s.batch.v1.job"), "{}", r.stdout);
}

/// Offline, Plan validates against the derived schema: a required field
/// is named with the resource, and a Secret's data is never printed.
#[test]
fn offline_plan_validates_and_hides_secrets() {
    let s = Scratch::project("k8s-offline-plan");
    real_demo(&s);
    s.write(
        "p.df",
        "edition 2026\nprovider k8s { source = \"./providers/k8s\" }\nresource k8s.secret token {\n  metadata.name = \"token\"\n  stringData = {password: \"hunter2\"}\n}\n",
    );
    let r = dform(&s, None, &["plan", "p.df"]).success();
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
    assert!(
        r.stdout.contains("stringData.password = (sensitive)"),
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        "edition 2026\nprovider k8s { source = \"./providers/k8s\" }\nresource k8s.deployment api {\n  metadata.name = \"api\"\n  spec.template.spec.containers = [{name: \"api\", image: \"api:1\"}]\n}\n",
    );
    let r = dform(&s, None, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("plan k8s.deployment/api: required attribute spec.selector is not set"),
        "{}",
        r.stderr
    );
}

/// Offline, what needs the cluster fails naming why.
#[test]
fn offline_apply_fails_naming_why() {
    let s = Scratch::project("k8s-offline-apply");
    real_demo(&s);
    let r = dform(&s, None, &["apply", "k8s_demo.df"]).failure();
    assert!(
        r.stderr.contains("no cluster (DFORM_K8S_OFFLINE is set)"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

// --- a fake API server ------------------------------------------------------

/// Objects by URL path, and the requests seen.
#[derive(Default)]
struct Api {
    objects: Mutex<BTreeMap<String, Json>>,
    requests: Mutex<Vec<String>>,
    serial: Mutex<u64>,
}

fn snapshot() -> Json {
    serde_json::from_str(
        &std::fs::read_to_string(repo().join("crates/dform-k8s/openapi-snapshot.json")).unwrap(),
    )
    .unwrap()
}

/// A Status response.
fn status(code: u16, reason: &str, message: &str, causes: Json) -> (u16, Json) {
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

fn owns(set: &Json, path: &str) -> bool {
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
    fn start() -> (Arc<Api>, String) {
        let api = Arc::new(Api::default());
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
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 {
                    return;
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
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
            let (code, out) = self.route(&method, &target, &body);
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
            let gvs = snapshot()["paths"].as_object().unwrap().clone();
            let index: serde_json::Map<String, Json> = gvs
                .keys()
                .map(|gv| {
                    let url = format!("/openapi/v3/{gv}?hash=0123");
                    (gv.clone(), json!({ "serverRelativeURL": url }))
                })
                .collect();
            return (200, json!({ "paths": index }));
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
                if q("fieldManager").as_deref() != Some("dform") {
                    return status(400, "BadRequest", "fieldManager is required", json!([]));
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

    fn get(&self, path: &str) -> Option<Json> {
        self.objects.lock().unwrap().get(path).cloned()
    }

    /// Requests starting with `prefix` and holding each of `parts`.
    fn count(&self, prefix: &str, parts: &[&str]) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with(prefix) && parts.iter().all(|p| r.contains(p)))
            .count()
    }
}

/// A kubeconfig naming the fake server, namespace `default`.
fn kubeconfig(s: &Scratch, url: &str) -> String {
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

/// `dform provider check` against the k8s binary: no cluster, the fake API
/// server standing in. The checks run on the provider's own examples.
#[test]
fn the_k8s_provider_conforms() {
    let s = Scratch::project("k8s-check");
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let r = dform(&s, Some(&kc), &["provider", "check", &k8s()]).success();
    assert!(!r.stdout.contains("FAIL"), "{}", r.stdout);
    for line in [
        "ok    Schema serves its own types, with examples",
        "ok    Plan refuses a document without a required attribute",
        "ok    Plan spells a keyed list by key",
        "ok    Plan marks a sensitive attribute sensitive",
        "ok    Apply CREATE returns the object with its computed values",
        "ok    Apply CREATE again with the same idempotency key answers the object it made",
        "skip  Query provider.created: no `managed` capability",
        "ok    Apply UPDATE changes the object in place",
        "ok    Apply REPLACE makes a new object",
        "ok    Apply DELETE removes the object",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
    assert!(r.stdout.ends_with("conforms\n"), "{}", r.stdout);
    // Plans were dry runs; writes were server-side applies as dform.
    assert!(
        api.count(
            "PATCH /api/v1/namespaces/default/services/dform-check?",
            &["dryRun=All", "fieldManager=dform"]
        ) > 0
    );
    assert!(
        api.objects.lock().unwrap().is_empty(),
        "the check cleans up"
    );
}

/// The demo applies through the fake API server: server-side apply as
/// `dform`, a generated ConfigMap name the Deployment then refers to, the
/// objects as the program describes them (the mock's world holds the same
/// documents), and nothing to do afterwards although the server added
/// defaults. The schema is cached in the stack's state directory.
#[test]
fn the_demo_applies_through_the_api_server() {
    let s = Scratch::project("k8s-apply");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    run(&["apply"]).success();

    let dep = api
        .get("/apis/apps/v1/namespaces/shop/deployments/web")
        .expect("the deployment");
    let cm_name = dep
        .pointer("/spec/template/spec/containers/0/envFrom/0/configMapRef/name")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    assert!(
        cm_name.starts_with("web-config-") && cm_name.len() == 16,
        "{cm_name}"
    );
    let cm = api
        .get(&format!("/api/v1/namespaces/shop/configmaps/{cm_name}"))
        .expect("the generated configmap");
    assert_eq!(cm["metadata"]["generateName"], "web-config-");
    assert!(api.get("/api/v1/namespaces/shop").is_some());
    assert!(api.get("/api/v1/namespaces/shop/services/web").is_some());

    // The same program applied to the mock: the same documents.
    let mock = Scratch::project("k8s-apply-mock");
    mock_demo(&mock);
    dform(
        &mock,
        None,
        &["dev", "--world", "w.json", "apply", "k8s_demo.df"],
    )
    .success();
    let w: Json = serde_json::from_str(&mock.read("w.json")).unwrap();
    let configured = |o: &Json| {
        let mut o = o.clone();
        for k in ["apiVersion", "kind", "status"] {
            o.as_object_mut().unwrap().remove(k);
        }
        let m = o["metadata"].as_object_mut().unwrap();
        for k in [
            "uid",
            "resourceVersion",
            "generation",
            "creationTimestamp",
            "managedFields",
        ] {
            m.remove(k);
        }
        // The Create's idempotency key rides on an annotation, not
        // configuration.
        let a = m["annotations"].as_object_mut().unwrap();
        assert!(a.remove("dform.io/idempotency-key").is_some(), "{a:?}");
        if a.is_empty() {
            m.remove("annotations");
        }
        o
    };
    let mut want = w["resources"]["k8s.deployment::web"]["attrs"].clone();
    want["spec"]["template"]["spec"]["containers"][0]["envFrom"][0]["configMapRef"]["name"] =
        json!(cm_name);
    let mut got = configured(&dep);
    got["spec"].as_object_mut().unwrap().remove("strategy");
    assert_eq!(got, want);

    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);
    let cache = s.path("dform.state/cache/k8s-openapi.json");
    assert!(cache.exists(), "the schema is cached in dform.state/cache/");
    assert!(
        s.path("dform.state/cache/k8s-schema.json").exists(),
        "and the schema derived from it"
    );
    let fetched = api.count("GET /openapi/v3/", &[]);
    run(&["plan"]).success();
    assert_eq!(
        api.count("GET /openapi/v3/", &[]),
        fetched,
        "a cached schema whose index is the server's is not fetched again"
    );

    // A change to the program is an update in place, planned by dry run.
    s.write(
        "k8s_demo.df",
        &s.read("k8s_demo.df")
            .replace("\"nginx:1.27\"", "\"nginx:1.28\""),
    );
    let dry = api.count(
        "PATCH /apis/apps/v1/namespaces/shop/deployments/web?",
        &["dryRun=All"],
    );
    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains(
            "spec.template.spec.containers[name=web].image: \"nginx:1.27\" -> \"nginx:1.28\""
        ),
        "{}",
        r.stdout
    );
    assert!(
        api.count(
            "PATCH /apis/apps/v1/namespaces/shop/deployments/web?",
            &["dryRun=All"],
        ) > dry,
        "plan is a dry-run apply"
    );
    run(&["apply"]).success();
    let dep = api
        .get("/apis/apps/v1/namespaces/shop/deployments/web")
        .unwrap();
    assert_eq!(
        dep["spec"]["template"]["spec"]["containers"][0]["image"],
        "nginx:1.28"
    );
}

/// A field the API server defaults (`spec.clusterIP`) is Optional+Computed:
/// a ref to it is a null until the Service exists, then the cluster's value.
#[test]
fn a_ref_to_a_server_defaulted_field_resolves_from_the_cluster() {
    let s = Scratch::project("k8s-defaulted");
    real_demo(&s);
    s.write(
        "k8s_demo.df",
        &format!(
            "{}\nresource k8s.config_map endpoints {{\n  metadata.name = \"endpoints\"\n  \
             metadata.namespace = shop.metadata.name\n  \
             data = {{ \"WEB\": k8s.service[\"web\"].spec.clusterIP }}\n}}\n",
            s.read("k8s_demo.df")
        ),
    );
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    let r = run(&["plan"]).success();
    assert!(
        r.stdout
            .contains("data.WEB = ?k8s.service/web#spec.clusterIP"),
        "{}",
        r.stdout
    );
    run(&["apply"]).success();
    let svc = api.get("/api/v1/namespaces/shop/services/web").unwrap();
    let ip = svc["spec"]["clusterIP"].as_str().unwrap().to_string();
    assert!(ip.starts_with("10.96.0."), "{svc}");
    let cm = api
        .get("/api/v1/namespaces/shop/configmaps/endpoints")
        .expect("the configmap");
    assert_eq!(cm["data"]["WEB"], json!(ip));
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);
}

/// The field managers of `obj` that own `path` (`.a.b`) per its managedFields.
fn managers_of(obj: &Json, path: &str) -> Vec<String> {
    obj.pointer("/metadata/managedFields")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter(|e| owns(&e["fieldsV1"], path))
        .map(|e| e["manager"].as_str().unwrap_or("").to_string())
        .collect()
}

/// A value the server defaulted (`spec.clusterIP`) is read back, but an
/// update never sends it: after a second apply of the Service the field is
/// still not dform's, and nothing is left to do.
#[test]
fn an_update_leaves_server_defaulted_fields_to_the_server() {
    let s = Scratch::project("k8s-defaulted-owner");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    let svc = || api.get("/api/v1/namespaces/shop/services/web").unwrap();
    run(&["apply"]).success();
    let ip = svc()["spec"]["clusterIP"].clone();
    assert!(ip.is_string(), "{}", svc());
    assert_eq!(managers_of(&svc(), ".spec.clusterIP"), Vec::<String>::new());

    s.write(
        "k8s_demo.df",
        &s.read("k8s_demo.df").replace("port: 80,", "port: 8080,"),
    );
    let r = run(&["plan"]).success();
    assert!(r.stdout.contains("~ k8s.service.web"), "{}", r.stdout);
    assert!(!r.stdout.contains("clusterIP"), "{}", r.stdout);
    let updates = api.count("PATCH /api/v1/namespaces/shop/services/web?", &[]);
    run(&["apply"]).success();
    assert!(
        api.count("PATCH /api/v1/namespaces/shop/services/web?", &[]) > updates,
        "the Service is applied again"
    );
    let now = svc();
    assert_eq!(now["spec"]["ports"][0]["port"], 8080);
    assert_eq!(now["spec"]["clusterIP"], ip, "the server keeps its value");
    assert_eq!(
        managers_of(&now, ".spec.clusterIP"),
        Vec::<String>::new(),
        "dform does not own a field the server defaulted: {now}"
    );
    assert_eq!(managers_of(&now, ".spec.ports"), vec!["dform".to_string()]);
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);

    // The generated ConfigMap name is the server's too, so the document
    // has none; Plan's dry run names the object by its remote id.
    s.write(
        "k8s_demo.df",
        &s.read("k8s_demo.df")
            .replace("\"LOG_LEVEL\": \"info\"", "\"LOG_LEVEL\": \"debug\""),
    );
    let dry = api.count(
        "PATCH /api/v1/namespaces/shop/configmaps/web-config-",
        &["dryRun=All"],
    );
    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains("data.LOG_LEVEL: \"info\" -> \"debug\""),
        "{}",
        r.stdout
    );
    assert!(
        api.count(
            "PATCH /api/v1/namespaces/shop/configmaps/web-config-",
            &["dryRun=All"]
        ) > dry,
        "the update of a generated name is a dry run"
    );
    run(&["apply"]).success();
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);
}

/// Another field manager takes `spec.replicas`: dform's plan puts it back,
/// and the apply (never forced) fails naming the manager and the field.
#[test]
fn a_field_another_manager_owns_fails_the_apply_naming_both() {
    let s = Scratch::project("k8s-conflict");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    run(&["apply"]).success();
    {
        let mut objects = api.objects.lock().unwrap();
        let dep = objects
            .get_mut("/apis/apps/v1/namespaces/shop/deployments/web")
            .unwrap();
        dep["spec"]["replicas"] = json!(5);
        let entries = dep["metadata"]["managedFields"].as_array_mut().unwrap();
        for e in entries.iter_mut() {
            if e["manager"] == "dform" {
                e["fieldsV1"]["f:spec"]
                    .as_object_mut()
                    .unwrap()
                    .remove("f:replicas");
            }
        }
        entries.push(json!({"manager": "kubectl", "operation": "Update",
                            "fieldsV1": {"f:spec": {"f:replicas": {}}}}));
    }
    let r = run(&["plan"]).success();
    assert!(r.stdout.contains("~ k8s.deployment.web"), "{}", r.stdout);
    let r = run(&["apply"]).failure();
    assert!(
        r.stderr.contains("apply k8s.deployment/web")
            && r.stderr
                .contains(".spec.replicas is owned by field manager \"kubectl\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(
        api.get("/apis/apps/v1/namespaces/shop/deployments/web")
            .unwrap()["spec"]["replicas"],
        5,
        "not forced"
    );
}

/// A type built at runtime (`format`) is named nowhere: the run asks for
/// the whole schema, so the Job still gets its computed nulls.
#[test]
fn a_type_built_at_runtime_gets_the_whole_schema() {
    let s = Scratch::project("k8s-runtime-type");
    real_demo(&s);
    s.write(
        "k8s_demo.df",
        &format!(
            "{}\nwant(t, \"batch\") if k = \"job\", t = \"k8s.batch.v1.{{k}}\"\n",
            s.read("k8s_demo.df")
        ),
    );
    let r = dform(&s, None, &["dev", "eval", "k8s_demo.df"]).success();
    let facts: usize = r.stdout.lines().next().unwrap()["facts: ".len()..]
        .parse()
        .unwrap();
    assert!(facts > 15_000, "{facts} facts: the schema was scoped");
    let r = dform(
        &s,
        None,
        &[
            "query",
            "attr(k8s.batch.v1.job, \"batch\", .metadata, V)",
            "k8s_demo.df",
        ],
    )
    .success();
    assert!(
        r.stdout
            .contains("uid: ?k8s.batch.v1.job/batch#metadata.uid"),
        "{}",
        r.stdout
    );
}

/// The provider, offline, as a plugin connection: configured with its
/// state directory `dir`.
fn offline_provider(s: &Scratch, dir: &str) -> dform::plugin::link::Link {
    let env = dform_grpc::spawn::Env::default()
        .unset("KUBECONFIG")
        .set("DFORM_K8S_OFFLINE", "1");
    let mut conn = dform_grpc::client::Conn::link(std::path::Path::new(&k8s()), &env).unwrap();
    let config = json!({"world": s.path(&format!("{dir}/world.json")).display().to_string()});
    let config = Some(dform::plugin::wire::doc(&config));
    let _: dform::plugin::pb::ConfigureResponse = conn
        .call(dform::plugin::pb::ConfigureRequest { config })
        .unwrap();
    conn
}

/// A Schema answer's facts, spelled.
fn schema_of(conn: &mut dform::plugin::link::Link, types: Option<&[&str]>) -> Vec<String> {
    use dform::plugin::{pb, wire};
    let req = pb::SchemaRequest {
        types: types.map(|t| pb::TypeFilter {
            names: t.iter().map(|s| s.to_string()).collect(),
        }),
    };
    let resp: pb::SchemaResponse = conn.call(req).unwrap();
    resp.facts
        .iter()
        .map(|f| dform::partition::fmt_atom(&wire::from_fact(f).unwrap()))
        .collect()
}

/// The provider derives the schema once per OpenAPI document and caches
/// it beside the document's cache (`k8s-schema.json`, keyed by its hash):
/// a Schema request naming types is answered from it with their rows (and
/// their alias targets') and every type_provider row.
#[test]
fn the_schema_is_derived_once_and_answered_for_the_types_asked_for() {
    let s = Scratch::project("k8s-schema-cache");
    let count =
        |facts: &[String], prefix: &str| facts.iter().filter(|f| f.starts_with(prefix)).count();
    let all = schema_of(&mut offline_provider(&s, "st"), None);
    assert!(count(&all, "type_attr(") > 15_000);
    let cache = s.path("st/k8s-schema.json");
    assert!(cache.exists(), "the derived schema is cached");

    let some = schema_of(&mut offline_provider(&s, "st"), Some(&["k8s.deployment"]));
    let types: std::collections::BTreeSet<&str> = some
        .iter()
        .filter(|f| f.starts_with("type_attr("))
        .map(|f| f.split('"').nth(1).unwrap())
        .collect();
    assert_eq!(
        types.into_iter().collect::<Vec<_>>(),
        ["k8s.apps.v1.deployment", "k8s.deployment"]
    );
    assert_eq!(
        count(&some, "type_provider("),
        count(&all, "type_provider(")
    );

    // Served from the cache: a row added there shows.
    let mut c: Json = serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
    c["facts"].as_array_mut().unwrap().push(json!([
        "type_attr",
        "k8s.core.v1.namespace",
        "cached.probe",
        "string",
        []
    ]));
    std::fs::write(&cache, c.to_string()).unwrap();
    let probe = "type_attr(\"k8s.core.v1.namespace\", \"cached.probe\"";
    let ns = schema_of(
        &mut offline_provider(&s, "st"),
        Some(&["k8s.core.v1.namespace"]),
    );
    assert_eq!(count(&ns, probe), 1, "answered from the cache");

    // A cache keyed by another document is derived again, and rewritten.
    c["key"] = json!("another");
    std::fs::write(&cache, c.to_string()).unwrap();
    let ns = schema_of(
        &mut offline_provider(&s, "st"),
        Some(&["k8s.core.v1.namespace"]),
    );
    assert_eq!(count(&ns, probe), 0);
    assert!(!std::fs::read_to_string(&cache).unwrap().contains("another"));
}
