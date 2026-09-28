//! Lifecycle as facts: `lifecycle(T, A, prevent_destroy)`, `moved(T, Old,
//! New)` and `ignore_changes(T, A, Path)` are plain facts the planner reads.

mod common;
use common::Scratch;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

fn world(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.json")).unwrap()
}

const NET: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
lifecycle(net.vpc, main, prevent_destroy).
"#;

/// prevent_destroy turns a delete, or a replace, into a deny: plan and
/// apply are blocked and the world keeps the object.
#[test]
fn prevent_destroy_makes_a_delete_a_deny() {
    let s = Scratch::new("prevent-destroy");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    // The resource goes; the fact stays.
    s.write("p.df", "lifecycle(net.vpc, main, prevent_destroy).\n");
    let r = dform(&s, &["plan"]).failure();
    assert!(r.stdout.contains("- net.vpc.main"), "{}", r.stdout);
    assert!(
        r.stderr.contains(
            "constraint violations:\n- lifecycle prevent_destroy: the plan would delete net.vpc.main\n"
        ),
        "{}",
        r.stderr
    );
    let r = dform(&s, &["apply"]).failure();
    assert!(
        r.stderr
            .contains("apply stopped at tick 1: blocked by constraints"),
        "{}",
        r.stderr
    );
    assert!(world(&s)["resources"].get("net.vpc::main").is_some());
    // A force_new change would replace it: also a deny.
    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = dform(&s, &["apply"]).failure();
    assert!(
        r.stderr
            .contains("- lifecycle prevent_destroy: the plan would replace net.vpc.main\n"),
        "{}",
        r.stderr
    );
    assert_eq!(
        world(&s)["resources"]["net.vpc::main"]["attrs"]["cidr"],
        "10.0.0.0/16"
    );
}

/// Renaming a component instance renames every address under it. With a
/// moved fact per resource the plan is undeformed: state's identity moves,
/// nothing is destroyed or created, and a second apply has nothing to move.
#[test]
fn moved_closes_rename_is_destroy() {
    let s = Scratch::new("moved");
    let prog = |inst: &str| {
        format!(
            r#"
component network {inst} {{
  resource net.vpc vpc {{ cidr = "10.0.0.0/16" }}.
  resource net.subnet a {{ vpc_id = ref(net.vpc, vpc, id), tier = "web" }}.
}}.
"#
        )
    };
    s.write("p.df", &prog("main"));
    dform(&s, &["apply"]).success();
    let before = world(&s);

    // Without moved/3 the rename is two creates and two deletes.
    s.write("p.df", &prog("core"));
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 2 to create, 0 to update, 2 to delete",
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        &format!(
            "{}moved(net.vpc, \"network.main::vpc\", \"network.core::vpc\").\n\
             moved(net.subnet, \"network.main::a\", \"network.core::a\").\n",
            prog("core")
        ),
    );
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.stdout,
        "moved net.subnet.network.main::a -> net.subnet.network.core::a\n\
         moved net.vpc.network.main::vpc -> net.vpc.network.core::vpc\n\
         plan: 0 to create, 0 to update, 0 to delete\n\
         stack p is undeformed\n"
    );
    // plan does not write state; apply does.
    let r = dform(&s, &["apply"]).success();
    assert!(r.stdout.ends_with("apply: nothing to do\n"), "{}", r.stdout);
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert_eq!(
        st["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["net.subnet::network.core::a", "net.vpc::network.core::vpc"]
    );
    assert_eq!(
        st["resources"]["net.vpc::network.core::vpc"]["remote"],
        "network.main::vpc"
    );
    assert_eq!(world(&s)["resources"], before["resources"]);
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.stdout,
        "plan: 0 to create, 0 to update, 0 to delete\nstack p is undeformed\n"
    );
}

/// ignore_changes drops the path from both sides: a value the world has
/// there is not a change, and an update keeps it.
#[test]
fn ignore_changes_drops_the_path_from_both_sides() {
    let s = Scratch::new("ignore-changes");
    let prog = |team: &str| {
        format!(
            "resource net.vpc main {{ cidr = \"10.0.0.0/16\", tags = {{ team: \"{team}\" }} }}.\n\
             ignore_changes(net.vpc, main, \"tags.owner\").\n"
        )
    };
    s.write("p.df", &prog("a"));
    dform(
        &s,
        &[
            "apply",
            "--chaos",
            r#"mutate=net.vpc/main:tags.owner="ops""#,
        ],
    )
    .success();
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
    s.write("p.df", &prog("b"));
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout
            .contains("~ net.vpc.main\n  tags.team: \"a\" -> \"b\"\napply: complete\n"),
        "{}",
        r.stdout
    );
    assert_eq!(
        world(&s)["resources"]["net.vpc::main"]["attrs"]["tags"],
        serde_json::json!({"owner": "ops", "team": "b"})
    );
}

/// The facts are ordinary: a policy reads them, also through `import as`,
/// which does not namespace them.
#[test]
fn policy_reads_lifecycle_facts() {
    let s = Scratch::new("lifecycle-policy");
    s.write(
        "p.df",
        r#"
resource db.postgres main { size = 1 }.
deny("databases must be protected", {addr: A}) :-
  want(db.postgres, A), not lifecycle(db.postgres, A, prevent_destroy).
"#,
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("databases must be protected"),
        "{}",
        r.stderr
    );
    s.write("lib.df", "lifecycle(db.postgres, main, prevent_destroy).\n");
    s.write(
        "p.df",
        &format!("import \"lib.df\" as lib.\n{}", s.read("p.df")),
    );
    dform(&s, &["plan"]).success();
}
