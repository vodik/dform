//! The Kubernetes provider (`dform-provider-k8s`) without a cluster: offline
//! (the schema from the checked-in OpenAPI snapshot, Plan local), and
//! against a fake API server that speaks enough of the Kubernetes API
//! (tests/k8s_api.rs) for the conformance suite and a whole apply.

use crate::common;
use crate::k8s_api::{Api, kubeconfig, owns};
use common::{Run, Scratch, repo};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

fn k8s() -> String {
    common::exe("dform-provider-k8s")
}

/// `dform ARGS` in the scratch directory with no cluster in reach but the
/// one `kubeconfig` names, which dform.toml grants the provider at
/// `providers/k8s` (none: offline).
fn dform<S: AsRef<std::ffi::OsStr>>(s: &Scratch, kubeconfig: Option<&str>, args: &[S]) -> Run {
    if kubeconfig.is_some() {
        grant(s);
    }
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT");
    match kubeconfig {
        Some(k) => c.env("KUBECONFIG", k).env_remove("DFORM_K8S_OFFLINE"),
        None => c.env("DFORM_K8S_OFFLINE", "1"),
    };
    Run::from(c.output().unwrap())
}

/// dform.toml grants the provider at `providers/k8s` (the real one) the
/// environment's kubeconfig.
fn grant(s: &Scratch) {
    if !s.path("providers/k8s/dform-provider-k8s").exists() {
        std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
        std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    }
    let toml = s.read("dform.toml");
    if !toml.contains("[providers.k8s]") {
        s.write(
            "dform.toml",
            &format!(
                "{toml}\n[providers.k8s]\nsource = \"providers/k8s\"\n\
                 credentials = [\"kubeconfig:environment\"]\n"
            ),
        );
    }
}

/// The demo with its provider's `use` pointed at the real provider:
/// `providers/k8s/` beside it holds the executable.
fn real_demo(s: &Scratch) {
    let src =
        std::fs::read_to_string(repo().join("tests/fixtures/k8s/stacks/k8s_demo.df")).unwrap();
    let real = src.replace("use k8s", "use k8s { source = \"./providers/k8s\" }");
    assert_ne!(src, real, "the demo names its provider `provider k8s.`");
    s.write("k8s_demo.df", &real);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
}

fn mock_demo(s: &Scratch) {
    s.write(
        "k8s_demo.df",
        &std::fs::read_to_string(repo().join("tests/fixtures/k8s/stacks/k8s_demo.df")).unwrap(),
    );
}

/// The plan file's parts that come from the program and the providers.
fn planned(s: &Scratch) -> Json {
    let p: Json = serde_json::from_str(&s.read("plan.json")).unwrap();
    json!({"deformations": p["deformations"], "nulls": p["nulls"],
           "pending_groups": p["pending_groups"], "ticks": p["ticks"]})
}

