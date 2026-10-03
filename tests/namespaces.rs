//! A provider's namespace as a range (R-49): `r in k8s` is a resource of
//! any type the `k8s` provider serves, so a policy covers a whole provider
//! and nothing else; `r in resource` stays every resource. The error for
//! an attribute some type of the namespace lacks has a case in
//! tests/syntax/err/membership.df.

mod common;
use common::Scratch;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on("p.df", &["--world", "w.json"], args))
}

/// A label on every Kubernetes object, none on the fake cloud's (whose
/// mock types include a `k8s.cluster` it serves itself, not `k8s`'s).
#[test]
fn a_set_over_a_namespace_reaches_its_types_only() {
    let s = Scratch::new("ns-set");
    s.write(
        "p.df",
        "edition 2026\n\nprovider fake\nprovider k8s\n\n\
         resource net.vpc v { cidr = \"10.0.0.0/16\" }\n\
         resource k8s.cluster c { name = \"c\" }\n\
         resource k8s.namespace ns { metadata.name = \"ns\" }\n\
         resource k8s.config_map cfg {\n  metadata.name = \"cfg\"\n  metadata.namespace = \"ns\"\n}\n\n\
         set r.metadata.labels.owner = \"ops\" where r in k8s\n\
         every(r) where r in resource\n",
    );
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "+ k8s.config_map[\"cfg\"]\n  metadata.labels.owner = \"ops\"\n  metadata.name = \"cfg\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ k8s.namespace[\"ns\"]\n  metadata.labels.owner = \"ops\"\n"),
        "{}",
        r.stdout
    );
    let cluster = r
        .stdout
        .split("+ k8s.cluster[\"c\"]\n")
        .nth(1)
        .unwrap_or_default();
    let cluster: Vec<&str> = cluster
        .lines()
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert_eq!(cluster, ["  name = \"c\""], "{}", r.stdout);
    assert_eq!(r.stdout.matches("labels.owner").count(), 2, "{}", r.stdout);
    let q = dform(&s, &["query", "every(r)"]).success();
    assert_eq!(q.stdout.lines().count(), 5, "{}", q.stdout);
}

/// The plan's deformation rows bind the same way: a deleted Kubernetes
/// object is in `k8s`, a deleted VPC is not.
#[test]
fn a_deformation_row_binds_through_the_namespace() {
    let s = Scratch::new("ns-deformation");
    s.write(
        "p.df",
        "edition 2026\n\nprovider fake\nprovider k8s\n\n\
         resource net.vpc v { cidr = \"10.0.0.0/16\" }\n\
         resource k8s.namespace ns { metadata.name = \"ns\" }\n",
    );
    dform(&s, &["apply"]).success();
    s.write(
        "p.df",
        "edition 2026\n\nprovider fake\nprovider k8s\n\n\
         deny \"a k8s delete: ${r}\" where deformation(\"delete\", r, _), r in k8s\n",
    );
    let r = dform(&s, &["plan"]).failure();
    assert!(
        r.stdout
            .contains("denied:\n! a k8s delete: k8s.namespace[\"ns\"]\n"),
        "{}",
        r.stdout
    );
    assert_eq!(r.stdout.matches("a k8s delete").count(), 1, "{}", r.stdout);
}
