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

/// The ticks are the tree's: an apps change reading only what platform
/// knows now is in tick 1 beside platform's; one reading what platform
/// makes at tick 1, and the namespace behind the kubeconfig, in tick 2.
#[test]
fn an_apps_change_is_in_the_tick_after_what_it_reads() {
    let s = project("one-plan-ticks");
    let (r, _) = plan(&s, &["plan", "apps"]);
    let r = r.success();
    assert_eq!(
        r.summary(),
        "plan: 5 changes (5 create) over 2 ticks",
        "{}",
        r.stdout
    );
    let apps = r.stdout.split("stacks.apps[env=lab]").nth(1).unwrap();
    let tick1 = apps.split("  tick 2").next().unwrap();
    assert!(
        tick1.contains("  tick 1  1 change\n    + net.subnet named"),
        "{}",
        r.stdout
    );
    let tick2 = apps.split("  tick 2").nth(1).unwrap();
    for x in [
        "+ k8s.namespace apps",
        "+ net.subnet inside",
        "stacks.platform[env=lab].vpc",
    ] {
        assert!(tick2.contains(x), "{x}\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("later"), "{}", r.stdout);
}

/// One refresh per object: a plan of the tree reads each object of the
/// deployments' states once.
#[test]
fn a_trees_plan_reads_each_object_once() {
    let s = project("one-plan-refresh");
    // The fake cloud's endpoint is no kubeconfig: apps without the cluster.
    let apps: String = APPS
        .lines()
        .filter(|l| !l.contains("k8s"))
        .collect::<Vec<_>>()
        .join("\n");
    s.write("stacks/apps.df", &apps);
    let (r, _) = plan(&s, &["apply", "--yes", "platform"]);
    r.success();
    let (r, spans) = plan(&s, &["plan", "apps"]);
    r.success();
    assert_eq!(
        calls(&spans, "fakecloud Read"),
        ["db.postgres server", "net.vpc main"]
    );
}

/// The tree's plan file: one file, each deployment's section with its own
/// digest and its changes under the tree's ticks.
#[test]
fn a_trees_plan_file_has_a_section_per_deployment() {
    let s = project("one-plan-file");
    let (r, _) = plan(&s, &["plan", "apps", "--out", "plan.json"]);
    let r = r.success();
    assert!(
        r.stderr.contains("plan digests: stacks.platform[env=lab] "),
        "{}",
        r.stderr
    );
    let f = s.json("plan.json");
    let sections = f["deployments"].as_array().unwrap();
    let names: Vec<&str> = sections
        .iter()
        .map(|d| d["deployment"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["stacks.platform[env=lab]", "stacks.apps[env=lab]"]);
    assert!(sections.iter().all(|d| d["digest"].is_string()), "{f}");
    let ticks = &sections[1]["ticks"];
    let at = |n: u64| {
        ticks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["tick"] == n)
            .map(|t| t.to_string())
            .unwrap_or_default()
    };
    assert!(at(1).contains("net.subnet[\\\"named\\\"]"), "{ticks}");
    assert!(at(2).contains("net.subnet[\\\"inside\\\"]"), "{ticks}");
}

/// Platform, whose kubeconfig is a secret output it reads from a file of
/// its env's, and apps, whose provider is configured from it; the master
/// is the passphrase in `DFORM_TEST_PASSPHRASE`.
fn sealed_project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\nk8s = \"k8s\"\n\n\
         [secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
    );
    s.write(
        "stacks/platform.df",
        r#"key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc main { cidr_block = "10.0.0.0/16" }
let raw: secret(string) = io.read("kubeconfig-${env}.yaml")
output kubeconfig: secret(string) = raw
"#,
    );
    s.write(
        "stacks/apps.df",
        r#"key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s { kubeconfig = platform[env].kubeconfig }
resource k8s.namespace apps { metadata.name = "apps" }
"#,
    );
    s.write(
        "kubeconfig-lab.yaml",
        "apiVersion: v1 # KUBECONFIG-SECRET-LAB",
    );
    s
}

/// A reader in the tree needs no master to read a sealed output of a
/// deployment in it: the value is the producer's plan's, in memory
/// (never printed, never written). Without any master the reader's
/// provider is configured from the kubeconfig the producer read, and its
/// objects plan in tick 1; what only the producer's master derives (a
/// `random.password`) stays the producer's stand-in, and what reads it
/// waits.
#[test]
fn a_reader_in_the_tree_reads_a_sealed_output_without_its_master() {
    let s = sealed_project("one-plan-masters");
    let out = common::dform()
        .args(["plan", "apps"])
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .env_remove("DFORM_TEST_PASSPHRASE")
        .env_remove("RANDOM_MASTER")
        .output()
        .unwrap();
    let r = Run::from(out).success();
    let apps = r.stdout.split("stacks.apps[env=lab]").nth(1).unwrap();
    assert!(
        apps.contains("  tick 1  1 change\n    + k8s.namespace apps"),
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("needs its master"), "{}", r.stderr);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("KUBECONFIG-SECRET"), "{out}");
    }
}

/// A reader keyed by env reads its upstream at its own env's value, not
/// the key's default: planned at env=prod, apps reads platform[env=prod]
/// (never applied), and says nothing of lab's, whose kubeconfig is not
/// sealed to it.
#[test]
fn a_keyed_reader_reads_its_own_keys_upstream_not_the_defaults() {
    let s = sealed_project("one-plan-keyed-reader");
    s.write(
        "kubeconfig-prod.yaml",
        "apiVersion: v1 # KUBECONFIG-SECRET-PROD",
    );
    let run = |args: &[&str]| {
        let out = common::dform()
            .args(args)
            .current_dir(&s.dir)
            .env("NO_COLOR", "1")
            .env("DFORM_K8S_OFFLINE", "1")
            .env("DFORM_TEST_PASSPHRASE", "one-plan")
            .output()
            .unwrap();
        Run::from(out).success()
    };
    run(&["apply", "platform", "env=lab", "--yes"]);
    let r = run(&["plan", "apps", "env=prod"]);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("env=lab"), "{out}");
    }
    assert!(
        r.stdout
            .contains("+ stacks.platform[env=prod]  stacks/platform.df  never applied"),
        "{}",
        r.stdout
    );
}
