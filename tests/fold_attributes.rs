//! R-124: an attribute prints as the source wrote it. The leaves one
//! contribution wrote fold back into one value, in the formatter's layout,
//! at the path where the writers diverge; a leaf another contribution
//! wrote splits out on its own line with its site, a schema's default of a
//! merge key with `schema default`; `-vv` expands to leaves; `why` and
//! `query` print a value the same way.

mod common;
use common::Scratch;

const SCHEMA: &str = r#"
type_provider("kube.deployment", "kube")
type_attr("kube.deployment", "metadata.name", "string", ["required"])
type_attr("kube.deployment", "metadata.labels", "map", [])
type_attr("kube.deployment", "spec.template.spec.containers", "list", ["required"])
type_list_key("kube.deployment", "spec.template.spec.containers", ["name"])
type_provider("kube.service", "kube")
type_attr("kube.service", "metadata.name", "string", ["required"])
type_attr("kube.service", "spec.ports", "list", ["required"])
type_list_key("kube.service", "spec.ports", ["port", "protocol"])
type_default("kube.service", "spec.ports.protocol", "TCP")
"#;

const MAIN: &str = r#"input acme_email: string = "admin@example.com"

use kube

resource kube.deployment traefik {
  metadata = { name: "traefik", labels: { app: "traefik" } }
  spec.template.spec.containers = [{
    name: "traefik",
    image: "traefik:v3.1",
    args: [
      "--entrypoints.web.address=:80",
      "--entrypoints.websecure.address=:443",
      "--providers.kubernetescrd",
      "--providers.kubernetesingress",
      "--certificatesresolvers.letsencrypt.acme.email=${acme_email}",
      "--certificatesresolvers.letsencrypt.acme.storage=/data/acme.json",
      "--certificatesresolvers.letsencrypt.acme.tlschallenge=true",
      "--api.dashboard=true",
    ],
  }]
}

resource kube.service traefik {
  metadata.name = "traefik"
  spec.ports = [{ name: "web", port: 80, targetPort: 8000 }, { name: "websecure", port: 443, targetPort: 8443 }]
}

set d.metadata.labels.owner = "simon" where d in kube.deployment
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nkube = \"providers/kube\"\n",
    );
    s.write("providers/kube/schema.df", SCHEMA);
    s.write("main.df", MAIN);
    s
}

fn run(s: &Scratch, args: &[&str]) -> String {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    s.run(&all).success().stdout
}

/// The Traefik deployment: the container one value, laid out, each
/// argument whole on its own line; the label a policy adds and the
/// service's port protocols the schema defaults on their own lines.
#[test]
fn a_value_one_write_made_is_one_laid_out_line() {
    let s = project("fold-traefik");
    let plan = run(&s, &["plan", "main"]);
    let want = r#"  + kube.deployment traefik            main.df:5
      metadata = { labels: { app: "traefik" }, name: "traefik" }
      metadata.labels.owner = "simon"  main.df:28
      spec.template.spec.containers[name=traefik] = {
        args: [
          "--entrypoints.web.address=:80",
          "--entrypoints.websecure.address=:443",
          "--providers.kubernetescrd",
          "--providers.kubernetesingress",
          "--certificatesresolvers.letsencrypt.acme.email=admin@example.com",
          "--certificatesresolvers.letsencrypt.acme.storage=/data/acme.json",
          "--certificatesresolvers.letsencrypt.acme.tlschallenge=true",
          "--api.dashboard=true",
        ],
        image: "traefik:v3.1",
        name: "traefik",
      }
"#;
    assert!(plan.contains(want), "{plan}");
    assert!(
        plan.contains(
            "      spec.ports = [\n        { name: \"web\", port: 80, targetPort: 8000 },\n        \
             { name: \"websecure\", port: 443, targetPort: 8443 },\n      ]\n      \
             spec.ports[port=80,protocol=TCP].protocol = \"TCP\"  schema default\n      \
             spec.ports[port=443,protocol=TCP].protocol = \"TCP\"  schema default\n"
        ),
        "{plan}"
    );
    // `-v` says the same lines.
    let how = run(&s, &["plan", "-v", "main"]);
    assert!(
        how.contains("      spec.template.spec.containers[name=traefik] = {\n"),
        "{how}"
    );
    // `-vv`: leaf by leaf, each with its chain.
    let full = run(&s, &["plan", "-vv", "main"]);
    for l in [
        "      metadata.labels.app = \"traefik\"\n",
        "      spec.template.spec.containers[name=traefik].args[4] = \
         \"--certificatesresolvers.letsencrypt.acme.email=admin@example.com\"\n",
        "      spec.template.spec.containers[name=traefik].image = \"traefik:v3.1\"\n",
    ] {
        assert!(full.contains(l), "{l}\n{full}");
    }
    assert!(!full.contains(" = {\n"), "{full}");
}

/// `why` folds the same way, the chain of the write that made the value;
/// `query` of one attribute prints its value in the same layout.
#[test]
fn why_and_query_fold_the_same_way() {
    let s = project("fold-why");
    let why = run(&s, &["why", "kube.deployment traefik", "main"]);
    assert!(
        why.contains(
            "  metadata = { labels: { app: \"traefik\" }, name: \"traefik\" }\n    \
             = { name: \"traefik\", labels: { app: \"traefik\" } }  main.df:6\n  \
             metadata.labels.owner = \"simon\"     main.df:28\n  \
             spec.template.spec.containers = [{  main.df:7\n    args: [\n"
        ),
        "{why}"
    );
    let q = run(
        &s,
        &["query", "kube.deployment[\"traefik\"].metadata", "main"],
    );
    assert_eq!(
        q,
        "{ labels: { app: \"traefik\", owner: \"simon\" }, name: \"traefik\" }\n"
    );
}

/// The engine's own open null for an optional computed attribute is a
/// placeholder the program's write replaces: never an `over` line
/// (R-124 amendment).
#[test]
fn a_placeholder_null_is_no_write_beaten() {
    let s = Scratch::new("fold-placeholder");
    let dir = common::repo().join("examples/crud-api");
    let full = s
        .run(&["-C", dir.to_str().unwrap(), "plan", "-vv"])
        .success()
        .stdout;
    assert!(
        full.contains("      metadata.name = \"crud-api-migrate-v42\""),
        "{full}"
    );
    assert!(!full.contains(" over ?"), "{full}");
}
