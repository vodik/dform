//! A type and a provider's data source may share a name (R-196, decided
//! 2026-10-09), and the two spellings keep their meanings: `x in T` is the
//! program's own resources of type `T`, `T(..)` the rows the provider
//! lists, managed or not. A policy over `w in k8s.deployment` never ranges
//! over what else the cluster runs, though the provider lists it.

mod common;
use common::{Scratch, repo};

#[test]
fn membership_is_the_programs_and_the_relation_is_the_providers() {
    let s = Scratch::project("type-table-names");
    s.write(
        "providers/k8s/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/k8s.df")).unwrap()
            + "\nextern_decl(\"k8s.deployment\", \"+namespace, -name, -image\")\n"),
    );
    // The cluster runs the program's deployment and one it does not manage.
    s.write(
        "providers/k8s/externs.df",
        "k8s.deployment(\"apps\", \"api\", \"api:1\")\n\
         k8s.deployment(\"apps\", \"legacy\", \"legacy:latest\")\n",
    );
    s.write(
        "main.df",
        r#"
use k8s

resource k8s.deployment api {
  metadata.name = "api"
  metadata.namespace = "apps"
  spec.selector.matchLabels = {app: "api"}
  spec.template.metadata.labels = {app: "api"}
  spec.template.spec.containers = [{name: "app", image: "api:1"}]
}

deny "the program runs ${w.metadata.name}" where w in k8s.deployment
deny "the cluster runs ${n} at ${i}" where k8s.deployment("apps", n, i), n != "api"
"#,
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("- the program runs api\n")
            && r.stderr.contains("- the cluster runs legacy at legacy:latest\n")
            && !r.stderr.contains("the program runs legacy"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}
