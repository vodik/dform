//! Nulls, stuck instances and phases (E §2.7) at the CLI: plan sections for
//! C's two-phase GKE stack against the gke mock schema.

mod common;
use common::{Scratch, repo};

const GKE_PLAN: &str = r#"plan: 3 to create, 0 to update, 0 to delete, 3 pending
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
pending on ?gke_cluster/pngu#ca_certificate ?gke_cluster/pngu#endpoint:
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
? want("gke_nodepool", _) x unknown, on ?gke_cluster/pngu#zones  (member/2 over a null list)
undetermined:
? deny "cluster must be in at least two zones" on ?gke_cluster/pngu#zones  (reads undetermined aggregate zone_count)
"#;

/// Three definite, three pending on the kubernetes provider's configuration,
/// one pending group, one undetermined policy (E §7.4, F 4.4 per key).
#[test]
fn gke_two_phase_plans_in_sections() {
    let s = Scratch::new("gke-sections");
    let prog = repo().join("examples/adversarial/gke_two_phase.df");
    let r = s
        .run(&[
            "--file",
            prog.to_str().unwrap(),
            "--provider",
            "gke",
            "--world",
            "w.json",
            "plan",
        ])
        .success();
    assert_eq!(r.stdout, GKE_PLAN);
}
