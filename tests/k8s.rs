//! The mock Kubernetes provider: providers/k8s/schema.df, no cluster.

mod common;
use common::{Scratch, repo};

const PLAN: &str = r#"plan: 4 to create, 0 to update, 0 to delete
+ k8s.namespace.shop
  metadata.labels.team = "storefront"
  metadata.name = "shop"
+ k8s.service.web
  metadata.name = "web"
  metadata.namespace = "shop"
  spec.ports[port=80,protocol=TCP].port = 80
  spec.ports[port=80,protocol=TCP].protocol = "TCP"
  spec.ports[port=80,protocol=TCP].targetPort = "8080"
  spec.selector.app = "web"
+ k8s.config_map.web_config
  data.LOG_LEVEL = "info"
  data.MODE = "production"
  metadata.generateName = "web-config-"
  metadata.namespace = "shop"
+ k8s.deployment.web
  metadata.name = "web"
  metadata.namespace = "shop"
  spec.replicas = 3
  spec.selector.matchLabels.app = "web"
  spec.template.metadata.labels.app = "web"
  spec.template.spec.containers[name=web].envFrom[0].configMapRef.name = ?k8s.config_map/web_config#metadata.name
  spec.template.spec.containers[name=web].image = "nginx:1.27"
  spec.template.spec.containers[name=web].name = "web"
  spec.template.spec.containers[name=web].ports[containerPort=8080,protocol=TCP].containerPort = 8080
  spec.template.spec.containers[name=web].ports[containerPort=8080,protocol=TCP].protocol = "TCP"
"#;

#[test]
fn k8s_demo_plans_against_the_mock() {
    let s = Scratch::new("k8s-demo");
    let prog = repo().join("examples/k8s_demo.df");
    let args = [
        "--file",
        prog.to_str().unwrap(),
        "--provider",
        "k8s",
        "--world",
        "w.json",
    ];
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert_eq!(r.stdout, PLAN);

    // Apply: the server picks the ConfigMap's name from generateName and fills
    // the Deployment's reference to it; defaults land in computed.
    s.run(&[&args[..], &["apply"]].concat()).success();
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let cm = w["resources"]["k8s.config_map::web_config"]["computed"]["metadata"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cm.starts_with("web-config-"), "{cm}");
    let dep = &w["resources"]["k8s.deployment::web"];
    assert_eq!(
        dep["attrs"]["spec"]["template"]["spec"]["containers"][0]["envFrom"][0]["configMapRef"]["name"],
        serde_json::json!(cm)
    );
    assert_eq!(dep["computed"]["status"]["readyReplicas"], 1);
    assert_eq!(dep["computed"]["spec"]["strategy"]["type"], "RollingUpdate");
    assert_eq!(
        w["resources"]["k8s.service::web"]["computed"]["spec"]["type"],
        "ClusterIP"
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert_eq!(
        r.summary(),
        "plan: 0 to create, 0 to update, 0 to delete",
        "{}",
        r.stdout
    );
}

const TWO: &str = r#"
resource k8s.deployment api {
  metadata.name = "api",
  spec.selector.matchLabels = {app: "api"},
  spec.template.spec.containers = [
    {name: "app", image: "api:1"},
    {name: "sidecar", image: "envoy:1"}
  ]
}.
"#;

#[test]
fn containers_diff_by_merge_key_not_index() {
    let s = Scratch::new("k8s-keys");
    s.write("p.df", TWO);
    let args = ["--file", "p.df", "--provider", "k8s", "--world", "w.json"];
    s.run(&[&args[..], &["apply"]].concat()).success();

    // The cluster reports the containers in the other order: not a change.
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let cs =
        w["resources"]["k8s.deployment::api"]["attrs"]["spec"]["template"]["spec"]["containers"]
            .as_array_mut()
            .unwrap();
    cs.reverse();
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert_eq!(
        r.summary(),
        "plan: 0 to create, 0 to update, 0 to delete",
        "{}",
        r.stdout
    );

    s.write("p.df", &TWO.replace("envoy:1", "envoy:2"));
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert!(
        r.stdout.contains(
            r#"spec.template.spec.containers[name=sidecar].image: "envoy:1" -> "envoy:2""#
        ),
        "{}",
        r.stdout
    );
}

#[test]
fn a_missing_required_attribute_names_resource_and_path() {
    let s = Scratch::new("k8s-required");
    s.write(
        "p.df",
        r#"resource k8s.deployment api { spec.template.spec.containers = [{name: "a", image: "b"}] }."#,
    );
    let r = s
        .run(&["--file", "p.df", "--provider", "k8s", "plan"])
        .failure();
    assert!(
        r.stderr.contains(
            "plan k8s.deployment/api: required attribute spec.selector.matchLabels is not set"
        ),
        "{}",
        r.stderr
    );
}
