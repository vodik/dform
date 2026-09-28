//! A world file is the mock cloud: `--world PATH` on plan and apply.

mod common;
use common::{Scratch, repo};

fn fixture(s: &Scratch) -> (String, String) {
    let world = repo().join("examples/world/dform.json");
    let state = repo().join("examples/world/dform.state.json");
    std::fs::copy(world, s.path("dform.json")).unwrap();
    std::fs::copy(state, s.path("dform.state.json")).unwrap();
    (repo().join("dform.df").to_str().unwrap().to_string(), "dform.json".to_string())
}

#[test]
fn editing_the_world_file_shows_drift_and_apply_writes_it_back() {
    let s = Scratch::new("world-drift");
    let (prog, world) = fixture(&s);
    let r = s.run(&["--file", &prog, "--world", &world, "plan"]).success();
    assert_eq!(r.summary(), "plan: 0 to create, 0 to update, 0 to delete", "{}", r.stdout);

    // Someone changed the VM out of band.
    let edited = s.read("dform.json").replace("\"10.50.0.21\"", "\"10.50.0.99\"");
    s.write("dform.json", &edited);
    let r = s.run(&["--file", &prog, "--world", &world, "plan"]).success();
    assert_eq!(r.summary(), "plan: 0 to create, 1 to update, 0 to delete", "{}", r.stdout);
    assert!(r.stdout.contains("~ compute.vm.bastion"), "{}", r.stdout);
    assert!(r.stdout.contains("private_ip: \"10.50.0.99\" -> \"10.50.0.21\""), "{}", r.stdout);

    s.run(&["--file", &prog, "--world", &world, "apply"]).success();
    assert!(s.read("dform.json").contains("\"10.50.0.21\""));
    let r = s.run(&["--file", &prog, "--world", &world, "plan"]).success();
    assert_eq!(r.summary(), "plan: 0 to create, 0 to update, 0 to delete", "{}", r.stdout);
    assert!(!s.path(".dform").exists(), "--world keeps everything beside the world file");
}

#[test]
fn a_world_file_without_state_is_taken_as_what_exists() {
    let s = Scratch::new("world-bare");
    let (prog, world) = fixture(&s);
    std::fs::remove_file(s.path("dform.state.json")).unwrap();
    let r = s.run(&["--file", &prog, "--world", &world, "plan"]).success();
    assert_eq!(r.summary(), "plan: 0 to create, 0 to update, 0 to delete", "{}", r.stdout);
}