/// Acceptance: tests/fixtures/k8s/stacks/k8s_demo.df plans against the mock unchanged, and
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
            .contains("configMapRef = { name = web_config.metadata.name }"),
        "{}",
        r.stdout
    );
    // The long names are the same types.
    real.write(
        "long.df",
        "\nuse k8s { source = \"./providers/k8s\" }\nresource k8s.apps.v1.deployment api {\n  metadata.name = \"api\"\n  spec.selector.matchLabels = {app = \"api\"}\n  spec.template.spec.containers = [{name = \"api\", image = \"api:1\"}]\n}\n",
    );
    let r = dform(&real, None, &["plan", "long.df"]).success();
    assert!(
        r.stdout.contains("+ k8s.apps.v1.deployment api")
            && r.stdout.contains(
                "spec.template.spec.containers[name=api] = { name = \"api\", image = \"api:1\" }"
            ),
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
        // A result set past five rows ends `(N rows)`.
        let last = r.stdout.lines().last().unwrap_or_default();
        last.trim_start_matches('(')
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .expect(&r.stdout)
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
        &format!("{src}\ncompletes(t) where type_attr(t, \"spec.completions\", _, _)\n"),
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
        "\nuse k8s { source = \"./providers/k8s\" }\nresource k8s.secret token {\n  metadata.name = \"token\"\n  stringData = {password = \"hunter2\"}\n}\n",
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
        "\nuse k8s { source = \"./providers/k8s\" }\nresource k8s.deployment api {\n  metadata.name = \"api\"\n  spec.template.spec.containers = [{name = \"api\", image = \"api:1\"}]\n}\n",
    );
    let r = dform(&s, None, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("error  p.df:3, k8s.deployment api: spec.selector is unset (required: "),
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

/// With no kubeconfig and no cluster, the provider granted the
/// environment's says so in one sentence of dform's (the host's, which
/// looked for it), and again under `DFORM_LOG=debug`.
#[test]
fn offline_is_said_in_one_sentence() {
    let s = Scratch::project("k8s-offline-line");
    real_demo(&s);
    grant(&s);
    let run = |debug: bool| {
        let mut c = common::dform();
        c.args(["plan", "k8s_demo.df"])
            .current_dir(&s.dir)
            .env("HOME", &s.dir)
            .env_remove("KUBECONFIG")
            .env_remove("DFORM_K8S_OFFLINE")
            .env_remove("KUBERNETES_SERVICE_HOST")
            .env_remove("KUBERNETES_SERVICE_PORT");
        match debug {
            true => c.env("DFORM_LOG", "debug"),
            false => c.env_remove("DFORM_LOG"),
        };
        Run::from(c.output().unwrap()).success()
    };
    let r = run(false);
    let said: Vec<&str> = r.stderr.lines().filter(|l| l.contains("k8s")).collect();
    assert_eq!(
        said,
        [
            "k8s: offline, provider k8s: the credential kubeconfig:environment: no kubeconfig \
             at ~/.kube/config and not in a cluster; planning against the snapshot schema \
             (crates/dform-k8s/openapi-snapshot.json)"
        ],
        "{}",
        r.stderr
    );
    let r = run(true);
    assert!(
        r.stderr.contains(
            "DEBUG k8s: provider k8s: the credential kubeconfig:environment: no kubeconfig"
        ),
        "{}",
        r.stderr
    );
}

/// The environment's kubeconfig reaches the provider only by dform.toml's
/// grant: without it the provider is offline, saying which grant it
/// lacks, and the cluster the kubeconfig names hears nothing; with it the
/// plan reaches the cluster.
#[test]
fn the_environments_kubeconfig_is_the_providers_only_by_a_grant() {
    let s = Scratch::project("k8s-env-grant");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let plan = || {
        let mut c = common::dform();
        c.args(["plan", "k8s_demo.df"])
            .current_dir(&s.dir)
            .env("KUBECONFIG", &kc)
            .env_remove("DFORM_K8S_OFFLINE")
            .env_remove("KUBERNETES_SERVICE_HOST");
        Run::from(c.output().unwrap()).success()
    };
    let r = plan();
    assert!(
        r.stderr.contains(
            "k8s: offline, provider k8s is not granted the credential kubeconfig:environment \
             (the environment's kubeconfig): add it to [providers.k8s] credentials in \
             dform.toml;"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(*api.requests.lock().unwrap(), Vec::<String>::new());
    grant(&s);
    let r = plan();
    assert!(!r.stderr.contains("offline"), "{}", r.stderr);
    assert!(api.count("GET /openapi/v3", &[]) > 0);
}

/// `dform provider check` against the k8s binary: no cluster, the fake API
/// server standing in, the kubeconfig as dform.toml grants it. The checks
/// run on the provider's own examples.
#[test]
fn the_k8s_provider_conforms() {
    let s = Scratch::project("k8s-check");
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let r = dform(&s, Some(&kc), &["provider", "check", &k8s()]).success();
    assert!(!r.stdout.contains("FAIL"), "{}", r.stdout);
    for line in [
        // An SDK provider: it declares the host's interfaces it uses.
        "host  native: imports: host http, secrets",
        // Checked with what dform.toml grants the provider at that path.
        "host  native: credentials: kubeconfig:environment",
        "ok    Schema serves its own types, with examples",
        "ok    Schema's types are named under the provider's name, k8s",
        "ok    Plan refuses a document without a required attribute",
        "ok    Plan spells a keyed list by key",
        "ok    Plan marks a sensitive attribute sensitive",
        "ok    Apply CREATE returns the object with its computed values",
        "ok    Apply CREATE again with the same idempotency key answers the object it made",
        "ok    Query provider.created answers what an idempotency key made",
        "ok    Query provider.created answers nothing for a key that made nothing",
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
    assert_eq!(
        api.objects.lock().unwrap().keys().collect::<Vec<_>>(),
        ["/api/v1/namespaces/default"],
        "the check cleans up"
    );
}

/// Every request the provider makes goes through the host: an `io.call`
/// span under the `provider.call` it is made for (Configure's discovery,
/// Plan's dry runs), paced in a bucket of its verb and resource, its
/// server the kubeconfig's.
#[test]
fn its_requests_are_spans_under_the_calls_they_are_made_for() {
    let s = Scratch::project("k8s-trace");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let args = common::on("k8s_demo.df", &[], &["plan", "--trace", "run.jsonl"]);
    dform(&s, Some(&kc), &args).success();
    let spans = common::spans(&s.path("run.jsonl"));
    let by_id: BTreeMap<&str, &Json> = spans
        .iter()
        .map(|sp| (sp["spanId"].as_str().unwrap(), sp))
        .collect();
    let requests: Vec<&Json> = spans
        .iter()
        .filter(|sp| common::attr(sp, "kind").as_deref() == Some("http"))
        .collect();
    let sent = api.requests.lock().unwrap().len();
    assert_eq!(
        requests.len(),
        sent,
        "a span for each of the server's requests"
    );
    let (mut under, mut buckets) = (BTreeMap::new(), std::collections::BTreeSet::new());
    for r in &requests {
        buckets.extend(common::attr(r, "bucket"));
        let addr = common::attr(r, "addr").unwrap();
        assert!(addr.starts_with(&url), "{addr}");
        let parent = r["parentSpanId"].as_str().and_then(|p| by_id.get(p));
        let call = parent.and_then(|p| p["name"].as_str()).unwrap_or_default();
        assert!(call.starts_with("k8s "), "{addr} is under {call:?}");
        *under.entry(call.to_string()).or_insert(0) += 1;
    }
    assert!(under.contains_key("k8s Configure"), "{under:?}");
    assert!(under.contains_key("k8s Plan"), "{under:?}");
    for b in ["get openapi", "patch namespaces", "patch services"] {
        assert!(buckets.contains(b), "{buckets:?}");
    }
}

/// A Configure with no document cached asks the index, then every
/// group-version document it names at once (the 27 of a k3s cluster
/// were asked one after another): the host's window paces them, so the
/// server answers several together and the trace's requests for them
/// overlap, each under the Configure.
#[test]
fn a_cold_configure_asks_for_every_document_at_once() {
    let s = Scratch::project("k8s-documents");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let args = common::on("k8s_demo.df", &[], &["plan", "--trace", "run.jsonl"]);
    dform(&s, Some(&kc), &args).success();
    let asked = api.count("GET /openapi/v3/", &[]);
    assert!(asked > 2, "{asked} documents");
    assert!(
        api.most_documents_at_once() >= 2,
        "the server answered {} of {asked} documents at once",
        api.most_documents_at_once()
    );
    let spans = common::spans(&s.path("run.jsonl"));
    let documents: Vec<&Json> = spans
        .iter()
        .filter(|sp| common::attr(sp, "addr").is_some_and(|a| a.contains("/openapi/v3/")))
        .collect();
    assert_eq!(documents.len(), asked, "a span each");
    assert!(
        most_at_once(&documents) >= 2,
        "the trace's requests for the documents overlap"
    );
}

/// The most of `spans` open at once.
fn most_at_once(spans: &[&Json]) -> usize {
    let at = |sp: &Json, k: &str| sp[k].as_str().unwrap().parse::<u128>().unwrap();
    let mut edges: Vec<(u128, i32)> = spans
        .iter()
        .flat_map(|sp| {
            [
                (at(sp, "startTimeUnixNano"), 1),
                (at(sp, "endTimeUnixNano"), -1),
            ]
        })
        .collect();
    // An end before a start at the same instant: touching is not open.
    edges.sort();
    let (mut open, mut most) = (0, 0);
    for (_, d) in edges {
        open += d;
        most = most.max(open);
    }
    most as usize
}

/// The cluster's OpenAPI documents, and the schema derived from them,
/// cached in dform.state/cache/ (`plan` plans again): a plan fetches no
/// document the index names at the same URL again, whatever its other
/// entries say (k3s answers new hashes for `apis` and `version` at every
/// ask), and only the one whose URL changed.
fn the_documents_are_cached(s: &Scratch, api: &Api, plan: &dyn Fn()) {
    let cache = s.path("dform.state/cache/k8s-openapi.json");
    assert!(cache.exists(), "the schema is cached in dform.state/cache/");
    assert!(
        s.path("dform.state/cache/k8s-schema.json").exists(),
        "and the schema derived from it"
    );
    let fetched = api.count("GET /openapi/v3/", &[]);
    plan();
    assert_eq!(
        api.count("GET /openapi/v3/", &[]),
        fetched,
        "a cached schema whose documents the index names again is not fetched again, \
         whatever the index's other entries say"
    );
    // One group-version's document changed: it alone is fetched again.
    *api.changed.lock().unwrap() = Some("apis/apps/v1".into());
    plan();
    let again: Vec<String> = api.requests.lock().unwrap()[..]
        .iter()
        .filter(|r| r.starts_with("GET /openapi/v3/"))
        .skip(fetched)
        .cloned()
        .collect();
    assert_eq!(again, ["GET /openapi/v3/apis/apps/v1?hash=4567"]);
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
        // The Create's idempotency key rides on an annotation, and the
        // deployment on a label: not configuration.
        let a = m["annotations"].as_object_mut().unwrap();
        assert!(a.remove("dform.io/idempotency-key").is_some(), "{a:?}");
        if a.is_empty() {
            m.remove("annotations");
        }
        let l = m["labels"].as_object_mut().unwrap();
        assert_eq!(l.remove("dform.io/stack"), Some(json!("k8s_demo")), "{l:?}");
        if l.is_empty() {
            m.remove("labels");
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
    assert_eq!(r.summary(), "stack k8s_demo is up to date", "{}", r.stdout);
    the_documents_are_cached(&s, &api, &|| {
        run(&["plan"]).success();
    });

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
            "spec.template.spec.containers[name=web].image = \"nginx:1.27\" → \"nginx:1.28\""
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
             data = {{ \"WEB\" = k8s.service[\"web\"].spec.clusterIP }}\n}}\n",
            s.read("k8s_demo.df")
        ),
    );
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains("data.WEB = web.spec.clusterIP"),
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
    assert_eq!(r.summary(), "stack k8s_demo is up to date", "{}", r.stdout);
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
        &s.read("k8s_demo.df").replace("port = 80,", "port = 8080,"),
    );
    let r = run(&["plan"]).success();
    assert!(r.stdout.contains("~ k8s.service web"), "{}", r.stdout);
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
    assert_eq!(r.summary(), "stack k8s_demo is up to date", "{}", r.stdout);

    // The generated ConfigMap name is the server's too, so the document
    // has none; Plan's dry run names the object by its remote id.
    s.write(
        "k8s_demo.df",
        &s.read("k8s_demo.df")
            .replace("\"LOG_LEVEL\" = \"info\"", "\"LOG_LEVEL\" = \"debug\""),
    );
    let dry = api.count(
        "PATCH /api/v1/namespaces/shop/configmaps/web-config-",
        &["dryRun=All"],
    );
    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains("data.LOG_LEVEL = \"info\" → \"debug\""),
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
    assert_eq!(r.summary(), "stack k8s_demo is up to date", "{}", r.stdout);
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
    assert!(r.stdout.contains("  ~ k8s.deployment web"), "{}", r.stdout);
    let r = run(&["apply"]).failure();
    assert!(
        r.stderr.contains(
            "! apply k8s.deployment web: refused, nothing changed\n    server-side apply"
        ) && r
            .stderr
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
            "{}\nwant(t, \"batch\") where k = \"job\", t = \"k8s.batch.v1.${{k}}\"\n",
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
            "attr(k8s.batch.v1.job, \"batch\", \"metadata\", V)",
            "k8s_demo.df",
        ],
    )
    .success();
    assert!(
        r.stdout
            .contains("uid = ?k8s.batch.v1.job batch.metadata.uid"),
        "{}",
        r.stdout
    );
}

/// The provider, offline, as a plugin connection, and its session
/// configured with its state directory `dir`.
fn offline_provider(
    s: &Scratch,
    dir: &str,
) -> (dform::plugin::link::Link, dform::plugin::backend::Session) {
    let env = dform_grpc::spawn::Env::default()
        .unset("KUBECONFIG")
        .set("DFORM_K8S_OFFLINE", "1");
    let mut conn =
        dform_grpc::client::Conn::link(&dform_grpc::spawn::Program::exe(k8s()), &env).unwrap();
    let config = json!({"world": s.path(&format!("{dir}/world.json")).display().to_string()});
    let r = conn
        .configure(
            dform::plugin::backend::Session::NONE,
            dform::plugin::wire::doc(&config),
        )
        .unwrap();
    (conn, dform::plugin::backend::Session(r.session))
}

/// A Schema answer's facts, spelled.
fn schema_of(
    (conn, session): &mut (dform::plugin::link::Link, dform::plugin::backend::Session),
    types: Option<&[&str]>,
) -> Vec<String> {
    use dform::ast::Atom;
    use dform::plugin::pb;
    let req = pb::SchemaRequest {
        types: types.map(|t| pb::TypeFilter {
            names: t.iter().map(|s| s.to_string()).collect(),
        }),
        ..Default::default()
    };
    let resp: pb::SchemaResponse = conn.call(*session, req).unwrap();
    resp.facts
        .iter()
        .map(|f| dform::spell::atom(&Atom::try_from(f).unwrap()))
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

    // Served from the cache: a row added there shows. The cache is a line
    // naming the bytes of each type's rows, then the rows (After R-123).
    let (mut head, mut groups) = read_cache(&cache);
    let rows = &mut groups
        .iter_mut()
        .find(|(t, eager, _)| t == "k8s.core.v1.namespace" && !eager)
        .unwrap()
        .2;
    rows.push(json!([
        "type_attr",
        "k8s.core.v1.namespace",
        "cached.probe",
        "string",
        []
    ]));
    write_cache(&cache, &mut head, &groups);
    let probe = "type_attr(\"k8s.core.v1.namespace\", \"cached.probe\"";
    let ns = schema_of(
        &mut offline_provider(&s, "st"),
        Some(&["k8s.core.v1.namespace"]),
    );
    assert_eq!(count(&ns, probe), 1, "answered from the cache");

    // A cache keyed by another document is derived again, and rewritten.
    head["key"] = json!("another");
    write_cache(&cache, &mut head, &groups);
    let ns = schema_of(
        &mut offline_provider(&s, "st"),
        Some(&["k8s.core.v1.namespace"]),
    );
    assert_eq!(count(&ns, probe), 0);
    let (head, _) = read_cache(&cache);
    assert_ne!(head["key"], json!("another"));
}

/// The k8s schema cache's head and its groups of rows: (type, read with
/// the kinds, rows).
fn read_cache(path: &std::path::Path) -> (Json, Vec<(String, bool, Vec<Json>)>) {
    let text = std::fs::read_to_string(path).unwrap();
    let (head, body) = text.split_once('\n').unwrap();
    let head: Json = serde_json::from_str(head).unwrap();
    let groups = head["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let at = |i: usize| e[i].as_u64().unwrap() as usize;
            let rows: Vec<Json> = serde_json::from_str(&body[at(1)..at(2)]).unwrap();
            (
                e[0].as_str().unwrap().to_string(),
                e[3] == json!(true),
                rows,
            )
        })
        .collect();
    (head, groups)
}

/// Write `groups` under `head` as the k8s schema cache.
fn write_cache(path: &std::path::Path, head: &mut Json, groups: &[(String, bool, Vec<Json>)]) {
    let mut body = String::new();
    let mut types = Vec::new();
    for (t, eager, rows) in groups {
        let from = body.len();
        body.push_str(&serde_json::to_string(rows).unwrap());
        types.push(json!([t, from, body.len(), eager]));
    }
    head["types"] = json!(types);
    std::fs::write(path, format!("{head}\n{body}")).unwrap();
}

/// Every file under `dir`, recursively.
fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// `provider_config("k8s", {kubeconfig: K})` with K a secret input:
/// the provider is configured from the text, in memory, and talks to the
/// cluster it names with its token; the environment's kubeconfig is not
/// read. No byte of the token is in state, the plan file, the audit log,
/// the schema cache or the output. Host and token as separate fields work
/// the same.
#[test]
fn a_kubeconfig_held_as_a_secret_configures_the_provider() {
    let s = Scratch::project("k8s-kubeconfig-secret");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    let (api, url) = Api::start();
    const TOKEN: &str = "tok-5ecret-9f2a71c4";
    let kc = json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "fake",
        "clusters": [{"name": "fake", "cluster": {"server": url}}],
        "contexts": [{"name": "fake", "context": {"cluster": "fake", "user": "fake",
                                                  "namespace": "default"}}],
        "users": [{"name": "fake", "user": {"token": TOKEN}}]
    })
    .to_string();
    let program = |settings: &str| {
        format!(
            "\ninput kubeconfig: secret(string)\n\
             use k8s {{ source = \"./providers/k8s\" }}\n\
             provider_config(\"k8s\", {settings}) where kubeconfig(k)\n\
             resource k8s.config_map settings {{\n  metadata.name = \"settings\"\n  \
             data = {{ \"MODE\" = \"test\" }}\n}}\n"
        )
    };
    s.write("p.df", &program("{ kubeconfig = k }"));
    // The environment names no cluster dform may use.
    let env_kc = s.path("no-such-kubeconfig").display().to_string();
    let set = format!("kubeconfig={kc}");
    let run = |args: &[&str]| {
        let mut all = common::on("p.df", &[], args);
        all.extend(["--set".to_string(), set.clone()]);
        dform(&s, Some(&env_kc), &all)
    };
    let mut out = String::new();
    let r = run(&["plan", "--out", "plan.json"]).success();
    out += &(r.stdout + &r.stderr);
    let r = run(&["apply"]).success();
    out += &(r.stdout + &r.stderr);
    assert!(
        api.get("/api/v1/namespaces/default/configmaps/settings")
            .is_some(),
        "{out}"
    );
    assert!(
        api.auth
            .lock()
            .unwrap()
            .iter()
            .any(|a| a == &format!("Bearer {TOKEN}")),
        "the token from the program authenticates"
    );
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    out += &(r.stdout + &r.stderr);
    assert!(!out.contains(TOKEN), "{out}");
    let mut files = Vec::new();
    files_under(&s.dir, &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("state.audit.jsonl")),
        "{files:?}"
    );
    assert!(files.iter().any(|f| f.ends_with("plan.json")));
    for f in files.iter().filter(|f| !f.ends_with("p.df")) {
        let bytes = std::fs::read(f).unwrap();
        assert!(
            !bytes.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
            "{} holds the token",
            f.display()
        );
    }

    // Host and token as separate fields.
    let (api, url) = Api::start();
    s.write(
        "p.df",
        &program(&format!("{{ host = \"{url}\", token = k }}")),
    );
    let set = format!("kubeconfig={TOKEN}");
    let mut args = common::on("p.df", &[], &["apply"]);
    args.extend(["--set".to_string(), set]);
    let r = dform(&s, Some(&env_kc), &args).success();
    assert!(!(r.stdout + &r.stderr).contains(TOKEN));
    assert!(
        api.get("/api/v1/namespaces/default/configmaps/settings")
            .is_some()
    );
    assert!(
        api.auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == &format!("Bearer {TOKEN}"))
    );
}

