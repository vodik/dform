//! Lifecycle as facts: `lifecycle(r, prevent_destroy)`, `moved(T, Old, r)`
//! and `ignore_changes(r, Path)` are plain facts the planner reads, `r` a
//! resource reference (R-42).

mod common;
use common::{Scratch, mock};

const NET: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
lifecycle(main, "prevent_destroy")
use fake
"#;

/// prevent_destroy turns a delete, or a replace, into a deny: plan and
/// apply are blocked and the world keeps the object.
#[test]
fn prevent_destroy_makes_a_delete_a_deny() {
    let s = Scratch::new("prevent-destroy");
    s.write("p.df", NET);
    mock(&s, &["apply"]).success();
    // The resource goes; the fact stays, naming it by its address.
    s.write(
        "p.df",
        "\nlifecycle(net.vpc[\"main\"], \"prevent_destroy\")\nuse fake\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(r.stdout.contains("- net.vpc main"), "{}", r.stdout);
    // The deny is the plan's `denied` section's, said once (R-111).
    assert!(
        r.stdout.contains(
            "\ndenied\n  lifecycle prevent_destroy: the plan would delete net.vpc[\"main\"]  "
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("prevent_destroy"), "{}", r.stderr);
    let r = mock(&s, &["apply"]).failure();
    assert!(
        r.stderr.contains("apply: refused  1 deny\n"),
        "{}",
        r.stderr
    );
    assert!(s.json("w.json")["resources"].get("net.vpc::main").is_some());
    // A force_new change would replace it: also a deny.
    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = mock(&s, &["apply"]).failure();
    assert!(
        r.stderr
            .contains("- lifecycle prevent_destroy: the plan would replace net.vpc[\"main\"]\n"),
        "{}",
        r.stderr
    );
    assert_eq!(
        s.json("w.json")["resources"]["net.vpc::main"]["attrs"]["cidr"],
        "10.0.0.0/16"
    );
}

/// Renaming a copy of a component renames every address under it. With a
/// moved fact per resource the plan is up to date: state's identity moves,
/// nothing is destroyed or created, and a second apply has nothing to move.
#[test]
fn moved_closes_rename_is_destroy() {
    let s = Scratch::new("moved");
    let prog = |inst: &str| {
        format!(
            r#"

component network {{
  resource net.vpc vpc {{ cidr = "10.0.0.0/16" }}
  resource net.subnet a {{ vpc, tier = "web" }}
}}
resource network {inst} {{}}
use fake
"#
        )
    };
    s.write("p.df", &prog("main"));
    mock(&s, &["apply"]).success();
    let before = s.json("w.json");

    // Without moved the rename is two creates and two deletes.
    s.write("p.df", &prog("core"));
    let r = mock(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 4 changes (2 create, 2 delete) over 1 tick",
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        &format!(
            "{}moved(net.vpc, \"main.vpc\", net.vpc[\"core.vpc\"])\n\
             moved(net.subnet, \"main.a\", net.subnet[\"core.a\"])\n",
            prog("core")
        ),
    );
    let r = mock(&s, &["plan"]).success();
    assert_eq!(
        r.stdout,
        "moved net.subnet[\"main.a\"] -> net.subnet[\"core.a\"]\n\
         moved net.vpc[\"main.vpc\"] -> net.vpc[\"core.vpc\"]\n\
         stack p is up to date\n"
    );
    // plan does not write state; apply does.
    let r = mock(&s, &["apply"]).success();
    assert!(r.stdout.ends_with("apply: nothing to do\n"), "{}", r.stdout);
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert_eq!(
        st["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["net.subnet::core.a", "net.vpc::core.vpc"]
    );
    assert_eq!(st["resources"]["net.vpc::core.vpc"]["remote"], "main.vpc");
    assert_eq!(s.json("w.json")["resources"], before["resources"]);
    let r = mock(&s, &["plan"]).success();
    assert_eq!(r.stdout, "stack p is up to date\n");
}

/// ignore_changes drops the path from both sides: a value the world has
/// there is not a change, and an update keeps it.
#[test]
fn ignore_changes_drops_the_path_from_both_sides() {
    let s = Scratch::new("ignore-changes");
    let prog = |team: &str| {
        format!(
            "\nresource net.vpc main {{ cidr = \"10.0.0.0/16\", tags = {{ team: \"{team}\" }} }}\n\
             ignore_changes(main, \"tags.owner\")\nuse fake\n"
        )
    };
    s.write("p.df", &prog("a"));
    mock(
        &s,
        &[
            "apply",
            "--chaos",
            r#"mutate=net.vpc["main"].tags.owner="ops""#,
        ],
    )
    .success();
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
    s.write("p.df", &prog("b"));
    let r = mock(&s, &["apply", "--why=none"]).success();
    assert!(
        r.stdout
            .contains("~ net.vpc[\"main\"]\n  tags.team: \"a\" -> \"b\"\napply order:\n  tick 1\n    net.vpc[\"main\"]\napply: complete\n"),
        "{}",
        r.stdout
    );
    assert_eq!(
        s.json("w.json")["resources"]["net.vpc::main"]["attrs"]["tags"],
        serde_json::json!({"owner": "ops", "team": "b"})
    );
}

/// The facts are ordinary: a policy reads them, also from a module the
/// program uses (they are the compiler's relation, never the module's).
#[test]
fn policy_reads_lifecycle_facts() {
    let s = Scratch::new("lifecycle-policy");
    s.write(
        "p.df",
        r#"

resource db.postgres main { size = 1 }
deny "databases must be protected" {addr: a} where {
  a in db.postgres
  not lifecycle(a, "prevent_destroy")
}
use fake
"#,
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("databases must be protected"),
        "{}",
        r.stderr
    );
    s.write("lib.df", "\nlifecycle(main, \"prevent_destroy\")\n");
    s.write(
        "p.df",
        &s.read("p.df")
            .replacen("use fake\n", "use fake\nuse lib\n", 1),
    );
    mock(&s, &["plan"]).success();
}

/// ignore_changes ignores changes to an object that exists: a create sets
/// the path, and a later change to it in the program is not a change.
#[test]
fn ignore_changes_still_sets_the_path_on_create() {
    let s = Scratch::new("ignore-changes-create");
    let prog = |owner: &str| {
        format!(
            "\nresource net.vpc main {{ cidr = \"10.0.0.0/16\", tags = {{ owner: \"{owner}\" }} }}\n\
             ignore_changes(main, \"tags.owner\")\nuse fake\n"
        )
    };
    s.write("p.df", &prog("ops"));
    let r = mock(&s, &["plan", "--why=none"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"main\"]\n  cidr = \"10.0.0.0/16\"\n  tags.owner = \"ops\"\n"),
        "{}",
        r.stdout
    );
    mock(&s, &["apply"]).success();
    assert_eq!(
        s.json("w.json")["resources"]["net.vpc::main"]["attrs"]["tags"]["owner"],
        "ops"
    );
    s.write("p.df", &prog("dev"));
    let r = mock(&s, &["plan"]).success();
    assert_eq!(r.stdout, "stack p is up to date\n");
}

/// An update does not set an ignored path the world does not have.
#[test]
fn ignore_changes_update_leaves_an_absent_path_absent() {
    let s = Scratch::new("ignore-changes-absent");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\", size = 1 }\nuse fake\n",
    );
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\", size = 2, tags = { owner: \"ops\" } }\nignore_changes(main, \"tags.owner\")\nuse fake\n",
    );
    let r = mock(&s, &["apply", "--why=none"]).success();
    assert!(
        r.stdout.contains("~ net.vpc[\"main\"]\n  size: 1 -> 2\n"),
        "{}",
        r.stdout
    );
    assert!(
        s.json("w.json")["resources"]["net.vpc::main"]["attrs"]
            .get("tags")
            .is_none(),
        "{}",
        s.read("w.json")
    );
}

/// prevent_destroy is a deny the evaluator derives from lifecycle/2 and the
/// plan's deformation/3, so `why` explains it; both print the resource as
/// its address.
#[test]
fn why_explains_prevent_destroy() {
    let s = Scratch::new("prevent-destroy-why");
    s.write("p.df", NET);
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        "\nlifecycle(net.vpc[\"main\"], \"prevent_destroy\")\nuse fake\n",
    );
    let r = mock(&s, &["why", "deny(M)"]).success();
    assert!(
        r.stdout.starts_with(
            "deny \"lifecycle prevent_destroy: the plan would delete net.vpc[\\\"main\\\"]\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("lifecycle(net.vpc main, \"prevent_destroy\")"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("deformation(\"delete\", net.vpc main, "),
        "{}",
        r.stdout
    );
}

/// A policy reads the deformation like any fact; its resource is a
/// reference, which a message prints as its address.
#[test]
fn policy_reads_the_deformation() {
    let s = Scratch::new("policy-deformation");
    s.write("p.df", NET);
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        "\nresource compute.vm keep { size = 1 }\ndeny(m) where deformation(\"delete\", r, _), m = \"no deletes here: ${r}\"\nuse fake\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stdout
            .contains("\ndenied\n  no deletes here: net.vpc[\"main\"]  net.vpc main    p.df:3\n"),
        "{}",
        r.stdout
    );
    let r = mock(&s, &["query", "deformation(K, R, _)"]).success();
    assert!(
        r.stdout.contains("\"create\"  compute.vm keep")
            && r.stdout.contains("\"delete\"  net.vpc main"),
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
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nresource net.vpc shadow {\n  cidr = \"10.1.0.0/16\"\n} where deformation(\"create\", main, _)\nuse fake\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr
            .contains("a resource rule reads deformation/3 or world_digest/2"),
        "{}",
        r.stderr
    );
}

/// The plan's rows and the lifecycle facts take a resource reference
/// (R-42): a rule binds it with `in`, which also reads its attributes, and
/// compares it with a resource by `==`; a message prints it as its
/// address, typed by `in` or not.
#[test]
fn a_deformation_row_is_a_reference() {
    let s = Scratch::new("deformation-reference");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc other { cidr = "10.1.0.0/16" }
resource compute.vm vm { size = 1 }
deny "a wide vpc: ${v}" where deformation(_, v, _), v in net.vpc, v.cidr == "10.1.0.0/16"
deny "main changes: ${r}" where deformation("create", r, _), r == main
deny "not main: ${r}" where deformation(_, r, _), r != main, r != net.vpc["gone"]
use fake
"#,
    );
    let r = mock(&s, &["plan"]).failure();
    for line in [
        "\n  a wide vpc: net.vpc[\"other\"]   net.vpc other ",
        "\n  main changes: net.vpc[\"main\"]  net.vpc main ",
        "\n  not main: compute.vm[\"vm\"]     compute.vm vm ",
        "\n  not main: net.vpc[\"other\"]     net.vpc other ",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
    let r = mock(&s, &["query", "deformation(K, R, _)"]).success();
    assert!(
        r.stdout.contains("\"create\"  net.vpc other"),
        "{}",
        r.stdout
    );
}

/// A lifecycle fact in the old (type, address) shape, or with an address
/// as text, is an error that names the shape.
#[test]
fn a_lifecycle_fact_takes_a_resource_not_text() {
    let s = Scratch::new("lifecycle-text");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nlifecycle(net.vpc, \"main\", \"prevent_destroy\")\nadopt(\"main\", \"vpc-1\")\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`lifecycle` takes 2 arguments: `lifecycle(resource, \"prevent_destroy\")` (R-42)"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("a resource here, not a value: its name in scope, `T[\"a\"]`, or a variable"),
        "{}",
        r.stderr
    );
}
