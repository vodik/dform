//! Lifecycle as facts: `lifecycle(T, A, prevent_destroy)`, `moved(T, Old,
//! New)` and `ignore_changes(T, A, Path)` are plain facts the planner reads.

mod common;
use common::Scratch;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on("p.df", &["--world", "w.json"], args))
}

fn world(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.json")).unwrap()
}

const NET: &str = r#"edition 2027

resource net.vpc main { cidr = "10.0.0.0/16" }
lifecycle(net.vpc, "main", "prevent_destroy")
"#;

/// prevent_destroy turns a delete, or a replace, into a deny: plan and
/// apply are blocked and the world keeps the object.
#[test]
fn prevent_destroy_makes_a_delete_a_deny() {
    let s = Scratch::new("prevent-destroy");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    // The resource goes; the fact stays.
    s.write(
        "p.df",
        "edition 2027\nlifecycle(net.vpc, \"main\", \"prevent_destroy\")\n",
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(r.stdout.contains("- net.vpc[\"main\"]"), "{}", r.stdout);
    assert!(
        r.stderr.contains(
            "constraint violations:\n- lifecycle prevent_destroy: the plan would delete net.vpc[\"main\"]\n"
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
            .contains("- lifecycle prevent_destroy: the plan would replace net.vpc[\"main\"]\n"),
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
            r#"edition 2027

module network {{
  resource net.vpc vpc {{ cidr = "10.0.0.0/16" }}
  resource net.subnet a {{ vpc_id = vpc.id, tier = "web" }}
}}
instance network {inst} {{}}
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
        "plan: 4 deformations (2 create, 2 delete)",
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        &format!(
            "{}moved(net.vpc, \"network.main::vpc\", \"network.core::vpc\")\n\
             moved(net.subnet, \"network.main::a\", \"network.core::a\")\n",
            prog("core")
        ),
    );
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.stdout,
        "moved net.subnet[\"network.main::a\"] -> net.subnet[\"network.core::a\"]\n\
         moved net.vpc[\"network.main::vpc\"] -> net.vpc[\"network.core::vpc\"]\n\
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
    assert_eq!(r.stdout, "stack p is undeformed\n");
}

/// ignore_changes drops the path from both sides: a value the world has
/// there is not a change, and an update keeps it.
#[test]
fn ignore_changes_drops_the_path_from_both_sides() {
    let s = Scratch::new("ignore-changes");
    let prog = |team: &str| {
        format!(
            "edition 2027\nresource net.vpc main {{ cidr = \"10.0.0.0/16\", tags = {{ team: \"{team}\" }} }}\n\
             ignore_changes(net.vpc, \"main\", \"tags.owner\")\n"
        )
    };
    s.write("p.df", &prog("a"));
    dform(
        &s,
        &[
            "apply",
            "--chaos",
            r#"mutate=net.vpc["main"].tags.owner="ops""#,
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
            .contains("~ net.vpc[\"main\"]\n  tags.team: \"a\" -> \"b\"\napply order:\n  tick 1\n    net.vpc[\"main\"]\napply: complete\n"),
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
        r#"edition 2027

resource db.postgres main { size = 1 }
deny "databases must be protected" {addr: a} if {
  a in db.postgres
  not lifecycle(db.postgres, a, "prevent_destroy")
}
"#,
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("databases must be protected"),
        "{}",
        r.stderr
    );
    s.write(
        "lib.df",
        "edition 2027\nlifecycle(\"db.postgres\", \"main\", \"prevent_destroy\")\n",
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replacen("edition 2027\n", "edition 2027\nimport \"lib.df\"\n", 1),
    );
    dform(&s, &["plan"]).success();
}

/// ignore_changes ignores changes to an object that exists: a create sets
/// the path, and a later change to it in the program is not a change.
#[test]
fn ignore_changes_still_sets_the_path_on_create() {
    let s = Scratch::new("ignore-changes-create");
    let prog = |owner: &str| {
        format!(
            "edition 2027\nresource net.vpc main {{ cidr = \"10.0.0.0/16\", tags = {{ owner: \"{owner}\" }} }}\n\
             ignore_changes(net.vpc, \"main\", \"tags.owner\")\n"
        )
    };
    s.write("p.df", &prog("ops"));
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"main\"]\n  cidr = \"10.0.0.0/16\"\n  tags.owner = \"ops\"\n"),
        "{}",
        r.stdout
    );
    dform(&s, &["apply"]).success();
    assert_eq!(
        world(&s)["resources"]["net.vpc::main"]["attrs"]["tags"]["owner"],
        "ops"
    );
    s.write("p.df", &prog("dev"));
    let r = dform(&s, &["plan"]).success();
    assert_eq!(r.stdout, "stack p is undeformed\n");
}

/// An update does not set an ignored path the world does not have.
#[test]
fn ignore_changes_update_leaves_an_absent_path_absent() {
    let s = Scratch::new("ignore-changes-absent");
    s.write(
        "p.df",
        "edition 2027\nresource net.vpc main { cidr = \"10.0.0.0/16\", size = 1 }\n",
    );
    dform(&s, &["apply"]).success();
    s.write(
        "p.df",
        "edition 2027\nresource net.vpc main { cidr = \"10.0.0.0/16\", size = 2, tags = { owner: \"ops\" } }\nignore_changes(net.vpc, \"main\", \"tags.owner\")\n",
    );
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout.contains("~ net.vpc[\"main\"]\n  size: 1 -> 2\n"),
        "{}",
        r.stdout
    );
    assert!(
        world(&s)["resources"]["net.vpc::main"]["attrs"]
            .get("tags")
            .is_none(),
        "{}",
        s.read("w.json")
    );
}

