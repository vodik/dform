//! Adoption against the discovery inventory.

mod common;
use common::{Scratch, repo};

/// `cloud_ref(T, N, tags.owner)` walks the inventory's nested attributes.
#[test]
fn adopt_demo_plans_with_dotted_cloud_refs() {
    let s = Scratch::new("adopt");
    std::fs::create_dir_all(s.path(".dform")).unwrap();
    std::fs::copy(repo().join("examples/inventory.json"), s.path(".dform/inventory.json")).unwrap();
    let prog = repo().join("examples/adopt_demo.df");
    let r = s.run(&["--file", prog.to_str().unwrap(), "plan", "--set", "env=prod"]).success();
    assert!(r.stdout.contains("> net.vpc.network.main::vpc"), "{}", r.stdout);
    assert!(r.stdout.contains("owner_tag = \"team-a\""), "{}", r.stdout);
}
