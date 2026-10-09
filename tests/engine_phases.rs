//! Nulls, stuck instances and phases (E §2.7) at the CLI: plan sections for
//! C's two-phase GKE stack against the gke mock schema.

mod common;
use common::{Scratch, repo};

const GKE_PLAN: &str = r#"plan: 6 changes (6 create) over 2 ticks; policy: 1 hold · 1 undetermined

tick 1  3 changes
  + google.compute_subnetwork gke_subnet            stacks/gke_two_phase.df:38
      ip_cidr_range = "10.141.76.0/22"              stacks/gke_two_phase.df:28
      name = "renfry-dev-gke-subnet"
      network = "projects/renfry-dev-973682/glo…l/networks/renfry-dev-network"
      project = "renfry-dev-973682"
      region = "us-east1"
  + google.container_cluster pngu                   stacks/gke_two_phase.df:54
      deletion_protection = true
      env = "dev"                                   stacks/gke_two_phase.df:11
      master_control_plane_cidr = "172.16.3.96/28"  stacks/gke_two_phase.df:28
      name = "renfry-dev-gke"
      network_id = "projects/renfry-dev-973682/glo…l/networks/renfry-dev-network"
      node_locations = ["us-east1-b", "us-east1-c"]
      project_id = "renfry-dev-973682"
      subnetwork_id = gke_subnet
  + google.compute_address static_ip                stacks/gke_two_phase.df:47
      name = "pngu-grpc"
      project = "renfry-dev-973682"
      region = "us-east1"
      subnetwork_id = gke_subnet

tick 2  3+ changes
  waits on  pngu.ca_certificate
            pngu.endpoint
            pngu.zones
  + k8s.deployment api                              stacks/gke_two_phase.df:77
      metadata.namespace = "pngu"
      spec.replicas = 3
      spec.selector.matchLabels.app = "api"
      spec.template.metadata.labels.app = "api"
      spec.template.spec.containers[name=api] = { name: "api", image: "gcr.io/renfry/api:1.42" }
  + k8s.namespace pngu                              stacks/gke_two_phase.df:74
      metadata.name = "pngu"
  + k8s.secret db_credentials                       stacks/gke_two_phase.df:85
      data.password = (sensitive)
      metadata.namespace = "pngu"
  google.container_node_pool "np-${z}"              stacks/gke_two_phase.df:91  waits on pngu.zones

policy  1 hold · 1 undetermined
  undetermined  cluster must be in at least two zones  stacks/gke_two_phase.df:100  1 undetermined
    google.container_cluster pngu                      until zones is known (tick 2)
"#;

/// Three definite, three pending on the kubernetes provider's configuration,
/// one pending group, one undetermined policy, and the apply order (E §7.4,
/// F 4.4 per key).
#[test]
fn gke_two_phase_plans_in_sections() {
    let s = Scratch::new("gke-sections");
    let prog = repo().join("examples/gke/stacks/gke_two_phase.df");
    let r = s
        .run(&[
            "dev",
            "--provider",
            "gke",
            "--provider",
            "k8s",
            "--world",
            "w.json",
            "plan",
            prog.to_str().unwrap(),
        ])
        .success();
    assert_eq!(r.stdout, GKE_PLAN);
}

/// F DR-11 revised: apply, then re-plan. Round 0 resolves every null through
/// the identity mapping before the Z-set is taken, so the stack cancels to
/// the zero Z-set with no null anywhere.
#[test]
fn apply_then_replan_is_undeformed() {
    let s = Scratch::new("undeformed");
    let prog = repo().join("examples/demo/stacks/dform.df");
    let prog = prog.to_str().unwrap();
    let run = |cmd: &str| {
        s.run(&common::on(prog, &["--world", "w.json"], &[cmd]))
            .success()
    };
    let first = run("plan");
    // A reference to a resource the plan makes is its address (R-111).
    assert!(first.stdout.contains("vpc = main.vpc"), "{}", first.stdout);
    assert!(!first.stdout.contains("undeformed"), "{}", first.stdout);
    s.run(&["dev", "--world", "w.json", "apply", prog, "env=staging"])
        .success();
    let again = run("plan");
    assert_eq!(
        again.stdout,
        "deployment: stacks.dform[env=staging]\nstack dform is up to date\n"
    );
}

