//! One plan over the dependency tree: `plan apps` plans platform, which
//! apps reads, and apps as one run. Each provider is one process for the
//! tree, its handshake and its schema asked once; each object is read
//! once.

use crate::common;
use common::{Run, Scratch, calls, spans};

/// What apps reads: a network made at tick 1, a name known now, and the
/// kubeconfig of a server made at tick 1.
const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake { source = "./providers/fake" }
resource net.vpc main { cidr = "10.0.0.0/16" }
resource db.postgres server { name = "server-${env}" }
output kubeconfig = server.endpoint
output region = "bhs5-${env}"
output vpc = ref(main)
"#;

/// Apps: a subnet named for what platform knows now, one for the
/// network platform makes, and the cluster's namespace.
const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake { source = "./providers/fake" }
use stacks.platform
use k8s { source = "./providers/k8s", kubeconfig = platform[env].kubeconfig }
resource net.subnet named { cidr = "10.0.1.0/24", name = platform[env].region }
resource net.subnet inside { cidr = "10.0.2.0/24", name = platform[env].vpc }
resource k8s.namespace apps { metadata.name = "apps" }
"#;

/// The two stacks, the fake cloud and the Kubernetes provider each a
/// process of its own.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    for (dir, exe) in [
        ("fake", "dform-provider-fake"),
        ("k8s", "dform-provider-k8s"),
    ] {
        std::fs::create_dir_all(s.path(&format!("providers/{dir}"))).unwrap();
        std::os::unix::fs::symlink(common::exe(exe), s.path(&format!("providers/{dir}/{exe}")))
            .unwrap();
    }
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s
}

/// `dform ARGS` in `s`, its trace kept, with no cluster in reach: the
/// Kubernetes provider plans against its snapshot.
fn plan(s: &Scratch, args: &[&str]) -> (Run, Vec<serde_json::Value>) {
    let out = common::dform()
        .args(args)
        .args(["--trace", "trace.jsonl"])
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    (Run::from(out), spans(&s.path("trace.jsonl")))
}

/// Each provider is started, shaken hands with and asked its schema once
/// for the tree, though both deployments use it.
#[test]
fn a_trees_providers_start_once() {
    let s = project("one-plan-providers");
    let (r, spans) = plan(&s, &["plan", "apps"]);
    r.success();
    let handshakes: Vec<String> = spans
        .iter()
        .filter(|x| {
            x["name"]
                .as_str()
                .is_some_and(|n| n.ends_with(" Handshake"))
        })
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(handshakes.len(), 2, "one per provider: {handshakes:?}");
    assert_eq!(calls(&spans, "fakecloud Schema").len(), 1);
    assert_eq!(calls(&spans, "k8s Schema").len(), 1);
}

/// An image both deployments name is resolved at its registry once for
/// the tree.
#[test]
fn a_tag_both_deployments_name_is_resolved_once() {
    let port = common::registry_answering_together(1, std::time::Duration::from_secs(5));
    let image = format!("127.0.0.1:{port}/acme/app:a1");
    let s = project("one-plan-tag");
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}resource compute.vm build {{ image = oci.resolve(\"{image}\") }}\n"),
    );
    s.write(
        "stacks/apps.df",
        &format!("{APPS}resource compute.vm web {{ image = oci.resolve(\"{image}\") }}\n"),
    );
    std::fs::create_dir_all(s.path("cache")).unwrap();
    let out = common::dform()
        .args(["plan", "apps", "--trace", "trace.jsonl"])
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .env("DFORM_K8S_OFFLINE", "1")
        .env("XDG_CACHE_HOME", s.path("cache"))
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    let r = Run::from(out).success();
    // Pinned in both (the plan elides the digest's middle).
    let pinned = format!("{image}@sh");
    assert_eq!(r.stdout.matches(&pinned).count(), 2, "{}", r.stdout);
    let resolves = spans(&s.path("trace.jsonl"))
        .into_iter()
        .filter(|x| x["name"] == "oci.resolve")
        .count();
    assert_eq!(resolves, 1, "the tag asked once for the tree");
}
