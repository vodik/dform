//! The bootstrap and handover demo (examples/bootstrap/, README "Bootstrap
//! and handover"): a batch stack creates the cluster and installs dform in
//! it, the workload stack's state is handed over to a bucket (s3: MinIO
//! when `DFORM_S3_TEST_ENDPOINT` names one, as tests/s3.rs takes it, else
//! the fake S3 server), and the controller runs the workload from there.

mod common;
use common::{Run, Scratch};
use dform_core::store::{S3Spec, Store};
use dform_s3::S3Store;

const HANDED: &str = "dform.state/bootstrap/k8s/dform-system/workload";

/// A copy of the project in a scratch directory.
fn demo(name: &str) -> Scratch {
    let s = Scratch::new(name);
    common::copy_dir(&common::repo().join("examples/bootstrap"), &s.dir);
    s
}

/// The controller's log without its `HH:MM:SS ` stamps and its first line.
fn log(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l[9..].to_string())
        .filter(|l| !l.starts_with("controller "))
        .collect()
}

fn controller(s: &Scratch) -> Vec<String> {
    let r = s
        .run(&["controller", "run", "--once", "workload"])
        .success();
    log(&r.stdout)
}

/// Where the handed-over workload's state goes: a fresh prefix of a
/// bucket, emptied when dropped.
struct Bucket {
    endpoint: String,
    bucket: String,
    id: String,
    secret: String,
    prefix: String,
}

impl Bucket {
    fn new(name: &str) -> Bucket {
        static FAKE: std::sync::OnceLock<dform_s3::fake::Server> = std::sync::OnceLock::new();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let prefix = format!("dform-test-{}-{nanos:x}-{name}", std::process::id());
        let b = match std::env::var("DFORM_S3_TEST_ENDPOINT") {
            Ok(e) if !e.is_empty() => Bucket {
                endpoint: e,
                bucket: std::env::var("DFORM_S3_TEST_BUCKET")
                    .unwrap_or_else(|_| "dform-test".into()),
                id: std::env::var("DFORM_S3_ACCESS_KEY_ID").expect("DFORM_S3_ACCESS_KEY_ID"),
                secret: std::env::var("DFORM_S3_SECRET_ACCESS_KEY")
                    .expect("DFORM_S3_SECRET_ACCESS_KEY"),
                prefix,
            },
            _ => {
                eprintln!("{name}: on the fake S3 server (DFORM_S3_TEST_ENDPOINT is not set)");
                Bucket {
                    endpoint: FAKE
                        .get_or_init(dform_s3::fake::Server::start)
                        .endpoint
                        .clone(),
                    bucket: "dform-test".into(),
                    id: "fake".into(),
                    secret: "fake".into(),
                    prefix,
                }
            }
        };
        b.store("").create_bucket().unwrap();
        b
    }

    fn store(&self, rel: &str) -> S3Store {
        let spec = S3Spec {
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            endpoint: Some(self.endpoint.clone()),
            region: Some("us-east-1".into()),
        };
        S3Store::with_credentials(
            &spec,
            rel,
            rusty_s3::Credentials::new(&self.id, &self.secret),
        )
        .unwrap()
    }

    /// `s3(...)` of `rel` under the prefix.
    fn term(&self, rel: &str) -> String {
        format!(
            "s3(\"{}\", \"{}/{rel}\", {{endpoint: \"{}\", region: \"us-east-1\"}})",
            self.bucket, self.prefix, self.endpoint
        )
    }

    /// `dform ARGS` in `s`, with the bucket's credentials.
    fn run(&self, s: &Scratch, args: &[&str]) -> Run {
        let out = common::dform()
            .args(common::yes(args))
            .current_dir(&s.dir)
            .env("DFORM_S3_ACCESS_KEY_ID", &self.id)
            .env("DFORM_S3_SECRET_ACCESS_KEY", &self.secret)
            .output()
            .unwrap();
        Run::from(out)
    }

    fn controller(&self, s: &Scratch) -> Vec<String> {
        let r = self
            .run(s, &["controller", "run", "--once", "workload"])
            .success();
        log(&r.stdout)
    }
}

impl Drop for Bucket {
    fn drop(&mut self) {
        let all = self.store("");
        for k in all.list("").unwrap_or_default() {
            let _ = all.delete(&k);
        }
    }
}

fn edit(s: &Scratch, rel: &str, from: &str, to: &str) {
    let text = s.read(rel);
    assert!(text.contains(from), "{rel}: {text}");
    s.write(rel, &text.replace(from, to));
}

