//! The Kubernetes provider against a real cluster. Runs only when
//! `DFORM_K8S_TEST_KUBECONFIG` names a kubeconfig (a kind cluster will do);
//! otherwise each test says so and passes. Everything happens in a fresh
//! namespace `dform-test-<random>`, deleted at the end.
//!
//!   kind create cluster --name dform-test --kubeconfig target/kind.kubeconfig
//!   DFORM_K8S_TEST_KUBECONFIG=target/kind.kubeconfig cargo test --test k8s_cluster

mod common;
use common::{Run, Scratch, repo};
use serde_json::{Value as Json, json};
use std::process::Command;

fn k8s() -> String {
    common::exe("dform-provider-k8s")
}

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
        .args(common::yes(args))
        .current_dir(&s.dir)
        .env("KUBECONFIG", kubeconfig)
        .env_remove("DFORM_K8S_OFFLINE")
        .output()
        .unwrap();
    Run::from(out)
}

fn program(ns: &str, image: &str) -> String {
    format!(
        r#"edition 2027

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
    envFrom: [{{ configMapRef: {{ name: settings.metadata.name }} }}]
  }}]
}}
"#
    )
}

/// A project (apply needs one) holding the real provider.
fn scratch(name: &str) -> Scratch {
    let s = Scratch::project(name);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(k8s(), s.path("providers/k8s/dform-provider-k8s")).unwrap();
    s
}

/// Apply, then nothing to do; a change is a dry-run-planned update; the
/// schema came from the cluster and is cached in dform.state/cache/.
#[test]
fn applies_and_converges_on_a_cluster() {
    let Some(kc) = kubeconfig("applies_and_converges_on_a_cluster") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-apply");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    let r = dform(&s, &kc, &["apply", "p.df"]).success();
    assert!(!r.stderr.contains("offline"), "{}", r.stderr);
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
    assert!(s.path("dform.state/cache/k8s-openapi.json").exists());

    s.write("p.df", &program(&ns.name, "nginx:1.28"));
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains(
            "spec.template.spec.containers[name=web].image: \"nginx:1.27\" -> \"nginx:1.28\""
        ),
        "{}",
        r.stdout
    );
    dform(&s, &kc, &["apply", "p.df"]).success();
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
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
    dform(&s, &kc, &["apply", "p.df"]).success();

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

    let r = dform(&s, &kc, &["apply", "p.df"]).failure();
    assert!(
        r.stderr.contains("apply k8s.deployment[\"web\"]")
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
    dform(&s, &kc, &["apply", "p.df"]).success();
    let without = program(&ns.name, "nginx:1.27");
    let without = &without[..without.find("resource k8s.deployment").unwrap()];
    s.write("p.df", without);
    let r = dform(&s, &kc, &["apply", "p.df"]).success();
    assert!(
        r.stdout.contains("- k8s.deployment[\"web\"]"),
        "{}",
        r.stdout
    );

    let (rt, client) = client(&kc);
    let api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(client, &ns.name);
    assert!(rt.block_on(api.get_opt("web")).unwrap().is_none());
}

