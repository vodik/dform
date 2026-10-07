//! A resource of a kind the cluster serves only once it has the CRD (R-126):
//! when the program makes the CustomResourceDefinition (here, from a
//! vendored manifest), the resource waits on it under `later`; apply asks
//! the provider again for the kind once the CRD exists, and makes it at
//! the next tick. When nothing makes the CRD, a kind the
//! cluster does not serve is an error that names the CRD it lacks.
//!
//! The mock plays the cluster: `crds` in its settings names the schema it
//! serves for a CRD once its world holds that CRD.

mod common;
use common::{Run, Scratch};

/// Traefik's CRDs as it ships them, one `---` stream (trimmed).
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

/// The schema the mock serves for the Middleware kind once it has the CRD.
const MIDDLEWARE: &str = "\
type_provider(k8s.traefik.middleware, \"k8s\")
type_attr(k8s.traefik.middleware, \"metadata.name\", \"string\", [\"required\", \"id\"])
type_attr(k8s.traefik.middleware, \"metadata.namespace\", \"string\", [])
type_attr(k8s.traefik.middleware, \"metadata.uid\", \"string\", [\"computed\", \"id\"])
type_mint(k8s.traefik.middleware, \"metadata.uid\", \"uid-{name}\")
type_attr(k8s.traefik.middleware, \"spec.buffering.maxRequestBodyBytes\", \"int\", [])
";

/// The shape of ~/src/ovh-infra's traefik.df: the CRDs from the vendored
/// manifest, and a Middleware the forge's Ingress names.
const PROG: &str = r#"
use k8s { kubeconfig = "kc", crds = { "middlewares.traefik.io": "mw.df" } }

resource k8s.custom_resource_definition "${d.metadata.name}" = d where d in yaml("crds.yml")

resource k8s.traefik.middleware large_upload {
  metadata = { name: "large-upload", namespace: "apps" }
  spec.buffering.maxRequestBodyBytes = 536870912
}
"#;

fn scratch(name: &str, prog: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("crds.yml", CRDS);
    s.write("mw.df", MIDDLEWARE);
    s.write("p.df", prog);
    s
}

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    let out = common::dform()
        .args(&all)
        .env("NO_COLOR", "1")
        .env("DFORM_WAIT_POLL_MS", "50")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    Run::from(out)
}

/// The plan makes the CRDs, and lists the Middleware under `later`,
/// waiting on its CRD, never "not a kind this cluster serves".
#[test]
fn a_kind_the_program_defines_waits_on_its_crd() {
    let s = scratch("crd-wait-plan", PROG);
    let r = dev(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 2 changes (2 create) over 1 tick, 1 later",
        "{}",
        r.stdout
    );
    let (tick1, later) = r.stdout.split_once("\nlater").unwrap();
    assert!(
        tick1.contains("  + k8s.custom_resource_definition \"middlewares.traefik.io\"")
            && tick1.contains("  + k8s.custom_resource_definition \"tlsoptions.traefik.io\""),
        "{}",
        r.stdout
    );
    assert!(!tick1.contains("k8s.traefik.middleware"), "{}", r.stdout);
    assert!(
        later.contains(
            "  waits on  k8s.custom_resource_definition \"middlewares.traefik.io\"\n  \
             + k8s.traefik.middleware large_upload"
        ),
        "{}",
        r.stdout
    );
}

/// Apply makes the CRDs, asks the provider again for the kind they define
/// at the boundary, and makes the Middleware in tick 2 with its schema.
#[test]
fn apply_makes_the_kind_the_tick_after_its_crd() {
    let s = scratch("crd-wait-apply", PROG);
    let r = dev(&s, &["apply", "--yes"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let mw = &w["resources"]["k8s.traefik.middleware::large_upload"];
    assert_eq!(mw["attrs"]["metadata"]["name"], "large-upload", "{w}");
    // Typed by the schema learned at the boundary: its computed uid.
    assert_eq!(mw["computed"]["metadata"]["uid"], "uid-large_upload", "{w}");
    let r = dev(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// Nothing makes the CRD, and the cluster (the provider configured from
/// the program's settings) does not serve the kind: an error at the
/// resource, naming the CRD it would need.
#[test]
fn a_kind_nothing_defines_is_an_error_naming_its_crd() {
    let prog = PROG.replace(
        "resource k8s.custom_resource_definition \"${d.metadata.name}\" = d where d in yaml(\"crds.yml\")\n",
        "",
    );
    let s = scratch("crd-wait-none", &prog);
    let r = dev(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "p.df:5:1: k8s.traefik.middleware large_upload: the cluster has no kind \
             traefik.middleware and nothing in the program makes its CRD \
             (middlewares.traefik.*)"
        ),
        "{}",
        r.stderr
    );
}

/// With the Kubernetes provider itself, waiting on its settings (R-110: a
/// kubeconfig read from a server not up yet): the CRDs are typed by its
/// static schema, and the Middleware waits on its CRD rather than on the
/// provider's schema.
#[test]
fn with_the_kubernetes_provider_the_kind_waits_on_its_crd() {
    let s = Scratch::project("crd-wait-k8s");
    s.write("crds.yml", CRDS);
    s.write(
        "stacks/p.df",
        r#"
use fake
use ssh
resource db.postgres server { name = "server" }
let raw = ssh.read(server.endpoint, "ubuntu", "/etc/rancher/k3s/k3s.yaml")
use k8s { source = "./providers/k8s", kubeconfig = raw }

resource k8s.custom_resource_definition "${d.metadata.name}" = d where d in yaml("crds.yml")

resource k8s.traefik.middleware large_upload {
  metadata = { name: "large-upload", namespace: "apps" }
  spec.buffering.maxRequestBodyBytes = 536870912
}
"#,
    );
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    let out = common::dform()
        .args(["plan", "p"])
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1")
        .env("NO_COLOR", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .output()
        .unwrap();
    let r = Run::from(out).success();
    let (_, later) = r.stdout.split_once("\nlater").unwrap();
    assert!(
        later.contains("      spec.group = \"traefik.io\"")
            && later.contains(
                "  waits on  k8s.custom_resource_definition \"middlewares.traefik.io\"\n  \
                 + k8s.traefik.middleware large_upload"
            ),
        "{}",
        r.stdout
    );
}
