//! A write whose type is a variable is partitioned per concrete type
//! (R-116): a policy over `r in k8s` writing a leaf of `metadata`, beside
//! a resource whose object write reads another type's `metadata.name`,
//! is one rule per type the variable can be, so the ConfigMap's
//! `metadata` reads the Namespace's and nothing of its own.

mod common;
use common::Scratch;

/// The shape of ~/src/ovh-infra's baseline and traefik.df, on the mock.
fn project(policy: &str) -> Scratch {
    let s = Scratch::project("partition-types");
    s.write(
        "stacks/lab.df",
        &format!(
            r#"
provider k8s

{policy}

resource k8s.namespace traefik {{ metadata.name = "traefik" }}

resource k8s.config_map settings {{
  metadata = {{ name: "traefik", namespace: traefik.metadata.name }}
  data = {{ MODE: "lab" }}
}}
"#
        ),
    );
    s
}

#[test]
fn a_policy_over_a_namespace_labels_a_block_that_reads_another_type() {
    let s = project(r#"set r.metadata.labels.owner = "simon" @default where r in k8s"#);
    let r = s.run(&["plan", "lab"]).success();
    assert_eq!(
        r.stdout
            .matches("metadata.labels.owner = \"simon\"")
            .count(),
        2,
        "{}",
        r.stdout
    );
    let r = s.run(&["dev", "strata", "lab"]).success();
    assert!(
        r.stdout.contains("(arg, k8s.config_map, metadata)")
            && r.stdout.contains("(arg, k8s.namespace, metadata)")
            && !r.stdout.contains("(arg, *, metadata)"),
        "{}",
        r.stdout
    );
}

/// `r in resource` is every type the program wants.
#[test]
fn a_policy_over_every_resource_is_per_type_too() {
    let s = project(
        r#"set r.metadata.annotations.team = "platform" @default where r in resource, has r.metadata"#,
    );
    let r = s.run(&["plan", "lab"]).success();
    assert_eq!(
        r.stdout
            .matches("metadata.annotations.team = \"platform\"")
            .count(),
        2,
        "{}",
        r.stdout
    );
}

/// A rule reading the cell it writes is still a cycle per type.
#[test]
fn a_policy_reading_what_it_writes_is_still_a_cycle() {
    let s = project(
        r#"set r.metadata.labels.owner = "simon" where r in k8s, not has r.metadata.labels.team"#,
    );
    let r = s.run(&["plan", "lab"]).failure();
    assert!(
        r.stderr.contains("program is not stratifiable"),
        "{}",
        r.stderr
    );
}