/// examples/k8s's demo, its namespace the test's, against the real
/// provider: it applies and converges, and a new image is a dry-run-planned
/// update in place that converges too.
#[test]
fn the_k8s_demo_applies_and_converges() {
    let Some(kc) = kubeconfig("the_k8s_demo_applies_and_converges") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-demo");
    let src = std::fs::read_to_string(repo().join("examples/k8s/stacks/k8s_demo.df")).unwrap();
    let demo = src
        .replace(
            "provider k8s {}",
            "provider k8s { source = \"./providers/k8s\" }",
        )
        .replace(
            "metadata.name = \"shop\"",
            &format!("metadata.name = \"{}\"", ns.name),
        );
    assert_ne!(src, demo);
    s.write("k8s_demo.df", &demo);
    let r = dform(&s, &kc, &["apply", "k8s_demo.df"]).success();
    assert!(!r.stderr.contains("offline"), "{}", r.stderr);
    let r = dform(&s, &kc, &["plan", "k8s_demo.df"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);

    s.write("k8s_demo.df", &demo.replace("nginx:1.27", "nginx:1.28"));
    let r = dform(&s, &kc, &["plan", "k8s_demo.df"]).success();
    assert!(
        r.stdout.contains("~ k8s.deployment[\"web\"]")
            && r.stdout.contains(
                "spec.template.spec.containers[name=web].image: \"nginx:1.27\" -> \"nginx:1.28\""
            ),
        "{}",
        r.stdout
    );
    dform(&s, &kc, &["apply", "k8s_demo.df"]).success();
    let r = dform(&s, &kc, &["plan", "k8s_demo.df"]).success();
    assert_eq!(r.summary(), "stack k8s_demo is undeformed", "{}", r.stdout);
}

/// Drift: a `kubectl patch` (a merge patch as another field manager) of a
/// field dform applied is planned back; the apply, never forced, fails
/// naming that manager.
#[test]
fn drift_from_a_kubectl_patch_is_planned_back() {
    let Some(kc) = kubeconfig("drift_from_a_kubectl_patch_is_planned_back") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-drift");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    dform(&s, &kc, &["apply", "p.df"]).success();

    let (rt, client) = client(&kc);
    let api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(client, &ns.name);
    let pp = kube::api::PatchParams {
        field_manager: Some("kubectl-patch".into()),
        ..Default::default()
    };
    rt.block_on(api.patch(
        "web",
        &pp,
        &kube::api::Patch::Merge(json!({"spec": {"replicas": 3}})),
    ))
    .unwrap();

    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("~ k8s.deployment[\"web\"]") && r.stdout.contains("spec.replicas"),
        "{}",
        r.stdout
    );
    let r = dform(&s, &kc, &["apply", "p.df"]).failure();
    assert!(
        r.stderr
            .contains(".spec.replicas is owned by field manager \"kubectl-patch\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// The deployment's state file (`dform.state/<stack>/state.json`).
fn state_file(s: &Scratch) -> std::path::PathBuf {
    fn find(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(f) = find(&p) {
                    return Some(f);
                }
            } else if p.file_name().is_some_and(|n| n == "state.json") {
                return Some(p);
            }
        }
        None
    }
    find(&s.path("dform.state")).expect("a state file")
}

/// A Create whose answer was lost: state forgets the ConfigMap it made
/// (whose name the server's `generateName` picked) and records the Create
/// as uncertain with the key the object carries. The next apply finds the
/// object by the deployment's label and that key, maps it, and makes no
/// second one.
#[test]
fn a_create_whose_answer_was_lost_is_found_not_made_again() {
    let Some(kc) = kubeconfig("a_create_whose_answer_was_lost_is_found_not_made_again") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-lost-create");
    s.write("p.df", &program(&ns.name, "nginx:1.27"));
    dform(&s, &kc, &["apply", "p.df"]).success();

    let (rt, client) = client(&kc);
    let api: kube::Api<k8s_openapi::api::core::v1::ConfigMap> =
        kube::Api::namespaced(client, &ns.name);
    let settings = || -> Vec<k8s_openapi::api::core::v1::ConfigMap> {
        rt.block_on(api.list(&Default::default()))
            .unwrap()
            .items
            .into_iter()
            .filter(|c| {
                c.metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("settings-"))
            })
            .collect()
    };
    let made = settings();
    assert_eq!(made.len(), 1);
    let name = made[0].metadata.name.clone().unwrap();
    let key = made[0]
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("dform.io/idempotency-key"))
        .cloned()
        .expect("the Create's key");
    assert_eq!(
        made[0]
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("dform.io/stack"))
            .map(String::as_str),
        Some("p")
    );

    let path = state_file(&s);
    let mut st: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let addr = "k8s.config_map::settings";
    assert!(
        st["resources"]
            .as_object_mut()
            .unwrap()
            .remove(addr)
            .is_some(),
        "{st}"
    );
    st["uncertain"] = json!({ addr: {"op": "create", "key": key} });
    std::fs::write(&path, serde_json::to_string_pretty(&st).unwrap()).unwrap();

    let r = dform(&s, &kc, &["apply", "p.df"]).success();
    assert!(
        r.stderr.contains(&format!(
            "k8s.config_map[\"settings\"]: the create whose answer was lost made {}/{name}",
            ns.name
        )),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(settings().len(), 1, "no second ConfigMap");
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

/// A Secret's `stringData` reads back from the `data` the server folded it
/// into: after the apply, nothing to do; a new value is an update.
#[test]
fn a_secrets_string_data_reads_back() {
    let Some(kc) = kubeconfig("a_secrets_string_data_reads_back") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-secret");
    let program = |pw: &str| {
        format!(
            "edition 2027\nprovider k8s {{ source = \"./providers/k8s\" }}\n\
             resource k8s.namespace test {{\n  metadata.name = \"{}\"\n}}\n\
             resource k8s.secret token {{\n  metadata.name = \"token\"\n  \
             metadata.namespace = test.metadata.name\n  stringData = {{ password: \"{pw}\" }}\n}}\n",
            ns.name
        )
    };
    s.write("p.df", &program("hunter2"));
    dform(&s, &kc, &["apply", "p.df"]).success();
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
    s.write("p.df", &program("hunter3"));
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("~ k8s.secret[\"token\"]") && !r.stdout.contains("hunter"),
        "{}",
        r.stdout
    );
}

