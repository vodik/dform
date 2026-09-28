//! The Kubernetes provider against a real cluster. Runs only when
//! `DFORM_K8S_TEST_KUBECONFIG` names a kubeconfig (a kind cluster will do);
//! otherwise each test says so and passes. Everything happens in a fresh
//! namespace `dform-test-<random>`, deleted at the end.
//!
//!   DFORM_K8S_TEST_KUBECONFIG=~/.kube/config cargo test --test k8s_cluster

mod common;
use common::{Run, Scratch};
use serde_json::json;
use std::process::Command;

const K8S: &str = env!("CARGO_BIN_EXE_dform-provider-k8s");

/// The test cluster's kubeconfig, or `None` (and a line saying the test is
/// skipped).
fn kubeconfig(test: &str) -> Option<String> {
    match std::env::var("DFORM_K8S_TEST_KUBECONFIG") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("{test}: skipped (DFORM_K8S_TEST_KUBECONFIG is not set)");
            None
        }
    }
}

fn client(kubeconfig: &str) -> (tokio::runtime::Runtime, kube::Client) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = rt.block_on(async {
        let kc = kube::config::Kubeconfig::read_from(kubeconfig).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        kube::Client::try_from(config).unwrap()
    });
    (rt, client)
}

/// A namespace for one test, deleted when dropped.
struct Namespace {
    name: String,
    kubeconfig: String,
}

impl Namespace {
    fn new(kubeconfig: &str) -> Namespace {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        Namespace {
            name: format!("dform-test-{:x}{:x}", std::process::id(), n),
            kubeconfig: kubeconfig.to_string(),
        }
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        let (rt, client) = client(&self.kubeconfig);
        let api: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(client);
        let _ = rt.block_on(api.delete(&self.name, &kube::api::DeleteParams::background()));
    }
}

fn dform(s: &Scratch, kubeconfig: &str, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_dform"))
        .args(args)
        .current_dir(&s.dir)
        .env("KUBECONFIG", kubeconfig)
        .env_remove("DFORM_K8S_OFFLINE")
        .output()
        .unwrap();
    Run::from(out)
}

fn program(ns: &str, image: &str) -> String {
    format!(
        r#"edition 2026

provider k8s {{ source = "./providers/k8s" }}

resource k8s.namespace test {{
  metadata.name = "{ns}"
}}

resource k8s.config_map settings {{
  metadata.generateName = "settings-"
  metadata.namespace = test.metadata.name
  data = {{ "MODE": "test" }}
}}

resource k8s.deployment web {{
  metadata.name = "web"
  metadata.namespace = test.metadata.name
  spec.replicas = 1
  spec.selector.matchLabels = {{ app: "web" }}
  spec.template.metadata.labels = {{ app: "web" }}
  spec.template.spec.containers = [{{
    name: "web",
    image: "{image}",
    envFrom: [{{ configMapRef: {{ name: k8s.config_map.settings.metadata.name }} }}]
  }}]
}}
"#
    )
}

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(K8S, s.path("providers/k8s/dform-provider-k8s")).unwrap();
    s
}

/// Apply, then nothing to do; a change is a dry-run-planned update; the
/// schema came from the cluster and is cached in the state directory.
#[test]
fn applies_and_converges_on_a_cluster() {
    let Some(kc) = kubeconfig("applies_and_converges_on_a_cluster") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-apply");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    let r = dform(&s, &kc, &["--file", "p.df", "apply"]).success();
    assert!(!r.stderr.contains("offline"), "{}", r.stderr);
    let r = dform(&s, &kc, &["--file", "p.df", "plan"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
    assert!(s.path(".dform/p/k8s-openapi.json").exists());

    s.write("p.df", &program(&ns.name, "nginx:1.28"));
    let r = dform(&s, &kc, &["--file", "p.df", "plan"]).success();
    assert!(
        r.stdout.contains(
            "spec.template.spec.containers[name=web].image: \"nginx:1.27\" -> \"nginx:1.28\""
        ),
        "{}",
        r.stdout
    );
    dform(&s, &kc, &["--file", "p.df", "apply"]).success();
    let r = dform(&s, &kc, &["--file", "p.df", "plan"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

/// Another field manager takes `spec.replicas`; dform's apply, never
/// forced, fails naming the manager and the field.
#[test]
fn a_field_another_manager_owns_fails_the_apply() {
    let Some(kc) = kubeconfig("a_field_another_manager_owns_fails_the_apply") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-conflict");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    dform(&s, &kc, &["--file", "p.df", "apply"]).success();

    let (rt, client) = client(&kc);
    let api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(client, &ns.name);
    let patch = json!({"apiVersion": "apps/v1", "kind": "Deployment",
                       "metadata": {"name": "web"}, "spec": {"replicas": 2}});
    rt.block_on(api.patch(
        "web",
        &kube::api::PatchParams::apply("dform-test-other").force(),
        &kube::api::Patch::Apply(&patch),
    ))
    .unwrap();

    let r = dform(&s, &kc, &["--file", "p.df", "apply"]).failure();
    assert!(
        r.stderr.contains("apply k8s.deployment/web")
            && r.stderr
                .contains(".spec.replicas is owned by field manager \"dform-test-other\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// Removing a resource from the program deletes it (background
/// propagation).
#[test]
fn a_removed_resource_is_deleted() {
    let Some(kc) = kubeconfig("a_removed_resource_is_deleted") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-delete");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    dform(&s, &kc, &["--file", "p.df", "apply"]).success();
    let without = program(&ns.name, "nginx:1.27");
    let without = &without[..without.find("resource k8s.deployment").unwrap()];
    s.write("p.df", without);
    let r = dform(&s, &kc, &["--file", "p.df", "apply"]).success();
    assert!(r.stdout.contains("- k8s.deployment.web"), "{}", r.stdout);

    let (rt, client) = client(&kc);
    let api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(client, &ns.name);
    assert!(rt.block_on(api.get_opt("web")).unwrap().is_none());
}
