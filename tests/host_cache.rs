//! The cache the host keeps for providers (`dform:host/cache`): the
//! Kubernetes provider keeps a cluster's OpenAPI documents in it, the
//! account's from one run to the next and the run's for every deployment
//! of a run, so a tree of two deployments on one cluster asks the
//! cluster's index once a run and its documents once ever; each get and
//! put is a `cache` span under its call, its scope and whether it hit
//! its fields.

use crate::common::{self, Scratch};
use crate::k8s_api::{Api, kubeconfig};
use serde_json::Value as Json;

/// A project of two stacks, `apps` reading `platform`'s output, each a
/// ConfigMap of the cluster the environment's kubeconfig names: `plan
/// apps` is a tree of two deployments, one run. The Kubernetes provider
/// at `provider` (its executable, or its wasm provider) granted the
/// kubeconfig.
fn a_tree(name: &str, provider: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers.k8s]\npath = \"{provider}\"\n\
             allow = [\"wasi:filesystem\"]\ncredentials = [\"kubeconfig:environment\"]\n"
        ),
    );
    s.write(
        "stacks/platform.df",
        "use k8s\n\nresource k8s.config_map mode {\n  metadata = { name = \"platform\", \
         namespace = \"default\" }\n  data = { \"MODE\" = \"lab\" }\n}\n\noutput mode = \
         \"lab\"\n",
    );
    s.write(
        "stacks/apps.df",
        "use k8s\nuse stacks.platform\n\nresource k8s.config_map mode {\n  metadata = { name \
         = \"apps\", namespace = \"default\" }\n  data = { \"MODE\" = platform.mode }\n}\n",
    );
    s
}

/// `dform plan apps`, the tree of `s`, against the cluster `kc` names,
/// its cache its own (`XDG_CACHE_HOME`, the compiled components the
/// shared one's), traced to run.jsonl: its spans.
fn plan(s: &Scratch, kc: &str) -> Vec<Json> {
    let xdg = s.path("xdg");
    let compiled =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("host-wasm-cache/dform/wasm");
    std::fs::create_dir_all(&compiled).unwrap();
    std::fs::create_dir_all(xdg.join("dform")).unwrap();
    if !xdg.join("dform/wasm").exists() {
        std::os::unix::fs::symlink(&compiled, xdg.join("dform/wasm")).unwrap();
    }
    let out = common::dform()
        .args(["plan", "apps", "--trace", "run.jsonl"])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env("XDG_CACHE_HOME", &xdg)
        .env("KUBECONFIG", kc)
        .env_remove("DFORM_K8S_OFFLINE")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    common::Run::from(out).success();
    common::spans(&s.path("run.jsonl"))
}

/// The index asked, and the documents.
fn asked(api: &Api) -> (usize, usize) {
    let index = api
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| *r == "GET /openapi/v3")
        .count();
    (index, api.count("GET /openapi/v3/", &[]))
}

/// Each `cache` span's name, scope and hit, counted.
fn cached(spans: &[Json]) -> std::collections::BTreeMap<String, usize> {
    let mut out = std::collections::BTreeMap::new();
    for sp in spans
        .iter()
        .filter(|sp| common::attr(sp, "scope").is_some())
    {
        let hit = common::attr(sp, "hit").map_or(String::new(), |h| format!(" hit={h}"));
        let scope = common::attr(sp, "scope").unwrap();
        let name = sp["name"].as_str().unwrap();
        *out.entry(format!("{name} {scope}{hit}")).or_insert(0) += 1;
    }
    out
}

/// Both deployments of the run configure the provider on one cluster:
/// the second takes the run's documents (one index, each document once);
/// the next run takes the account's, asks the index once to see they
/// are current, and no document.
fn one_fetch_per_cluster_per_run(provider: &str, name: &str) {
    let (api, url) = Api::start();
    let s = a_tree(name, provider);
    let kc = kubeconfig(&s, &url);
    let spans = plan(&s, &kc);
    let (index, documents) = asked(&api);
    assert_eq!(index, 1, "{:#?}", api.requests.lock().unwrap());
    assert!(documents > 2, "{documents} documents");
    // The first deployment's Configure misses the run's and the
    // account's, fetches, and keeps the documents (both) and the schema
    // derived (the account's); the second's takes the run's documents and
    // the account's schema.
    let gets = |spans: &[Json]| {
        cached(spans)
            .into_iter()
            .map(|(k, n)| format!("{k} x{n}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        gets(&spans),
        [
            "k8s cache get account hit=false x2",
            "k8s cache get account hit=true x1",
            "k8s cache get run hit=false x1",
            "k8s cache get run hit=true x1",
            "k8s cache put account x2",
            "k8s cache put run x1",
        ]
    );
    // Each get and put is under its call.
    let by_id: std::collections::BTreeMap<&str, &Json> = spans
        .iter()
        .map(|sp| (sp["spanId"].as_str().unwrap(), sp))
        .collect();
    for sp in spans
        .iter()
        .filter(|sp| common::attr(sp, "scope").is_some())
    {
        let parent = by_id[sp["parentSpanId"].as_str().unwrap()];
        assert!(
            common::attr(parent, "provider.call").is_some(),
            "{sp} under {parent}"
        );
    }

    let spans = plan(&s, &kc);
    assert_eq!(
        asked(&api),
        (index + 1, documents),
        "the account's, current"
    );
    // The next run's first Configure takes the account's documents, which
    // the index says are current, and keeps them for the run.
    assert_eq!(
        gets(&spans),
        [
            "k8s cache get account hit=true x3",
            "k8s cache get run hit=false x1",
            "k8s cache get run hit=true x1",
            "k8s cache put run x1",
        ]
    );
}

#[test]
fn the_k8s_provider_fetches_a_clusters_documents_once_a_run() {
    one_fetch_per_cluster_per_run(&common::exe("dform-provider-k8s"), "host-cache-native");
}

#[cfg(feature = "wasm")]
#[test]
fn the_k8s_component_fetches_a_clusters_documents_once_a_run() {
    let wasm = crate::host_trace::built(
        "dform_provider_k8s.wasm",
        "cargo build -p dform-provider-k8s --release --target wasm32-wasip3 --features component",
    );
    one_fetch_per_cluster_per_run(&wasm, "host-cache-wasm");
}
