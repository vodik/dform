//! An attribute's value is merged leaf by leaf (R-116), the provider's
//! computed values too: a map whose usual keys the schema types (a
//! claim's `status.allocatedResources`, with `cpu`, `memory`, `storage`
//! and `ephemeral-storage` typed as quantities) is minted as those keys,
//! not as one unknown map beside its own keys, which disagreed with it
//! ("two contributions disagree at status.allocatedResources.cpu").

mod common;
use common::{Scratch, repo};

/// A schema of our own: a volume whose computed `status.capacity` is a
/// map with two typed keys.
fn volume(s: &Scratch) {
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + r#"
type_provider(db.volume, "fakecloud")
type_attr(db.volume, "id", "string", ["computed", "id"])
type_attr(db.volume, "size", "int", [])
type_attr(db.volume, "status.capacity", "map", ["computed"])
type_attr(db.volume, "status.capacity.storage", "bytes(quantity)", ["computed"])
type_attr(db.volume, "status.capacity.cpu", "cpu(quantity)", ["computed"])
"#),
    );
}

#[test]
fn a_computed_map_with_typed_keys_is_its_keys() {
    let s = Scratch::project("object-leaves");
    volume(&s);
    s.write(
        "stacks/lab.df",
        "use fake\nresource db.volume data { size = 10 }\n",
    );
    let r = s.run(&["plan", "lab"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("disagree"), "{}", r.stdout);
    s.run(&["apply", "lab"]).success();
    let r = s.run(&["plan", "lab"]).success();
    assert_eq!(r.summary(), "stack lab is up to date", "{}", r.stdout);
}

/// The witness: a PersistentVolumeClaim planned against the Kubernetes
/// provider's own schema (the OpenAPI snapshot, offline).
#[test]
fn a_claim_plans_against_the_kubernetes_schema() {
    let s = Scratch::project("object-leaves-k8s");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "stacks/lab.df",
        r#"
use k8s { source = "./providers/k8s" }

resource k8s.namespace traefik { metadata.name = "traefik" }

resource k8s.persistent_volume_claim acme {
  metadata = { name: "traefik-acme", namespace: traefik.metadata.name }
  spec.accessModes = ["ReadWriteOnce"]
  spec.resources.requests.storage = "1Gi"
}
"#,
    );
    let mut c = common::dform();
    c.args(["plan", "lab"])
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1");
    let r = common::Run::from(c.output().unwrap()).success();
    assert_eq!(
        r.summary(),
        "plan: 2 changes (2 create) over 1 tick",
        "{}",
        r.stdout
    );
}
