//! `has r.PATH` in a rule that writes under PATH is answered from the
//! schema when PATH is a configurable attribute of the type (R-106): a
//! baseline over every resource that writes `metadata.labels.owner`
//! guarded by `has r.metadata` does not read what it writes. Everywhere
//! else, and for an attribute no schema declares, `has` stays a value test.

mod common;
use common::Scratch;

fn project(stack: &str) -> Scratch {
    let s = Scratch::project("has-schema");
    s.write("stacks/p.df", stack);
    s
}

/// The baseline of ~/src/ovh-infra over `resource` rather than `k8s`: the
/// Kubernetes objects get the label, the database (whose schema has no
/// `metadata`) does not.
#[test]
fn a_baseline_over_every_resource_guarded_by_has_stratifies() {
    let s = project(
        r#"
use fake
use k8s
resource k8s.namespace ns { metadata.name = "traefik" }
resource k8s.config_map cm { metadata.name = "cm", data = { a: "1" } }
resource db.postgres db { name = "db" }
set r.metadata.labels.owner = "simon" @default where r in resource, has r.metadata
"#,
    );
    let r = s.run(&["plan", "p"]).success();
    assert_eq!(
        r.stdout
            .matches("metadata.labels.owner = \"simon\"")
            .count(),
        2,
        "{}",
        r.stdout
    );
    let db = r.stdout.split("db.postgres db").nth(1).unwrap();
    let db = db.split("\n  +").next().unwrap();
    assert!(!db.contains("metadata"), "{}", r.stdout);
}

/// An attribute no schema declares (the fake provider accepts any) and a
/// computed one are value tests: `has` is whether the value is there.
#[test]
fn has_of_an_undeclared_attribute_is_a_value_test() {
    let s = project(
        r#"
use fake
resource db.postgres tagged { name = "a", tags = { team: "x" } }
resource db.postgres bare { name = "b" }
deny "tagged" { r: r } where r in db.postgres, has r.tags.team
"#,
    );
    let r = s.run(&["plan", "p"]).failure();
    assert!(
        r.stderr
            .contains("refused  tagged  stacks/p.df:5\n  └─ r = \"tagged\"\n"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("\"r\":\"bare\""), "{}", r.stderr);
}

/// A rule that does not write under the path tests the value: the
/// database sets no `metadata`, the namespace does.
#[test]
fn not_has_in_a_rule_that_does_not_write_it_tests_the_value() {
    let s = project(
        r#"
use fake
use k8s
resource k8s.namespace ns { metadata.name = "traefik" }
resource db.postgres db { name = "db" }
deny "no metadata" { r: r } where r in resource, not has r.metadata
"#,
    );
    let r = s.run(&["plan", "p"]).failure();
    assert!(
        r.stderr
            .contains("refused  no metadata  stacks/p.df:6\n  └─ r = \"db\"\n"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("\"r\":\"ns\""), "{}", r.stderr);
}
