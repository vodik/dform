//! A provider the program configures, whose settings this plan does not
//! know (a kubeconfig read over SSH from a server not created yet), plans
//! nothing of its own: every resource it serves is under `later`, waiting
//! on it (R-110). The Kubernetes provider serves the stable kinds without
//! a cluster (its static schema), so they are typed there; a kind only a
//! cluster serves (a CRD) waits on the provider for its schema.

mod common;
use common::{Run, Scratch};

/// The k3s shape of ~/src/ovh-infra: the kubeconfig is read from the
/// server once it is up; the Traefik module's objects are the cluster's.
const STACK: &str = r#"
use fake
resource db.postgres server { name = "server" }
let raw: secret(string) = text("ssh://ubuntu@${server.endpoint}/etc/rancher/k3s/k3s.yaml")
use k8s { source = "./providers/k8s", kubeconfig = raw }
use traefik
"#;

const TRAEFIK: &str = r#"
resource k8s.namespace ns { metadata.name = "traefik" }
resource k8s.storage_class block { metadata.name = "block", provisioner = "rancher.io/local-path" }
resource k8s.deployment web {
  metadata.name = "web"
  metadata.namespace = ns.metadata.name
}
resource k8s.traefik.io.v1alpha1.middleware strip {
  metadata.name = "strip"
  spec.stripPrefix.prefixes = ["/a"]
}
"#;

fn project() -> Scratch {
    let s = Scratch::project("provider-waits");
    s.write("stacks/p.df", STACK);
    s.write("traefik.df", TRAEFIK);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> Run {
    let out = common::dform()
        .args(common::yes(args))
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .output()
        .unwrap();
    Run::from(out)
}

#[test]
fn a_providers_resources_wait_on_its_settings_under_later() {
    let s = project();
    let r = dform(&s, &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick, 4 later",
        "{}",
        r.stdout
    );
    let (ticks, later) = r.stdout.split_once("\nlater").unwrap();
    assert!(!ticks.contains("k8s."), "{}", r.stdout);
    for line in [
        "  waits on  provider k8s  kubeconfig = raw",
        "  + k8s.namespace traefik.ns",
        // The static schema's kind, by its short name, typed.
        "  + k8s.storage_class traefik.block",
        "      provisioner = \"rancher.io/local-path\"",
        "  + k8s.deployment traefik.web",
        "  waits on  provider k8s  schema",
        "  + k8s.traefik.io.v1alpha1.middleware traefik.strip",
        "      spec.stripPrefix.prefixes[0] = \"/a\"",
    ] {
        assert!(later.contains(line), "{line}\n{}", r.stdout);
    }
}

#[test]
fn why_names_the_provider_a_resource_waits_on() {
    let s = project();
    let r = dform(&s, &["why", "k8s.storage_class[\"traefik.block\"]", "p"]).success();
    assert!(
        r.stdout
            .starts_with("k8s.storage_class traefik.block  traefik.df:3")
            && r.stdout
                .ends_with("\nlater  waits on  provider k8s  kubeconfig = raw\n"),
        "{}",
        r.stdout
    );
}

/// A kind of a provider the program configures is not a compile error at
/// the top level either; one whose namespace names no provider still is.
#[test]
fn an_unknown_type_is_an_error_unless_its_provider_is_configured_later() {
    let s = project();
    s.write(
        "stacks/p.df",
        &format!(
            "{STACK}resource k8s.traefik.io.v1alpha1.middleware top {{ metadata.name = \"top\" }}\n"
        ),
    );
    let r = dform(&s, &["plan", "p"]).success();
    assert!(
        r.stdout
            .contains("  + k8s.traefik.io.v1alpha1.middleware top"),
        "{}",
        r.stdout
    );
    s.write(
        "stacks/p.df",
        &format!("{STACK}resource nowhere.thing top {{ name = \"top\" }}\n"),
    );
    let r = dform(&s, &["plan", "p"]).failure();
    assert!(
        r.stderr
            .contains("nowhere.thing; no known provider schema declares it"),
        "{}",
        r.stderr
    );
}

/// A kubeconfig read from a host that has not answered yet (the
/// `ssh://` read is "not yet"): the provider waits on it, and `later` says
/// both, the read by its location (R-153), never split at its dots as if
/// it were an address.
#[test]
fn a_provider_waiting_on_a_read_not_yet_answered_says_both() {
    let s = project();
    s.write(
        "stacks/p.df",
        &STACK.replace("${server.endpoint}", "127.0.0.1:1"),
    );
    let r = dform(&s, &["plan", "p"]).success();
    assert!(
        r.stdout.contains(
            "\n  waits on  provider k8s  kubeconfig = raw, \
             ssh://ubuntu@127.0.0.1:1/etc/rancher/k3s/k3s.yaml\n"
        ),
        "{}",
        r.stdout
    );
}