/// `spec.podSelector = {}` (every pod) is present and empty: a policy
/// with it and one without both converge, though the API server defaults
/// the missing selector to `{}`; adding it to the one without is a change.
#[test]
fn an_empty_pod_selector_converges() {
    let Some(kc) = kubeconfig("an_empty_pod_selector_converges") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-pod-selector");
    let program = |selector: &str| {
        format!(
            "edition 2027\nprovider k8s {{ source = \"./providers/k8s\" }}\n\
             resource k8s.namespace test {{\n  metadata.name = \"{}\"\n}}\n\
             resource k8s.network_policy all {{\n  metadata.name = \"all\"\n  \
             metadata.namespace = test.metadata.name\n  spec.podSelector = {{}}\n  \
             spec.policyTypes = [\"Ingress\"]\n}}\n\
             resource k8s.network_policy other {{\n  metadata.name = \"other\"\n  \
             metadata.namespace = test.metadata.name\n{selector}  \
             spec.policyTypes = [\"Ingress\"]\n}}\n",
            ns.name
        )
    };
    s.write("p.df", &program(""));
    dform(&s, &kc, &["apply", "p.df"]).success();
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
    s.write("p.df", &program("  set spec.podSelector = {}\n"));
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("~ k8s.network_policy[\"other\"]")
            && !r.stdout.contains("k8s.network_policy[\"all\"]"),
        "{}",
        r.stdout
    );
    dform(&s, &kc, &["apply", "p.df"]).success();
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

/// The provider configured from the program: the kubeconfig's text as a
/// secret input, the environment's kubeconfig not a cluster. It applies
/// and converges, and no byte of the client key is in state, the plan
/// file, the audit log, the cache or the output.
#[test]
fn a_kubeconfig_held_as_a_secret_configures_the_provider() {
    let Some(kc) = kubeconfig("a_kubeconfig_held_as_a_secret_configures_the_provider") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let s = scratch("k8s-cluster-kubeconfig");
    let text = std::fs::read_to_string(&kc).unwrap();
    let key = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("client-key-data:"))
        .or_else(|| text.lines().find_map(|l| l.trim().strip_prefix("token:")))
        .expect("a kubeconfig with a client key or a token")
        .trim()
        .to_string();
    s.write(
        "p.df",
        &format!(
            "edition 2027\nprovider k8s {{ source = \"./providers/k8s\" }}\n\
             input kubeconfig: secret(string)\n\
             provider_config(\"kubernetes\", {{ kubeconfig: k }}) if kubeconfig(k)\n\
             resource k8s.namespace test {{\n  metadata.name = \"{}\"\n}}\n",
            ns.name
        ),
    );
    let set = format!("kubeconfig={text}");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_dform"))
            .args(args)
            .args(["--set", &set])
            .current_dir(&s.dir)
            .env("KUBECONFIG", s.path("no-such-kubeconfig"))
            .env_remove("DFORM_K8S_OFFLINE")
            .output()
            .unwrap();
        Run::from(out)
    };
    let mut out = String::new();
    let r = run(&["plan", "--out", "plan.json", "p.df"]).success();
    out += &(r.stdout + &r.stderr);
    let r = run(&["apply", "p.df"]).success();
    out += &(r.stdout + &r.stderr);
    let r = run(&["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
    out += &(r.stdout + &r.stderr);
    assert!(!out.contains(&key), "{out}");
    let mut files = Vec::new();
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            match e.path() {
                p if p.is_dir() => walk(&p, out),
                p => out.push(p),
            }
        }
    }
    walk(&s.dir, &mut files);
    for f in files.iter().filter(|f| !f.ends_with("p.df")) {
        let bytes = std::fs::read(f).unwrap_or_default();
        assert!(
            !bytes.windows(key.len()).any(|w| w == key.as_bytes()),
            "{} holds the key",
            f.display()
        );
    }
}

/// A world read (`world.k8s.config_map["NS/NAME"].data.K`) answers from a
/// live object dform does not manage.
#[test]
fn a_world_read_answers_from_a_live_object() {
    let Some(kc) = kubeconfig("a_world_read_answers_from_a_live_object") else {
        return;
    };
    let ns = Namespace::new(&kc);
    let (rt, client) = client(&kc);
    let nss: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(client.clone());
    let pp = kube::api::PostParams::default();
    rt.block_on(nss.create(
        &pp,
        &serde_json::from_value(json!({"metadata": {"name": ns.name}})).unwrap(),
    ))
    .unwrap();
    let cms: kube::Api<k8s_openapi::api::core::v1::ConfigMap> =
        kube::Api::namespaced(client, &ns.name);
    rt.block_on(
        cms.create(
            &pp,
            &serde_json::from_value(json!({"metadata": {"name": "release"},
                                       "data": {"COLOR": "green"}}))
            .unwrap(),
        ),
    )
    .unwrap();
    let s = scratch("k8s-cluster-world");
    s.write(
        "p.df",
        &format!(
            "edition 2027\nprovider k8s {{ source = \"./providers/k8s\" }}\n\
             color = world.k8s.config_map[\"{0}/release\"].data.COLOR\n\
             resource k8s.config_map serving {{\n  metadata.name = \"serving\"\n  \
             metadata.namespace = \"{0}\"\n  data = {{ \"COLOR\": color }}\n}}\n",
            ns.name
        ),
    );
    let r = dform(&s, &kc, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("data.COLOR = \"green\""),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}
