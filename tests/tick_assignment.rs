//! Ticks by the dependency graph alone (R-156): a change whose waits are
//! all made by earlier ticks of this plan is in the first tick after
//! them, counted, its attributes planned as written when its schema
//! arrives only at that boundary; `later` keeps the waits no tick of this
//! plan makes.
//!
//! The platform shape of ~/src/ovh-infra on the mocks: a server in tick
//! 1, the k8s provider configured from what it makes, its objects (the
//! Traefik CRDs among them) in tick 2, a Middleware of a kind a CRD of
//! tick 2 defines in tick 3.

mod common;
use common::{Run, Scratch};

const CRDS: &str = "\
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: middlewares.traefik.io
spec:
  group: traefik.io
  names:
    kind: Middleware
    plural: middlewares
  scope: Namespaced
  versions:
  - name: v1alpha1
    served: true
    storage: true
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: tlsoptions.traefik.io
spec:
  group: traefik.io
  names:
    kind: TLSOption
    plural: tlsoptions
  scope: Namespaced
  versions:
  - name: v1alpha1
    served: true
    storage: true
";

const MIDDLEWARE: &str = "\
type_provider(k8s.traefik.middleware, \"k8s\")
type_attr(k8s.traefik.middleware, \"metadata.name\", \"string\", [\"required\", \"id\"])
type_attr(k8s.traefik.middleware, \"metadata.namespace\", \"string\", [])
type_attr(k8s.traefik.middleware, \"spec.buffering.maxRequestBodyBytes\", \"int\", [])
";

const PLATFORM: &str = r#"
use env
use fake { source = "prov" }
resource db.postgres server { name = "server" }
resource net.vpc edge { cidr = "10.0.0.0/16" }
resource iam.policy admin { name = "admin" }
let kubeconfig = str.format("%s@%s", env.var("TICKS_KUBECONFIG"), server.endpoint)
use k8s { kubeconfig = kubeconfig, crds = { "middlewares.traefik.io": "mw.df" } }

resource k8s.custom_resource_definition "${d.metadata.name}" = d where d in yaml.decode(io.read("crds.yml"))
resource k8s.namespace traefik { metadata.name = "traefik" }

resource k8s.traefik.middleware large_upload {
  metadata = { name: "large-upload", namespace: traefik.metadata.name }
  spec.buffering.maxRequestBodyBytes = 536870912
}
"#;

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("crds.yml", CRDS);
    s.write("mw.df", MIDDLEWARE);
    s.write("p.df", PLATFORM);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    let out = common::dform()
        .args(&all)
        .env("TICKS_KUBECONFIG", "kc")
        .env("DFORM_WAIT_POLL_MS", "50")
        .env("NO_COLOR", "1")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    Run::from(out)
}

/// Three ticks and no `later`: the provider's objects after the server
/// its settings are made from, the Middleware after its CRD.
#[test]
fn what_this_plan_makes_is_waited_on_in_its_ticks() {
    let s = scratch("ticks-platform");
    let r = dev(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 7 changes (7 create) over 3 ticks",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("\nlater"), "{}", r.stdout);
    let (tick1, rest) = r.stdout.split_once("\ntick 2  3 changes\n").unwrap();
    let (tick2, tick3) = rest.split_once("\ntick 3  1 change\n").unwrap();
    assert!(
        tick1.contains("tick 1  3 changes\n") && !tick1.contains("k8s."),
        "{}",
        r.stdout
    );
    assert!(
        tick2.starts_with("  waits on  provider k8s  ")
            && tick2.contains("kubeconfig = kubeconfig\n")
            && tick2.contains("  + k8s.custom_resource_definition \"middlewares.traefik.io\"")
            && tick2.contains("  + k8s.namespace traefik"),
        "{}",
        r.stdout
    );
    assert!(
        tick3.starts_with(
            "  waits on  k8s.custom_resource_definition \"middlewares.traefik.io\"\n  \
             + k8s.traefik.middleware large_upload"
        ) && tick3.contains("spec.buffering.maxRequestBodyBytes = 536870912"),
        "{}",
        r.stdout
    );
    // `--json` says the same ticks, and nothing under `later`.
    let r = dev(&s, &["plan", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["summary"]["changes"], 7, "{j}");
    assert_eq!(j["later"], serde_json::json!([]), "{j}");
    let ticks: Vec<usize> = j["ticks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["changes"].as_array().unwrap().len())
        .collect();
    assert_eq!(ticks, [3, 3, 1], "{j}");
}

/// Apply makes the three ticks the plan showed, asked once.
#[test]
fn apply_makes_the_ticks_the_plan_showed() {
    let s = scratch("ticks-platform-apply");
    let r = dev(&s, &["apply", "--yes"]).success();
    assert!(r.stdout.contains("tick 3  1 change"), "{}", r.stdout);
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let mw = &w["resources"]["k8s.traefik.middleware::large_upload"];
    assert_eq!(mw["attrs"]["metadata"]["namespace"], "traefik", "{w}");
    let r = dev(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// A provider configured from what no tick of this plan makes (a read
/// that has not answered and no resource it waits on) stays `later`.
#[test]
fn a_provider_configured_from_outside_the_plan_is_later() {
    let s = scratch("ticks-outside");
    s.write(
        "p.df",
        &PLATFORM.replace(
            "env.var(\"TICKS_KUBECONFIG\"), server.endpoint",
            "env.var(\"TICKS_KUBECONFIG\"), io.read(\"ssh://ubuntu@127.0.0.1:1/kc\")",
        ),
    );
    let r = dev(&s, &["plan"]).success();
    assert!(
        r.summary().ends_with("over 1 tick, 4 later"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("\nlater\n") && !r.stdout.contains("\ntick 2"),
        "{}",
        r.stdout
    );
}
