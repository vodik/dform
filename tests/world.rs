//! A world file is the mock cloud: `--world PATH` on plan and apply.

mod common;
use common::{Scratch, repo};

fn fixture(s: &Scratch) -> (String, String) {
    let world = repo().join("tests/fixtures/world/dform.json");
    let state = repo().join("tests/fixtures/world/dform.state.json");
    std::fs::copy(world, s.path("dform.json")).unwrap();
    std::fs::copy(state, s.path("dform.state.json")).unwrap();
    (
        repo()
            .join("examples/demo/stacks/dform.df")
            .to_str()
            .unwrap()
            .to_string(),
        "dform.json".to_string(),
    )
}

#[test]
fn editing_the_world_file_shows_drift_and_apply_writes_it_back() {
    let s = Scratch::new("world-drift");
    let (prog, world) = fixture(&s);
    let r = s.run(&["dev", "--world", &world, "plan", &prog]).success();
    assert_eq!(r.summary(), "stack dform is up to date", "{}", r.stdout);

    // Someone changed the VM out of band.
    let edited = s
        .read("dform.json")
        .replace("\"10.50.0.21\"", "\"10.50.0.99\"");
    s.write("dform.json", &edited);
    let r = s.run(&["dev", "--world", &world, "plan", &prog]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("~ compute.vm bastion"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("private_ip = \"10.50.0.99\" → \"10.50.0.21\""),
        "{}",
        r.stdout
    );

    s.run(&["dev", "--world", &world, "apply", &prog, "env=staging"])
        .success();
    assert!(s.read("dform.json").contains("\"10.50.0.21\""));
    let r = s.run(&["dev", "--world", &world, "plan", &prog]).success();
    assert_eq!(r.summary(), "stack dform is up to date", "{}", r.stdout);
    assert!(
        !s.path("dform.state").exists(),
        "--world keeps everything beside the world file"
    );
}

/// The world's objects state does not map are not dform's: with no state
/// file, as after a stack deleted everything it owned, none is adopted
/// and none is planned for delete.
#[test]
fn a_world_file_without_state_adopts_nothing() {
    let s = Scratch::new("world-bare");
    let (prog, world) = fixture(&s);
    std::fs::remove_file(s.path("dform.state.json")).unwrap();
    s.write("empty.df", "\nuse fake\n");
    let r = s
        .run(&["dev", "--world", &world, "plan", "empty.df"])
        .success();
    assert_eq!(r.summary(), "stack empty is up to date", "{}", r.stdout);
    // The program's resources are creates: the world's objects of the
    // same names are someone else's until an `adopt` says otherwise.
    let r = s.run(&["dev", "--world", &world, "plan", &prog]).success();
    assert!(
        !r.stdout.contains("\n- ") && !r.stdout.contains("\n> "),
        "{}",
        r.stdout
    );
}

/// A stack that deleted everything it owned plans nothing next: the
/// objects left in the world were never its own.
#[test]
fn an_emptied_stack_adopts_nothing() {
    let s = Scratch::new("world-emptied");
    s.write(
        "w.json",
        r#"{"resources": {
            "compute.vm::mine": {"typ": "compute.vm", "name": "mine", "attrs": {}, "computed": {"id": "vm-1"}},
            "compute.vm::theirs": {"typ": "compute.vm", "name": "theirs", "attrs": {}, "computed": {"id": "vm-2"}}}}"#,
    );
    s.write(
        "w.state.json",
        r#"{"version": 1, "resources": {"compute.vm::mine": {"provider": "fakecloud", "remote": "mine"}}}"#,
    );
    s.write("p.df", "\nuse fake\n");
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    assert!(r.stdout.contains("- compute.vm mine"), "{}", r.stdout);
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["plan"]))
        .success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    assert!(!s.read("w.state.json").contains("theirs"));
    assert!(s.read("w.json").contains("compute.vm::theirs"));
}
