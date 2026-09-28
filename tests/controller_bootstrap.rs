//! The bootstrap and handover demo (examples/bootstrap/, README "Bootstrap
//! and handover"): a batch stack creates the cluster and installs dform in
//! it, the workload stack's state is handed over to the in-cluster backend,
//! and the controller runs the workload from there.

mod common;
use common::Scratch;

const HANDED: &str = "dform.state/renfry.bootstrap/k8s/dform-system/workload";

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
        .run(&["controller", "run", "--once", "renfry.workload"])
        .success();
    log(&r.stdout)
}

fn edit(s: &Scratch, rel: &str, from: &str, to: &str) {
    let text = s.read(rel);
    assert!(text.contains(from), "{rel}: {text}");
    s.write(rel, &text.replace(from, to));
}

fn ticks(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| l.starts_with("tick ")).collect()
}

/// The dform-controller Deployment reads the helper `node_pool_up`, whose
/// instances are stuck until the pools have instance groups: at tick 2 the
/// rule is a pending group (F DR-2 revised, last clause), so the apply
/// runs a third tick instead of stopping complete a tick early.
#[test]
fn a_resource_rule_reading_a_stuck_helper_is_a_pending_group() {
    let s = demo("bootstrap-helper");
    let r = s.run(&["apply", "renfry.bootstrap"]).success();
    let tick2 = r
        .stdout
        .split("tick 2:\n")
        .nth(1)
        .and_then(|t| t.split("tick 3:\n").next())
        .unwrap_or_default();
    assert!(
        tick2.contains(
            "pending groups:\n? k8s.deployment.dform_controller x unknown, on \
             ?gke_nodepool/np-us-east1-b#instance_group, resolves after tick 2  \
             (reads node_pool_up(\"np-us-east1-b\"), which is stuck)\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        tick2.contains("tick 3 [k8s.deployment.dform_controller]"),
        "{tick2}"
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let world = s.read("dform.state/renfry.bootstrap/remote.json");
    assert!(world.contains("\"dform-controller\""), "{world}");
}

#[test]
fn bootstrap_handover_and_the_controller_runs_the_workload() {
    let s = demo("bootstrap");
    // Tick 1 the network and the cluster, tick 2 the node pools and the
    // namespace, tick 3 dform itself.
    let r = s.run(&["apply", "renfry.bootstrap"]).success();
    assert_eq!(
        ticks(&r.stdout),
        ["tick 1:", "tick 2:", "tick 3:"],
        "{}",
        r.stdout
    );
    let world = s.read("dform.state/renfry.bootstrap/remote.json");
    assert!(world.contains("\"dform-controller\""), "{world}");
    assert!(
        world.contains("\"run\",\n") && world.contains("\"renfry.workload\""),
        "{world}"
    );
    let r = s.run(&["plan", "renfry.bootstrap"]).success();
    assert_eq!(r.summary(), "stack renfry.bootstrap is undeformed");

    // The bootstrap stack stays batch.
    let r = s
        .run(&["controller", "run", "--once", "renfry.bootstrap"])
        .failure();
    assert!(
        r.stderr.contains(
            "stack renfry.bootstrap is role = bootstrap: it stays batch, and the controller never runs it"
        ),
        "{}",
        r.stderr
    );

    // The workload was planned from its default place; hand it over.
    let r = s
        .run(&[
            "stack",
            "handover",
            "renfry.workload",
            "--to",
            "k8s(\"dform-system/workload\")",
        ])
        .success();
    assert!(
        r.stdout
            .starts_with("stack renfry.workload handed over to k8s(\"dform-system/workload\"): "),
        "{}",
        r.stdout
    );
    let registry = s.read("dform.state/stacks.json");
    assert!(
        registry.contains("\"backend\": \"k8s(\\\"dform-system/workload\\\")\""),
        "{registry}"
    );

    // The controller starts on the workload, in its new place.
    assert_eq!(
        controller(&s),
        [
            "event start",
            "tick 1: plan: 3 deformations (3 create)",
            "stack renfry.workload is undeformed",
        ]
    );
    let world = format!("{HANDED}/remote.json");
    assert!(s.read(&world).contains("gcr.io/renfry/web:1.0"));
    assert!(!s.path("dform.state/renfry.workload").exists());
    // A batch apply of a handed-over stack is refused; plan still reads it.
    let r = s.run(&["apply", "renfry.workload"]).failure();
    assert!(
        r.stderr.contains(
            "stack renfry.workload was handed over to k8s(\"dform-system/workload\"): the controller runs it"
        ),
        "{}",
        r.stderr
    );
    let r = s.run(&["plan", "renfry.workload"]).success();
    assert_eq!(r.summary(), "stack renfry.workload is undeformed");

    // A release: deployed.
    edit(&s, "data/release.facts", "web:1.0", "web:1.1");
    assert_eq!(
        controller(&s),
        [
            "input release changed (file data/release.facts)",
            "event input release",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    assert!(s.read(&world).contains("gcr.io/renfry/web:1.1"));

    // Someone edits the cluster: replicas and the image. (The registry
    // holds the handed-over place as an absolute path; the log names it
    // from the root.)
    edit(&s, &world, "\"replicas\": 3", "\"replicas\": 5");
    assert_eq!(
        controller(&s),
        [
            &format!("event world {world} changed"),
            "drift k8s.deployment.web spec.replicas: 3 -> 5 (auto_reconcile)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    edit(
        &s,
        &world,
        "gcr.io/renfry/web:1.1",
        "gcr.io/renfry/web:debug",
    );
    assert_eq!(
        controller(&s),
        [
            &format!("event world {world} changed"),
            "drift k8s.deployment.web spec.template.spec.containers[0].image: \
             \"gcr.io/renfry/web:1.1\" -> \"gcr.io/renfry/web:debug\" \
             (held until approve or an input change)",
            "tick 1: plan: 1 deformation (1 update)",
            "tick 1: proceed: held, drift at spec.template.spec.containers[0].image needs \
             approval: k8s.deployment.web",
            "stack renfry.workload is deformed: k8s.deployment.web held",
        ]
    );
    let w = s.read(&world);
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
        .run(&["stack", "handover", "renfry.workload", "--to", to])
        .failure();
    assert!(
        r.stderr
            .contains("no bootstrap stack is registered to own the cluster"),
        "{}",
        r.stderr
    );
    s.run(&["apply", "renfry.bootstrap"]).success();
    let r = s
        .run(&["stack", "handover", "renfry.bootstrap", "--to", to])
        .failure();
    assert!(
        r.stderr
            .contains("a bootstrap stack stays batch and is never handed over"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&["stack", "handover", "renfry.workload", "--to", "s3(\"x\")"])
        .failure();
    assert!(
        r.stderr.contains("unknown backend s3(\"x\")"),
        "{}",
        r.stderr
    );
    s.write(&format!("{HANDED}/stray"), "");
    let r = s
        .run(&["stack", "handover", "renfry.workload", "--to", to])
        .failure();
    assert!(r.stderr.contains("is not empty"), "{}", r.stderr);
}

#[test]
fn handover_moves_applied_state_to_a_local_backend() {
    let s = demo("handover-local");
    // The workload, run by a controller where it is, then moved.
    assert_eq!(
        controller(&s).last().unwrap(),
        "stack renfry.workload is undeformed"
    );
    assert!(s.path("dform.state/renfry.workload/state.json").exists());
    s.run(&[
        "stack",
        "handover",
        "renfry.workload",
        "--to",
        "local(\"moved\")",
    ])
    .success();
    assert!(!s.path("dform.state/renfry.workload").exists());
    assert!(s.path("moved/state.json").exists());
    // Nothing to do from the new place: the state and the memo moved too.
    assert_eq!(
        controller(&s),
        ["event resync", "stack renfry.workload is undeformed"]
    );
}