fn gke(s: &Scratch, file: &str, extra: &[&str]) -> common::Run {
    let prog = repo().join("examples/gke/stacks").join(file);
    s.run(&common::on(
        prog.to_str().unwrap(),
        &[
            "--provider",
            "gke",
            "--provider",
            "k8s",
            "--world",
            "w.json",
        ],
        extra,
    ))
}

fn world_resources(s: &Scratch) -> Vec<String> {
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// E §2.7 item 6: tick 1 creates the subnet, the address and the cluster;
/// the boundary resolves the cluster's zones, endpoint and ca; tick 2 creates
/// a nodepool per zone and the kubernetes objects. The nodepools are a
/// pending group tick 1's plan could not name: tick 2's plan names them,
/// and `--yes` applies it (R-122). Apply again: undeformed.
#[test]
fn gke_two_phase_applies_in_two_ticks() {
    let s = Scratch::new("gke-ticks");
    let r = gke(&s, "gke_two_phase.df", &["apply"]).success();
    assert!(
        r.stdout.starts_with(
            "plan: 6 changes (6 create) over 2 ticks; policy: 1 hold · 1 undetermined\n\ntick 1  "
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "plan: 5 changes (5 create) over 1 tick; policy: 2 hold\n\ntick 2  5 changes\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
    assert_eq!(
        world_resources(&s),
        [
            "google.compute_address::static_ip",
            "google.compute_subnetwork::gke_subnet",
            "google.container_cluster::pngu",
            "google.container_node_pool::np-us-east1-b",
            "google.container_node_pool::np-us-east1-c",
            "k8s.deployment::api",
            "k8s.namespace::pngu",
            "k8s.secret::db_credentials",
        ]
    );
    let again = gke(&s, "gke_two_phase.df", &["apply"]).success();
    assert_eq!(again.stdout, "stack gke_two_phase is up to date\n");
}

/// The other branch of item 6: placed in one zone (`--set zones=1`), the
/// cluster comes back with one, the policy derives at the boundary, and
/// apply stops after tick 1 with the deny printed, before the nodepools and
/// the kubernetes objects.
#[test]
fn gke_one_zone_stops_after_tick_one() {
    let s = Scratch::new("gke-one-zone");
    let r = gke(&s, "gke_two_phase.df", &["apply", "--set", "zones=1"]).failure();
    assert!(r.stdout.contains("tick 1  "), "{}", r.stdout);
    // One plan printed: no later tick was planned.
    assert_eq!(r.stdout.matches("plan: ").count(), 1, "{}", r.stdout);
    assert!(
        r.stderr
            .contains("- cluster must be in at least two zones  cluster = \"pngu\"")
            && r.stderr
                .contains("; stopped after tick 1; ticks 1 to 1 were applied"),
        "{}",
        r.stderr
    );
    assert_eq!(
        world_resources(&s),
        [
            "google.compute_address::static_ip",
            "google.compute_subnetwork::gke_subnet",
            "google.container_cluster::pngu",
        ]
    );
}

/// `--max-ticks` bounds the loop.
#[test]
fn max_ticks_bounds_the_loop() {
    let s = Scratch::new("gke-max-ticks");
    let r = gke(&s, "gke_two_phase.df", &["apply", "--max-ticks", "1"]).failure();
    assert!(
        r.stderr
            .contains("apply stopped after 1 ticks (--max-ticks)"),
        "{}",
        r.stderr
    );
    assert_eq!(world_resources(&s).len(), 3);
}

/// An update pending on an open null (E §2.8) is decided and applied at
/// the next tick, once the resource that owns the null exists.
#[test]
fn a_pending_update_applies_after_the_boundary() {
    let s = Scratch::new("pending-update");
    s.write_owned_world(
        "w.json",
        r#"{"resources": {"compute.vm::app": {"typ": "compute.vm", "name": "app",
            "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}}"#,
    );
    s.write(
        "p.df",
        "\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\nuse fake\n",
    );
    // Tick 2's plan, re-derived at the boundary, is printed again with the
    // value it now knows (After R-206).
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    assert!(
        r.stdout
            .contains("plan: 2 changes (1 create, 1 update) over 2 ticks\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .split_once("plan: 1 change (1 update) over 1 tick\n\ntick 2  1 change\n")
            .is_some_and(|(_, t)| t.contains("db_host = \"old.db.fake\" → \"main.db.fake\"")),
        "{}",
        r.stdout
    );
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["plan"]))
        .success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}