/// The dform-controller Deployment reads the helper `node_pool_up`, whose
/// instances are stuck until the pools have instance groups: once the pools
/// are planned the rule is a pending group (F DR-2 revised, last clause),
/// so the apply that makes them stops before the tick that adds the
/// Deployment (R-30) instead of stopping complete a tick early.
#[test]
fn a_resource_rule_reading_a_stuck_helper_is_a_pending_group() {
    let s = demo("bootstrap-helper");
    let runs = s.converge(&["apply", "bootstrap"]);
    assert_eq!(runs.len(), 3, "{:?}", runs.last().unwrap().stdout);
    let second = &runs[1];
    assert!(
        second.stdout.contains(
            "pending groups:\n? k8s.deployment[\"dform_controller\"] x unknown, on \
             ?google.container_node_pool[\"np-us-east1-b\"].instance_group, resolves after tick 1  \
             (reads node_pool_up(\"np-us-east1-b\"), which is stuck)\n"
        ),
        "{}",
        second.stdout
    );
    assert!(
        second.stderr.contains(
            "apply stopped after tick 1: tick 2 adds 1 deformation the plan could not name \
             (k8s.deployment[\"dform_controller\"] on \
             ?google.container_node_pool[\"np-us-east1-b\"].instance_group)"
        ),
        "{}",
        second.stderr
    );
    let last = runs.last().unwrap();
    assert!(
        last.stdout
            .contains("+ k8s.deployment[\"dform_controller\"]\n"),
        "{}",
        last.stdout
    );
    assert!(
        last.stdout.ends_with("apply: complete\n"),
        "{}",
        last.stdout
    );
    let world = s.read("dform.state/bootstrap/remote.json");
    assert!(world.contains("\"dform-controller\""), "{world}");
}

#[test]
fn bootstrap_handover_and_the_controller_runs_the_workload() {
    let s = demo("bootstrap");
    // The network and the cluster, then the node pools and the namespace,
    // then dform itself: each apply stops before what its plan could not
    // name (R-30).
    let runs = s.converge(&["apply", "bootstrap"]);
    assert_eq!(runs.len(), 3, "{:?}", runs.last().unwrap().stdout);
    let world = s.read("dform.state/bootstrap/remote.json");
    assert!(world.contains("\"dform-controller\""), "{world}");
    assert!(
        world.contains("\"run\",\n") && world.contains("\"workload\""),
        "{world}"
    );
    let r = s.run(&["plan", "bootstrap"]).success();
    assert_eq!(r.summary(), "stack bootstrap is undeformed");

    // The bootstrap stack stays batch.
    let r = s
        .run(&["controller", "run", "--once", "bootstrap"])
        .failure();
    assert!(
        r.stderr.contains(
            "stack bootstrap is role = bootstrap: it stays batch, and the controller never runs it"
        ),
        "{}",
        r.stderr
    );

    // The workload was planned from its default place; hand it over to
    // the bucket.
    let b = Bucket::new("bootstrap");
    let to = b.term("workload");
    let r = b
        .run(&s, &["stack", "handover", "workload", "--to", &to])
        .success();
    assert!(
        r.stdout
            .starts_with(&format!("stack workload handed over to {to}: s3://")),
        "{}",
        r.stdout
    );
    let registry = s.read("dform.state/stacks.json");
    assert!(
        registry.contains(&format!(
            "\"state\": \"s3://{}/{}/workload/state.json\"",
            b.bucket, b.prefix
        )),
        "{registry}"
    );

    // The controller starts on the workload, its state in the bucket; the
    // world (the mock's cluster) is the provider's and stays.
    assert_eq!(
        b.controller(&s),
        [
            "event start",
            "tick 1: plan: 3 deformations (3 create)",
            "stack workload is undeformed",
        ]
    );
    let keys = b.store("workload").list("").unwrap();
    for k in ["state.json", "state.key", "controller.json"] {
        assert!(keys.iter().any(|x| x == k), "{k} in {keys:?}");
    }
    let world = "dform.state/workload/remote.json";
    assert!(s.read(world).contains("gcr.io/renfry/web:1.0"));
    assert!(!s.path("dform.state/workload/state.json").exists());
    // A batch apply of a handed-over stack is refused; plan still reads it.
    let r = b.run(&s, &["apply", "workload"]).failure();
    assert!(
        r.stderr.contains(&format!(
            "stack workload was handed over to {to}: the controller runs it"
        )),
        "{}",
        r.stderr
    );
    let r = b.run(&s, &["plan", "workload"]).success();
    assert_eq!(r.summary(), "stack workload is undeformed");

    // A release: deployed.
    edit(&s, "data/releases.df", "web:1.0", "web:1.1");
    assert_eq!(
        b.controller(&s),
        [
            "input data.releases changed (file data/releases.df)",
            "event input data.releases",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
        ]
    );
    assert!(s.read(world).contains("gcr.io/renfry/web:1.1"));

    // Someone edits the cluster: replicas and the image.
    edit(&s, world, "\"replicas\": 3", "\"replicas\": 5");
    assert_eq!(
        b.controller(&s),
        [
            &format!("event world {world} changed"),
            "drift k8s.deployment[\"web\"].spec.replicas: 3 -> 5 (auto_reconcile)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
        ]
    );
    edit(
        &s,
        world,
        "gcr.io/renfry/web:1.1",
        "gcr.io/renfry/web:debug",
    );
    assert_eq!(
        b.controller(&s),
        [
            &format!("event world {world} changed"),
            "drift k8s.deployment[\"web\"].spec.template.spec.containers[0].image: \
             \"gcr.io/renfry/web:1.1\" -> \"gcr.io/renfry/web:debug\" \
             (held until approve or an input change)",
            "tick 1: plan: 1 deformation (1 update)",
            "tick 1: proceed: held, drift at spec.template.spec.containers[0].image needs \
             approval: k8s.deployment[\"web\"]",
            "stack workload is deformed: k8s.deployment[\"web\"] held",
        ]
    );
    let w = s.read(world);
    assert!(
        w.contains("\"replicas\": 3") && w.contains("web:debug"),
        "{w}"
    );
}