/// A kubeconfig the program gives with a client certificate (k3s's, a
/// managed cluster's): the host presents it to the API server, over one
/// TLS connection for the whole run, its CA trusted beside the machine's;
/// the provider never sees the key.
#[test]
fn the_host_presents_the_programs_client_certificate() {
    use base64::Engine;
    use dform::http::fake::{CA, CLIENT_CHAIN, CLIENT_KEY, Server};
    let s = Scratch::project("k8s-client-cert");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    let server = Server::h2_mutual(100);
    let (presented, shown) = (Server::client_der(), Arc::new(Mutex::new(Vec::new())));
    let seen = shown.clone();
    server.answer(move |r| {
        seen.lock()
            .unwrap()
            .push(r.peer.as_ref() == Some(&presented));
        let index = json!({"paths": {}});
        (
            200,
            vec![("content-type".into(), "application/json".into())],
            index.to_string().into_bytes(),
        )
    });
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let kc = json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "k3s",
        "clusters": [{"name": "k3s", "cluster": {"server": server.url(""),
                                                 "certificate-authority-data": b64(CA)}}],
        "contexts": [{"name": "k3s", "context": {"cluster": "k3s", "user": "admin"}}],
        "users": [{"name": "admin", "user": {"client-certificate-data": b64(CLIENT_CHAIN),
                                             "client-key-data": b64(CLIENT_KEY)}}]
    })
    .to_string();
    s.write(
        "p.df",
        "\ninput kubeconfig: secret(string)\n\
         use k8s { source = \"./providers/k8s\" }\n\
         provider_config(\"k8s\", { kubeconfig = k }) where kubeconfig(k)\n\
         resource k8s.namespace shop { metadata.name = \"shop\" }\n",
    );
    let mut args = common::on("p.df", &[], &["plan"]);
    args.extend(["--set".to_string(), format!("kubeconfig={kc}")]);
    dform(&s, None, &args);
    let shown = shown.lock().unwrap();
    assert!(!shown.is_empty(), "the provider asked the cluster");
    assert!(
        shown.iter().all(|p| *p),
        "each request presented the certificate"
    );
    assert_eq!(server.connections(), 1, "one connection for the run");
}

