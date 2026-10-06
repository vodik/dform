//! The collision lint of a keyed stack (R-117): a name every deployment
//! writes the same collides, unless its provider reaches a per-deployment
//! account: one configured from the key, or from what the cloud gave a
//! resource of the deployment (a cluster's endpoint, its kubeconfig). One
//! warning per provider, listing the names.

mod common;
use common::{Run, Scratch};

const NAMES: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16", name = "main" }
resource net.vpc other { cidr = "10.1.0.0/16", name = "other" }
"#;

fn plan(providers: &str, rest: &str) -> Run {
    let s = Scratch::project("lint-isolated");
    s.write(
        "stacks/app.df",
        &format!("key env: enum(\"dev\", \"prod\") = \"dev\"\n{providers}\n{NAMES}{rest}"),
    );
    s.run(&["plan", "app"]).success()
}

fn warnings(r: &Run) -> Vec<&str> {
    r.stderr
        .lines()
        .filter(|l| l.contains("does not depend") || l.contains("do not depend"))
        .collect()
}

#[test]
fn a_constant_provider_warns_once_listing_its_names() {
    let r = plan("use fake", "");
    let w = warnings(&r);
    assert_eq!(w.len(), 1, "{}", r.stderr);
    for name in [
        "net.vpc[\"main\"].name = \"main\"",
        "net.vpc[\"other\"].name = \"other\"",
        "2 names do not depend on the stack's key (env)",
    ] {
        assert!(w[0].contains(name), "{name}: {}", w[0]);
    }
}

#[test]
fn a_provider_configured_from_the_key_is_isolated() {
    let r = plan("use fake { region = \"r-${env}\" }", "");
    assert_eq!(warnings(&r), Vec::<&str>::new(), "{}", r.stderr);
}

/// The ovh-infra shape: Kubernetes configured from what a resource of
/// the deployment answers, its cluster's endpoint.
#[test]
fn a_provider_configured_from_a_computed_attribute_is_isolated() {
    let r = plan(
        "use fake\nuse k8s { kubeconfig = cluster.api_endpoint }",
        "resource k8s.cluster cluster { name = \"c-${env}\" }\n\
         resource k8s.namespace web { metadata.name = \"web\" }\n",
    );
    let w = warnings(&r);
    assert_eq!(w.len(), 1, "{}", r.stderr);
    assert!(!w[0].contains("k8s.namespace"), "{}", w[0]);
    assert!(w[0].contains("net.vpc[\"main\"]"), "{}", w[0]);
}
