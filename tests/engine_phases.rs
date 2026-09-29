//! Nulls, stuck instances and phases (E §2.7) at the CLI: plan sections for
//! C's two-phase GKE stack against the gke mock schema.

mod common;
use common::{Scratch, repo};

const GKE_PLAN: &str = r#"plan: 3 deformations (3 create), 4 pending, 1 undetermined
definite:
+ google_compute_subnetwork.gke_subnet
  ip_cidr_range = "10.141.76.0/22"
  name = "renfry-dev-gke-subnet"
  network = "projects/renfry-dev-973682/global/networks/renfry-dev-network"
  project = "renfry-dev-973682"
  region = "us-east1"
+ gke_cluster.pngu
  deletion_protection = true
  env = "dev"
  master_control_plane_cidr = "172.16.3.96/28"
  name = "renfry-dev-gke"
  network_id = "projects/renfry-dev-973682/global/networks/renfry-dev-network"
  node_locations[0] = "us-east1-b"
  node_locations[1] = "us-east1-c"
  project_id = "renfry-dev-973682"
  subnetwork_id = ?google_compute_subnetwork/gke_subnet#id
+ google_compute_address.static_ip
  name = "pngu-grpc"
  project = "renfry-dev-973682"
  region = "us-east1"
  subnetwork_id = ?google_compute_subnetwork/gke_subnet#id
pending on ?gke_cluster/pngu#ca_certificate ?gke_cluster/pngu#endpoint (resolves after tick 1):
+ k8s.deployment.api
  image = "gcr.io/renfry/api:1.42"
  namespace = "pngu"
  replicas = 3
+ k8s.namespace.pngu
  name = "pngu"
+ k8s.secret.db_credentials
  data.password = (sensitive google.secret_manager_secret_version/db_pw#secret_data)
  namespace = "pngu"
pending groups:
? gke_nodepool.? x unknown, on ?gke_cluster/pngu#zones, resolves after tick 1  (member/2 over a null list)
undetermined:
? deny "cluster must be in at least two zones" on ?gke_cluster/pngu#zones, decided after tick 1  (reads undetermined aggregate zone_count)
apply order: tick 1 [google_compute_subnetwork.gke_subnet gke_cluster.pngu google_compute_address.static_ip] tick 2 [k8s.deployment.api k8s.namespace.pngu k8s.secret.db_credentials gke_nodepool.?]
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
    assert!(first.stdout.contains('?'), "{}", first.stdout);
    assert!(!first.stdout.contains("undeformed"), "{}", first.stdout);
    s.run(&["dev", "--world", "w.json", "apply", prog, "env=staging"])
        .success();
    let again = run("plan");
    assert_eq!(again.stdout, "stack dform is undeformed\n");
}

fn gke(s: &Scratch, file: &str, extra: &[&str]) -> common::Run {
    let prog = repo().join("examples/gke/stacks").join(file);
    s.run(&common::on(
        prog.to_str().unwrap(),
        &["--provider", "gke", "--world", "w.json"],
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
/// a nodepool per zone and the kubernetes objects. Apply again: undeformed.
#[test]
fn gke_two_phase_applies_in_two_ticks() {
    let s = Scratch::new("gke-ticks");
    let r = gke(&s, "gke_two_phase.df", &["apply"]).success();
    assert!(
        r.stdout
            .contains("tick 1:\nplan: 3 deformations (3 create), 4 pending, 1 undetermined\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("tick 2:\nplan: 5 deformations (5 create)\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("tick 3:"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    assert_eq!(
        world_resources(&s),
        [
            "gke_cluster::pngu",
            "gke_nodepool::np-us-east1-b",
            "gke_nodepool::np-us-east1-c",
            "google_compute_address::static_ip",
            "google_compute_subnetwork::gke_subnet",
            "k8s.deployment::api",
            "k8s.namespace::pngu",
            "k8s.secret::db_credentials",
        ]
    );
    let again = gke(&s, "gke_two_phase.df", &["apply"]).success();
    assert_eq!(
        again.stdout,
        "stack gke_two_phase is undeformed\napply: nothing to do\n"
    );
}

/// The other branch of item 6: placed in one zone (`--set zones=1`), the
/// cluster comes back with one, the policy derives at the boundary, and
/// apply stops after tick 1 with the deny printed, before the nodepools and
/// the kubernetes objects.
#[test]
fn gke_one_zone_stops_after_tick_one() {
    let s = Scratch::new("gke-one-zone");
    let r = gke(&s, "gke_two_phase.df", &["apply", "--set", "zones=1"]).failure();
    assert!(r.stdout.contains("tick 1:"), "{}", r.stdout);
    assert!(!r.stdout.contains("tick 2:"), "{}", r.stdout);
    assert!(
        r.stderr
            .contains("- cluster must be in at least two zones ctx={\"cluster\":\"pngu\"}")
            && r.stderr
                .contains("apply stopped after tick 1: blocked by constraints"),
        "{}",
        r.stderr
    );
    assert_eq!(
        world_resources(&s),
        [
            "gke_cluster::pngu",
            "google_compute_address::static_ip",
            "google_compute_subnetwork::gke_subnet",
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
        "edition 2026\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\n",
    );
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    assert!(
        r.stdout
            .contains("tick 1:\nplan: 1 deformation (1 create), 1 pending\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("tick 2:\nplan: 1 deformation (1 update)\ndefinite:\n~ compute.vm.app\n  db_host: \"old.db.fake\" -> \"main.db.fake\"\n"),
        "{}",
        r.stdout
    );
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["plan"]))
        .success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
}