/// A Create whose answer is lost (the server applied it and the
/// connection closed) made an object with a generated name, which the
/// next run cannot know. It finds it by the deployment's label and the
/// Create's idempotency-key annotation (`provider.created`), maps it, and,
/// the program having dropped the resource, deletes it: nothing is left in
/// the cluster.
#[test]
fn a_create_whose_answer_was_lost_is_found_by_its_label_and_key() {
    let s = Scratch::project("k8s-lost-create");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    let head = "\nuse k8s { source = \"./providers/k8s\" }\n";
    let cm = "resource k8s.config_map settings {\n  metadata.generateName = \"settings-\"\n  \
              data = { \"MODE\" = \"test\" }\n}\n";
    s.write("p.df", &format!("{head}{cm}"));
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("p.df", &[], args));
    let configmaps = || -> Vec<(String, Json)> {
        api.objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.starts_with("/api/v1/namespaces/default/configmaps/"))
            .map(|(p, o)| (p.clone(), o.clone()))
            .collect()
    };
    *api.lose_answers.lock().unwrap() = 1;
    let r = run(&["apply"]).failure();
    let made = configmaps();
    assert_eq!(made.len(), 1, "{}\n{}", r.stdout, r.stderr);
    let (path, obj) = &made[0];
    assert_eq!(obj["metadata"]["labels"]["dform.io/stack"], "p");
    assert!(
        obj["metadata"]["annotations"]["dform.io/idempotency-key"]
            .as_str()
            .is_some_and(|k| k.starts_with("dform-")),
        "{obj}"
    );

    // The program drops the resource before the next run.
    s.write("p.df", head);
    let lists = api.count(
        "GET /api/v1/configmaps?",
        &["labelSelector=dform.io%2Fstack%3Dp"],
    );
    let r = run(&["apply"]).success();
    assert!(
        api.count(
            "GET /api/v1/configmaps?",
            &["labelSelector=dform.io%2Fstack%3Dp"]
        ) > lists,
        "found by a label selector"
    );
    let name = path.rsplit('/').next().unwrap();
    assert!(
        r.stderr.contains(&format!(
            "k8s.config_map settings: the create whose answer was lost made default/{name}"
        )),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("- k8s.config_map settings"),
        "{}",
        r.stdout
    );
    assert!(configmaps().is_empty(), "nothing is left in the cluster");
}

