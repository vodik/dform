//! Computed values come only from Apply; secrets travel as labels.

mod common;
use common::{Scratch, repo};

#[test]
fn a_fresh_stack_carries_nulls_until_apply() {
    let s = Scratch::new("computed-fresh");
    let prog = repo().join("dform.df");
    let prog = prog.to_str().unwrap();
    let r = s
        .run(&["--file", prog, "--world", "w.json", "plan"])
        .success();
    assert!(
        r.stdout.contains("vpc_id = ?net.vpc/network.main::vpc#id"),
        "{}",
        r.stdout
    );
    // Plan never synthesizes an id.
    assert!(
        !r.stdout.contains("net.vpc:network.main::vpc"),
        "{}",
        r.stdout
    );

    // Apply mints them per the schema, and a fresh apply is the fixture world.
    s.run(&["--file", prog, "--world", "w.json", "apply"])
        .success();
    let mut got: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    // The fixture predates the world's tick counter.
    assert_eq!(
        got.as_object_mut().unwrap().remove("tick"),
        Some(serde_json::json!(1))
    );
    let want: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("examples/world/dform.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(got, want);

    // Round 0 resolves every ref against the world: no nulls, no changes.
    let r = s
        .run(&["--file", prog, "--world", "w.json", "plan", "--show-noop"])
        .success();
    assert_eq!(
        r.summary(),
        "plan: 0 deformations, 14 no-op",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains('?'), "{}", r.stdout);
}

const SECRET_SCHEMA: &str = r#"
type_provider(db.instance, mock).
type_attr(db.instance, id, string, [computed, id]).
type_attr(db.instance, password, string, [computed, sensitive]).
type_attr(db.instance, master_password, string, [sensitive]).
type_provider(app.secret, mock).
type_attr(app.secret, id, string, [computed, id]).
"#;

const SECRET_PROG: &str = r#"
resource db.instance main { master_password = "hunter2", size = 10 }.
resource app.secret creds { value = ref(db.instance, main, password), db = ref(db.instance, main, id) }.
"#;

#[test]
fn secrets_are_labels_and_print_redacted() {
    let s = Scratch::new("computed-secret");
    s.write("schema.df", SECRET_SCHEMA);
    s.write("p.df", SECRET_PROG);
    let args = [
        "--file",
        "p.df",
        "--provider",
        "schema.df",
        "--world",
        "w.json",
    ];
    let run = |cmd: &str| s.run(&[&args[..], &[cmd]].concat()).success();

    let plan = run("plan");
    assert!(
        plan.stdout
            .contains("value = (sensitive db.instance/main#password)"),
        "{}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains("master_password = (sensitive)"),
        "{}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains("db = ?db.instance/main#id"),
        "{}",
        plan.stdout
    );

    let apply = run("apply");
    let world = s.read("w.json");
    let world_json: serde_json::Value = serde_json::from_str(&world).unwrap();
    let minted = world_json["resources"]["db.instance::main"]["computed"]["password"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(minted.starts_with("fake-secret-"), "{world}");
    // The consumer holds the label, never the bytes.
    assert_eq!(
        world_json["resources"]["app.secret::creds"]["attrs"]["value"],
        serde_json::json!({"$secret": "db.instance/main#password"})
    );
    for out in [&plan.stdout, &plan.stderr, &apply.stdout, &apply.stderr] {
        assert!(!out.contains(&minted) && !out.contains("hunter2"), "{out}");
    }

    let steady = run("plan");
    assert_eq!(
        steady.summary(),
        "stack p is undeformed",
        "{}",
        steady.stdout
    );

    s.write("p.df", &SECRET_PROG.replace("hunter2", "hunter3"));
    let changed = run("plan");
    assert!(
        changed
            .stdout
            .contains("master_password: (sensitive) -> (sensitive)"),
        "{}",
        changed.stdout
    );
    assert!(!changed.stdout.contains("hunter"), "{}", changed.stdout);
}

#[test]
fn optional_computed_is_the_programs_when_set_else_minted_at_apply() {
    let s = Scratch::new("computed-optional");
    s.write(
        "schema.df",
        "type_provider(vm, mock).\ntype_attr(vm, id, string, [computed, id]).\ntype_attr(vm, zone, string, [optional_computed]).\n",
    );
    s.write(
        "p.df",
        r#"
resource vm a { size = 1 }.
resource vm b { zone = "z1" }.
resource vm c { peer_zone = ref(vm, a, zone), other_zone = ref(vm, b, zone) }.
"#,
    );
    let args = [
        "--file",
        "p.df",
        "--provider",
        "schema.df",
        "--world",
        "w.json",
    ];
    let run = |cmd: &str| s.run(&[&args[..], &[cmd]].concat()).success();
    let plan = run("plan");
    assert!(
        plan.stdout.contains("peer_zone = ?vm/a#zone"),
        "{}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains("other_zone = \"z1\""),
        "{}",
        plan.stdout
    );
    run("apply");
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let zone = w["resources"]["vm::a"]["computed"]["zone"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        w["resources"]["vm::c"]["attrs"]["peer_zone"],
        serde_json::json!(zone)
    );
    assert!(w["resources"]["vm::b"]["computed"].get("zone").is_none());
    let steady = run("plan");
    assert_eq!(
        steady.summary(),
        "stack p is undeformed",
        "{}",
        steady.stdout
    );
}
