//! `dform render` (R-202): a deployment's planned documents as their
//! provider's objects, for a tool that applies them. What a process
//! shows: the stream on stdout as `kubectl apply -f -` reads it, what is
//! not rendered on stderr, and the exit status of a render that is
//! refused (a deny, 4) or has a hole (1). The documents, their holes and
//! their order are `dform_core::render`'s unit tests; the kinds and the
//! edges `src/cli/render.rs`'s.

mod common;
use common::{Scratch, copy_dir, repo};
use serde_json::Value as Json;

/// A program on the mock's Kubernetes types beside one of its cloud's.
const SHOP: &str = "\
input env: enum(\"dev\", \"prod\") = \"dev\"
use fake
use k8s
resource net.vpc main { cidr = \"10.0.0.0/16\" }
resource k8s.namespace shop { metadata.name = \"shop\" }
resource k8s.config_map cfg {
  metadata = { name: \"cfg\", namespace: \"shop\" }
  data = { mode: env }
}
resource k8s.deployment web {
  metadata = { name: \"web\", namespace: shop.metadata.name }
  spec.selector.matchLabels = { app: \"web\" }
  spec.template.metadata.labels = { app: \"web\" }
  spec.template.spec.containers = [{
    name: \"web\",
    image: \"nginx:1.27\",
    resources: { limits: { memory: 512Mi } },
    envFrom: [{ configMapRef: { name: cfg.metadata.name } }],
  }]
}
deny \"prod is not rendered here\" where env == \"prod\"
";

fn shop(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", SHOP);
    s
}

/// The objects as the provider sends them: `apiVersion` and `kind`
/// first, a reference resolved, a quantity as Kubernetes writes it, no
/// label of dform's; the Namespace before what is in it (the ConfigMap
/// names it by a string, not a reference), the Deployment after the
/// ConfigMap it reads. The vpc is said once on stderr. `--json` is the
/// same objects.
#[test]
fn a_deployment_renders_as_a_stream_in_the_plans_order() {
    let s = shop("render-stream");
    let r = s.run(&["render", "p.df"]).success();
    assert_eq!(
        r.stdout,
        "---\napiVersion: v1\nkind: Namespace\nmetadata:\n  name: shop\n\
         ---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cfg\n  namespace: shop\n\
         data:\n  mode: dev\n\
         ---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: shop\n\
         spec:\n  selector:\n    matchLabels:\n      app: web\n  template:\n    metadata:\n      \
         labels:\n        app: web\n    spec:\n      containers:\n      - envFrom:\n        \
         - configMapRef:\n            name: cfg\n        image: nginx:1.27\n        name: web\n        \
         resources:\n          limits:\n            memory: 512Mi\n",
        "{}",
        r.stderr
    );
    assert_eq!(
        r.stderr,
        "not rendered: net.vpc main (fakecloud has no document form)\n"
    );
    let j = s.run(&["render", "p.df", "--json"]).success();
    let objects: Vec<Json> = serde_json::from_str(&j.stdout).unwrap();
    let kinds: Vec<&str> = objects
        .iter()
        .map(|o| o["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Namespace", "ConfigMap", "Deployment"]);
    assert_eq!(
        objects[2].pointer("/spec/template/spec/containers/0/envFrom/0/configMapRef/name"),
        Some(&Json::from("cfg"))
    );
}

/// A deny refuses the render as it refuses the plan: its denies, exit 4,
/// nothing on stdout.
#[test]
fn a_deny_refuses_the_render() {
    let s = shop("render-deny");
    let r = s.run(&["render", "p.df", "--set", "env=prod"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(
        r.stderr
            .contains("refused  prod is not rendered here  p.df:21\n"),
        "{}",
        r.stderr
    );
}

/// A name the API server picks and a secret are holes: the render
/// refuses naming each cell (exit 1); `--partial` prints what has none
/// and says each hole.
#[test]
fn a_hole_refuses_the_render_unless_partial() {
    let s = Scratch::project("render-hole");
    s.write(
        "p.df",
        "use k8s\n\
         resource k8s.namespace shop { metadata.name = \"shop\" }\n\
         resource k8s.config_map cfg {\n  \
           metadata = { generateName: \"cfg-\", namespace: \"shop\" }\n  data = { a: \"1\" }\n}\n\
         resource k8s.secret pw {\n  \
           metadata = { name: \"pw\", namespace: \"shop\" }\n  \
           stringData = { password: random.password(\"pw\", 16) }\n}\n\
         resource k8s.service_account sa {\n  \
           metadata = { name: cfg.metadata.name, namespace: \"shop\" }\n}\n",
    );
    let r = s.run(&["render", "p.df"]).failure();
    assert_eq!(r.code, Some(1));
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    let cell = "hole: k8s.service_account sa metadata.name = k8s.config_map cfg.metadata.name, \
                known after apply";
    let secret = "hole: k8s.secret pw stringData is a secret: a render prints none";
    assert!(
        r.stderr.contains(&format!(
            "error  render p: 2 of what its documents need are not known to a render, and a \
             rendered document has no holes:\n  {secret}\n  {cell}\n  help: apply the \
             deployment first, and give the cluster a secret another way; or render the \
             documents with none with `--partial`"
        )),
        "{}",
        r.stderr
    );
    let p = s.run(&["render", "p.df", "--partial", "--json"]).success();
    let objects: Vec<Json> = serde_json::from_str(&p.stdout).unwrap();
    let kinds: Vec<&str> = objects
        .iter()
        .map(|o| o["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Namespace", "ConfigMap"]);
    assert_eq!(p.stderr, format!("{secret}\n{cell}\n"));
}

/// Against the real provider's snapshot: a kind's `apiVersion` and
/// `kind` as the API spells them (`CSIDriver`, not the type's snake
/// case), and a kind a CRD of the program defines after that CRD, though
/// its address comes first.
#[test]
fn the_k8s_provider_names_each_kind() {
    let s = Scratch::project("render-k8s");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "p.df",
        "use k8s { source = \"./providers/k8s\" }\n\
         resource k8s.custom_resource_definition widgets {\n  \
           metadata.name = \"widgets.acme.example.com\"\n  \
           spec = {\n    group: \"acme.example.com\",\n    \
             names: { kind: \"Widget\", plural: \"widgets\" },\n    \
             scope: \"Namespaced\",\n    \
             versions: [{ name: \"v1alpha1\", served: true, storage: true }],\n  }\n}\n\
         resource k8s.csi_driver d {\n  \
           metadata.name = \"d.example.com\"\n  spec = { attachRequired: true }\n}\n\
         resource k8s.acme.widget a {\n  \
           metadata = { name: \"a\", namespace: \"web\" }\n  \
           spec = { size: 3 }\n}\n",
    );
    let mut c = common::dform();
    c.args(["render", "p.df", "--json"])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("KUBECONFIG")
        .env_remove("KUBERNETES_SERVICE_HOST");
    let r = common::Run::from(c.output().unwrap()).success();
    let objects: Vec<Json> = serde_json::from_str(&r.stdout).unwrap();
    let kinds: Vec<(&str, &str)> = objects
        .iter()
        .map(|o| {
            (
                o["apiVersion"].as_str().unwrap(),
                o["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("storage.k8s.io/v1", "CSIDriver"),
            ("apiextensions.k8s.io/v1", "CustomResourceDefinition"),
            ("acme.example.com/v1alpha1", "Widget"),
        ],
        "{}",
        r.stderr
    );
}

/// With no target, each deployment project.df lists, after its name.
#[test]
fn each_listed_deployment_renders_under_its_name() {
    let s = Scratch::project("render-matrix");
    s.write(
        "stacks/app.df",
        "key env: enum(\"a\", \"b\")\nuse k8s\n\
         resource k8s.namespace ns { metadata.name = env }\n",
    );
    s.write(
        "project.df",
        "resource stacks.app a { env = \"a\" }\nresource stacks.app b { env = \"b\" }\n",
    );
    let r = s.run(&["render"]).success();
    assert_eq!(
        r.stdout,
        "# stacks.app[env=a]\n---\napiVersion: v1\nkind: Namespace\nmetadata:\n  name: a\n\
         # stacks.app[env=b]\n---\napiVersion: v1\nkind: Namespace\nmetadata:\n  name: b\n",
        "{}",
        r.stderr
    );
}

/// The demo makes nothing of a provider with a document form: an empty
/// stream, each resource said once, and a render that succeeds.
#[test]
fn the_demo_renders_an_empty_stream() {
    let s = Scratch::new("render-demo");
    copy_dir(&repo().join("examples/demo"), &s.dir);
    let r = s.run(&["render", "dform", "env=staging"]).success();
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    let lines: Vec<&str> = r.stderr.lines().collect();
    assert!(
        !lines.is_empty()
            && lines.iter().all(|l| l.starts_with("not rendered: ")
                && l.ends_with(" (fakecloud has no document form)")),
        "{}",
        r.stderr
    );
    assert!(lines.contains(&"not rendered: net.vpc main.vpc (fakecloud has no document form)"));
}