#[test]
fn handover_needs_one_bootstrap_stack_and_an_empty_target() {
    let s = demo("handover-errors");
    let to = "k8s(\"dform-system/workload\")";
    let r = s
        .run(&["stack", "handover", "workload", "--to", to])
        .failure();
    assert!(
        r.stderr
            .contains("no bootstrap stack is registered to own the cluster"),
        "{}",
        r.stderr
    );
    s.converge(&["apply", "bootstrap"]);
    let r = s
        .run(&["stack", "handover", "bootstrap", "--to", to])
        .failure();
    assert!(
        r.stderr
            .contains("a bootstrap stack stays batch and is never handed over"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&["stack", "handover", "workload", "--to", "s3(\"x\")"])
        .failure();
    assert!(
        r.stderr
            .contains("s3(\"x\"): backend s3(\"BUCKET\", \"PREFIX\", {endpoint: \"URL\", region: \"R\"}) takes a bucket, a prefix"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&["stack", "handover", "workload", "--to", "gcs(\"x\")"])
        .failure();
    assert!(
        r.stderr.contains("unknown backend gcs(\"x\")"),
        "{}",
        r.stderr
    );
    s.write(&format!("{HANDED}/state.json"), "{}");
    let r = s
        .run(&["stack", "handover", "workload", "--to", to])
        .failure();
    assert!(
        r.stderr.contains("is not empty: it holds state.json"),
        "{}",
        r.stderr
    );
    // Emptied, the in-cluster stand-in takes it, and the controller runs
    // it there.
    std::fs::remove_file(s.path(&format!("{HANDED}/state.json"))).unwrap();
    s.run(&["stack", "handover", "workload", "--to", to])
        .success();
    assert_eq!(
        controller(&s).last().unwrap(),
        "stack workload is undeformed"
    );
    assert!(s.path(&format!("{HANDED}/state.json")).exists());
    assert!(s.path(&format!("{HANDED}/remote.json")).exists());
}

#[test]
fn handover_moves_applied_state_to_a_local_backend() {
    let s = demo("handover-local");
    // The workload, run by a controller where it is, then moved.
    assert_eq!(
        controller(&s).last().unwrap(),
        "stack workload is undeformed"
    );
    assert!(s.path("dform.state/workload/state.json").exists());
    s.run(&["stack", "handover", "workload", "--to", "local(\"moved\")"])
        .success();
    assert!(!s.path("dform.state/workload").exists());
    assert!(s.path("moved/state.json").exists());
    // Nothing to do from the new place: the state and the memo moved too.
    assert_eq!(
        controller(&s),
        ["event resync", "stack workload is undeformed"]
    );
}