/// `spec.podSelector = {}` (every pod) is a value, not absent: the path is
/// an object with no required field (`Schema::empty_is_present`). Added to
/// a NetworkPolicy that has none, it is a change the plan shows and the
/// apply sends; then nothing is left to do.
#[test]
fn an_empty_pod_selector_is_present() {
    let s = Scratch::project("k8s-empty-object");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    let program = |selector: &str| {
        format!(
            "\nuse k8s {{ source = \"./providers/k8s\" }}\n\
             resource k8s.network_policy deny {{\n  metadata.name = \"deny\"\n{selector}  \
             spec.policyTypes = [\"Ingress\"]\n}}\n"
        )
    };
    s.write("p.df", &program(""));
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("p.df", &[], args));
    run(&["apply"]).success();
    let path = "/apis/networking.k8s.io/v1/namespaces/default/networkpolicies/deny";
    assert!(api.get(path).unwrap()["spec"].get("podSelector").is_none());

    s.write("p.df", &program("  spec.podSelector = {}\n"));
    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains("~ k8s.network_policy deny"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("spec.podSelector = <none> → {}"),
        "{}",
        r.stdout
    );
    run(&["apply"]).success();
    assert_eq!(api.get(path).unwrap()["spec"]["podSelector"], json!({}));
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// The provider answers the inventory (`world.T[e].p`) from the cluster:
/// the objects of the kinds the program reads the world of, named by
/// remote id, whoever manages them. Only those kinds are listed.
#[test]
fn a_world_read_is_answered_from_the_live_object() {
    let s = Scratch::project("k8s-world-read");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    s.write(
        "p.df",
        "\n\
         use k8s { source = \"./providers/k8s\" }\n\
         let active = world.k8s.service[\"shop/web\"].spec.selector.color\n\
         resource k8s.config_map serving {\n\
           metadata.name = \"serving\"\n\
           data = { \"COLOR\" = active }\n\
         }\n\
         ",
    );
    let (api, url) = Api::start();
    api.objects.lock().unwrap().insert(
        "/api/v1/namespaces/shop/services/web".into(),
        json!({"apiVersion": "v1", "kind": "Service",
               "metadata": {"name": "web", "namespace": "shop", "managedFields": [
                   {"manager": "kubectl", "operation": "Update", "fieldsV1": {}}]},
               "spec": {"selector": {"app": "web", "color": "blue"}}}),
    );
    let kc = kubeconfig(&s, &url);
    let r = dform(&s, Some(&kc), &common::on("p.df", &[], &["plan"])).success();
    assert!(
        r.stdout.contains("data.COLOR = \"blue\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert!(api.count("GET /api/v1/services?", &[]) > 0);
    assert_eq!(
        api.count("GET /apis/apps/v1/deployments?", &[]),
        0,
        "a kind the program does not read is not listed"
    );
}

/// Drift: `kubectl patch` (a merge patch, another field manager) changes a
/// field dform applied. The field is that manager's now, so the next plan
/// puts dform's value back; the apply, never forced, then fails naming the
/// manager and the field.
#[test]
fn drift_from_a_kubectl_patch_is_planned_back() {
    let s = Scratch::project("k8s-drift");
    real_demo(&s);
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let run = |args: &[&str]| dform(&s, Some(&kc), &common::on("k8s_demo.df", &[], args));
    run(&["apply"]).success();
    // What `kubectl patch deployment web -p '{"spec":{"replicas":5}}'` sends.
    let body = serde_json::to_vec(&json!({"spec": {"replicas": 5}})).unwrap();
    let head = format!(
        "PATCH /apis/apps/v1/namespaces/shop/deployments/web?fieldManager=kubectl-patch HTTP/1.1\r\n\
         Host: x\r\nContent-Type: application/merge-patch+json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut conn = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
    conn.write_all(head.as_bytes()).unwrap();
    conn.write_all(&body).unwrap();
    let mut status_line = String::new();
    BufReader::new(conn).read_line(&mut status_line).unwrap();
    assert!(status_line.contains(" 200 "), "{status_line}");
    assert_eq!(
        managers_of(
            &api.get("/apis/apps/v1/namespaces/shop/deployments/web")
                .unwrap(),
            ".spec.replicas"
        ),
        ["kubectl-patch"]
    );

    let r = run(&["plan"]).success();
    assert!(
        r.stdout.contains("  ~ k8s.deployment web")
            && r.stdout.contains("spec.replicas = <none> → 3"),
        "{}",
        r.stdout
    );
    let r = run(&["apply"]).failure();
    assert!(
        r.stderr
            .contains(".spec.replicas is owned by field manager \"kubectl-patch\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// A secret output of one stack, a Secret's `stringData` key, read into a
/// Secret of another: the provider reads it from the cluster where it is
/// held, inside the Apply (E DR-19), and the object reads back as the
/// reference (`dform.io/held`), so the next plan is up to date; the bytes
/// are never in dform's output or files.
#[test]
fn a_held_secret_is_read_from_the_cluster() {
    const PW: &str = "k8s-held-5ecret-31c9";
    let s = Scratch::project("k8s-held");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    s.write(
        "a.df",
        "\n\
         input pw: secret(string)\n\
         use k8s { source = \"./providers/k8s\" }\n\
         resource k8s.secret creds {\n\
           metadata.name = \"creds\"\n\
           stringData = { pw = p }\n\
         } where pw(p)\n\
         output pw: secret(string) = creds.stringData.pw\n\
         ",
    );
    s.write(
        "b.df",
        "\n\
         use k8s { source = \"./providers/k8s\" }\n\
         use a\n\
         resource k8s.secret copy {\n\
           metadata.name = \"copy\"\n\
           stringData = { pw = a.pw }\n\
         }\n\
         ",
    );
    // dform.toml names a.df a stack, which b uses.
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.a]\n",
    );
    let (api, url) = Api::start();
    let kc = kubeconfig(&s, &url);
    let set = format!("pw={PW}");
    let mut out = String::new();
    let r = dform(&s, Some(&kc), &["apply", "a.df", "--set", &set]).success();
    out += &(r.stdout + &r.stderr);
    let r = dform(&s, Some(&kc), &["apply", "b.df", "--set", &set]).success();
    out += &(r.stdout + &r.stderr);
    let copy = api
        .get("/api/v1/namespaces/default/secrets/copy")
        .expect("the copy");
    let got = copy["stringData"]["pw"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            let d = copy["data"]["pw"].as_str()?;
            Some(String::from_utf8(dform_k8s_base64(d)).unwrap())
        });
    assert_eq!(got.as_deref(), Some(PW), "{copy}");
    // A plan of b plans a first (R-200), with its input.
    let r = dform(&s, Some(&kc), &["plan", "b.df", "--set", &set]).success();
    assert_eq!(r.summary(), "plan: 0 changes", "{}", r.stdout);
    out += &(r.stdout + &r.stderr);
    assert!(!out.contains(PW), "{out}");
    let mut files = Vec::new();
    files_under(&s.path("dform.state"), &mut files);
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        assert!(
            !bytes.windows(PW.len()).any(|w| w == PW.as_bytes()),
            "{} holds the secret",
            f.display()
        );
    }
}

fn dform_k8s_base64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            _ => 63,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