/// prevent_destroy is a deny the evaluator derives from lifecycle/3 and the
/// plan's deformation/4, so `why` explains it.
#[test]
fn why_explains_prevent_destroy() {
    let s = Scratch::new("prevent-destroy-why");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    s.write(
        "p.df",
        "edition 2027\nlifecycle(net.vpc, \"main\", \"prevent_destroy\")\n",
    );
    let r = dform(&s, &["why", "deny(M)"]).success();
    assert!(
        r.stdout.starts_with(
            "deny(\"lifecycle prevent_destroy: the plan would delete net.vpc[\\\"main\\\"]\")\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("lifecycle(\"net.vpc\", \"main\", \"prevent_destroy\")"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("deformation(\"delete\", \"net.vpc\", \"main\", "),
        "{}",
        r.stdout
    );
}

/// A policy reads the deformation like any fact.
#[test]
fn policy_reads_the_deformation() {
    let s = Scratch::new("policy-deformation");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    s.write(
        "p.df",
        "edition 2027\nresource compute.vm keep { size = 1 }\ndeny(m) if deformation(\"delete\", t, a, _), m = format(\"no deletes here: %s.%s\", t, a)\n",
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(
        r.stdout.contains("denied:\n") && r.stdout.contains("no deletes here: net.vpc.main"),
        "{}",
        r.stdout
    );
    let r = dform(&s, &["query", "deformation(K, T, A, _)"]).success();
    assert!(
        r.stdout.contains("\"create\"") && r.stdout.contains("\"delete\""),
        "{}",
        r.stdout
    );
}

/// Only policy may read the deformation: a resource rule over it would make
/// the plan depend on itself.
#[test]
fn a_resource_rule_over_the_deformation_is_an_error() {
    let s = Scratch::new("deformation-circular");
    s.write(
        "p.df",
        "edition 2027\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nresource net.vpc shadow {\n  if deformation(\"create\", \"net.vpc\", \"main\", _)\n  cidr = \"10.1.0.0/16\"\n}\n",
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(
        r.stderr
            .contains("a resource rule reads deformation/4 or world_digest/3"),
        "{}",
        r.stderr
    );
}
