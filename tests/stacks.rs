//! State is scoped to the program: two programs against one `.dform/` do not
//! see each other's resources.

mod common;
use common::Scratch;

const NET: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { vpc_id = ref(net.vpc, main, id), cidr = "10.0.1.0/24" }.
"#;

const DB: &str = r#"
resource db.postgres main { backup_days = 7 }.
"#;

#[test]
fn a_second_program_does_not_plan_deletes_of_the_first() {
    let s = Scratch::new("stacks");
    s.write("net.df", NET);
    s.write("db.df", DB);

    s.run(&["--file", "net.df", "apply"]).success();
    assert!(s.path(".dform/net/state.json").exists());

    let db = s.run(&["--file", "db.df", "plan"]).success();
    assert_eq!(
        db.summary(),
        "plan: 1 to create, 0 to update, 0 to delete",
        "{}",
        db.stdout
    );

    s.run(&["--file", "db.df", "apply"]).success();
    let net = s.run(&["--file", "net.df", "plan"]).success();
    assert_eq!(
        net.summary(),
        "plan: 0 to create, 0 to update, 0 to delete",
        "{}",
        net.stdout
    );
}

#[test]
fn unscoped_state_migrates_to_the_dform_stack() {
    let s = Scratch::new("migrate");
    s.write("dform.df", NET);
    s.run(&["--file", "dform.df", "apply"]).success();
    // Put the stack's files back where an old dform wrote them.
    std::fs::rename(
        s.path(".dform/dform/state.json"),
        s.path(".dform/state.json"),
    )
    .unwrap();
    std::fs::rename(
        s.path(".dform/dform/remote.json"),
        s.path(".dform/remote.json"),
    )
    .unwrap();

    let r = s.run(&["--file", "dform.df", "plan"]).success();
    assert!(r.stderr.contains("moved unscoped"), "{}", r.stderr);
    assert_eq!(
        r.summary(),
        "plan: 0 to create, 0 to update, 0 to delete",
        "{}",
        r.stdout
    );
    assert!(!s.path(".dform/state.json").exists());
    assert!(s.path(".dform/dform/state.json").exists());
}
