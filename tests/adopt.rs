//! Adoption against the discovery inventory.

mod common;
use common::{Scratch, repo};

/// `cloud_ref(T, N, tags.owner)` walks the inventory's nested attributes.
/// Runs from a clean clone: `--inventory` points straight at the shipped
/// fixture, no `dform.state/` setup needed.
#[test]
fn adopt_demo_plans_with_dotted_cloud_refs() {
    let s = Scratch::new("adopt");
    let prog = repo().join("examples/adopt/stacks/adopt_demo.df");
    let inventory = repo().join("tests/fixtures/world/inventory.json");
    let r = s
        .run(&[
            "dev",
            "--inventory",
            inventory.to_str().unwrap(),
            "plan",
            "--set",
            "env=prod",
            prog.to_str().unwrap(),
        ])
        .success();
    assert!(
        r.stdout.contains("> net.vpc[\"network/vpc\"]"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("owner_tag = \"team-a\""), "{}", r.stdout);
}

/// With `--world` given and no `--inventory`, the inventory beside the world
/// file is picked up automatically.
#[test]
fn world_flag_defaults_inventory_beside_it() {
    let s = Scratch::new("adopt-world-default");
    let prog = repo().join("examples/adopt/stacks/adopt_demo.df");
    std::fs::create_dir_all(s.path("world")).unwrap();
    std::fs::copy(
        repo().join("tests/fixtures/world/inventory.json"),
        s.path("world/inventory.json"),
    )
    .unwrap();
    let r = s
        .run(&[
            "dev",
            "--world",
            s.path("world/dform.json").to_str().unwrap(),
            "plan",
            "--set",
            "env=prod",
            prog.to_str().unwrap(),
        ])
        .success();
    assert!(
        r.stdout.contains("> net.vpc[\"network/vpc\"]"),
        "{}",
        r.stdout
    );
}
