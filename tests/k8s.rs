//! The mock Kubernetes provider: crates/dform-mock/schemas/k8s.df, no cluster.

mod common;
use common::{Scratch, repo};

/// This plan is pinned as tests/golden/k8s_demo/default.plan.txt (the
/// golden test in tests/golden.rs runs the same case against an empty
/// world; here it's re-run against a named world file so the rest of this
/// test can inspect `w.json` after apply).
fn expected_plan() -> String {
    let golden = std::fs::read_to_string(repo().join("tests/golden/k8s_demo/default.plan.txt"))
        .expect("tests/golden/k8s_demo/default.plan.txt");
    let stdout = golden
        .split_once("-- stdout --\n")
        .and_then(|(_, rest)| rest.split_once("-- stderr --\n"))
        .map(|(stdout, _)| stdout)
        .expect("golden transcript format");
    stdout.to_string()
}

#[test]
fn k8s_demo_plans_against_the_mock() {
    let s = Scratch::new("k8s-demo");
    let prog = repo().join("examples/k8s/stacks/k8s_demo.df");
    let prog = prog.to_str().unwrap();
    let args = |cmd| common::on(prog, &["--provider", "k8s", "--world", "w.json"], cmd);
    let r = s.run(&args(&["plan"])).success();
    assert_eq!(r.stdout, expected_plan());

    // Apply: the server picks the ConfigMap's name from generateName and fills
    // the Deployment's reference to it; defaults land in computed.
    s.run(&args(&["apply"])).success();
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
    let r = s.run(&args(&["plan"])).success();
    assert_eq!(r.summary(), "stack k8s_demo is up to date", "{}", r.stdout);
}

const TWO: &str = r#"

resource k8s.deployment api {
  metadata.name = "api",
  spec.selector.matchLabels = {app: "api"},
  spec.template.spec.containers = [
    {name: "app", image: "api:1"},
    {name: "sidecar", image: "envoy:1"}
  ]
}
"#;

#[test]
fn containers_diff_by_merge_key_not_index() {
    let s = Scratch::new("k8s-keys");
    s.write("p.df", TWO);
    let args = |cmd| common::on("p.df", &["--provider", "k8s", "--world", "w.json"], cmd);
    s.run(&args(&["apply"])).success();

    // The cluster reports the containers in the other order: not a change.
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let cs =
        w["resources"]["k8s.deployment::api"]["attrs"]["spec"]["template"]["spec"]["containers"]
            .as_array_mut()
            .unwrap();
    cs.reverse();
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = s.run(&args(&["plan"])).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);

    s.write("p.df", &TWO.replace("envoy:1", "envoy:2"));
    let r = s.run(&args(&["plan"])).success();
    assert!(
        r.stdout.contains(
            r#"spec.template.spec.containers[name=sidecar].image = "envoy:1" → "envoy:2""#
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
        r#"
resource k8s.deployment api { spec.template.spec.containers = [{name: "a", image: "b"}] }"#,
    );
    let r = s
        .run(&["dev", "--provider", "k8s", "plan", "p.df"])
        .failure();
    assert!(
        r.stderr.contains(
            "Error: plan k8s.deployment api: refused\n  spec.selector.matchLabels is required: \
             matchLabels is a map of {key,value} pairs\n  p.df:2\n"
        ),
        "{}",
        r.stderr
    );
}
