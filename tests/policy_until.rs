//! A deny that waits on another deployment's apply is about what it
//! ranges over (R-200, R-214): each resource whose instance of it waits
//! is undetermined, never counted as holding, and its line says until
//! when, `(after stacks.platform[env=lab] is applied)`. Two deployments on
//! the mock; `apps` reads an output of `platform` its plan cannot know.

mod common;
use common::Scratch;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource db.postgres db { name = "db-${env}" }
output endpoint = db.endpoint
"#;

/// `a` reads the output inside a string, `d` as it is, `b` not at all.
const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc a {
  cidr = "10.1.0.0/16"
  name = "n-${platform[env].endpoint}"
}
resource net.vpc b { cidr = "10.2.0.0/16" }
resource net.vpc d {
  cidr = "10.3.0.0/16"
  name = platform[env].endpoint
}
deny "networks are not named nowhere" where v in net.vpc, v.name == "nowhere"
deny "the endpoint is somewhere" { vpc: v } where v in net.vpc, platform[env].endpoint == "nowhere"
"#;

/// A policy line as `--json` says it: its text, what holds, and each
/// resource undetermined with until when.
type Policy = (String, Vec<String>, Vec<(String, String)>);

const AFTER: &str = "(after stacks.platform[env=lab] is applied)";

#[test]
fn a_deny_waiting_on_another_deployment_is_about_what_it_ranges_over() {
    let s = Scratch::project("policy-until");
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    let r = s.run(&["plan", "apps", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    let apps = j["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deployment"] == "stacks.apps[env=lab]")
        .expect(&r.stdout);
    let lines: Vec<Policy> = apps["plan"]["policy"]
        .as_array()
        .expect(&r.stdout)
        .iter()
        .map(|p| {
            (
                p["text"].as_str().unwrap().to_string(),
                p["holds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| h.as_str().unwrap().to_string())
                    .collect(),
                p["undetermined"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|u| {
                        (
                            u["of"].as_str().unwrap().to_string(),
                            u["until"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    let output = format!("until stacks.platform[env=lab].endpoint is known {AFTER}");
    assert_eq!(
        lines,
        [
            (
                "networks are not named nowhere".to_string(),
                vec!["net.vpc b".to_string()],
                vec![
                    ("net.vpc a".to_string(), output.clone()),
                    (
                        "net.vpc d".to_string(),
                        format!("until name is known {AFTER}")
                    ),
                ],
            ),
            (
                "the endpoint is somewhere".to_string(),
                vec![],
                vec![
                    ("net.vpc a".to_string(), output.clone()),
                    ("net.vpc b".to_string(), output.clone()),
                    // Where a resource reads it, the cell it reads it at.
                    (
                        "net.vpc d".to_string(),
                        format!("until name is known {AFTER}")
                    ),
                ],
            ),
        ],
        "{}",
        r.stdout
    );

    // The text says the same, under the deployment that waits.
    let r = s.run(&["plan", "apps"]).success();
    assert!(
        r.stdout.contains(&format!("{output}\n")) && !r.stdout.contains("is known\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("undetermined  networks are not named nowhere  stacks/apps.df:14  1 hold · 2 undetermined"),
        "{}",
        r.stdout
    );
}

/// Not reached yet: a component's deny over an input that waits on
/// another deployment. Its undetermined instance is bound to the
/// resource's name inside the copy (`{W: "v"}`), and the copy it is in
/// (`__copy1`) is not among a stuck instance's bindings (the engine's
/// `stuck_as` keeps no `__` variable), so the block cannot say which
/// copy's `net.vpc` waits: it names the deployment and counts both as
/// holding.
#[test]
#[ignore = "a stuck instance's bindings name no copy (engine/nulls.rs `stuck_as`)"]
fn a_components_deny_waiting_on_another_deployment_is_about_its_copys_resources() {
    let s = Scratch::project("policy-until-copy");
    s.write("stacks/platform.df", PLATFORM);
    s.write(
        "stacks/apps.df",
        r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
component node {
  input ep: string
  resource net.vpc v { cidr = "10.1.0.0/16" }
  deny "a node's endpoint is somewhere" { vpc: w } where w in net.vpc, ep == "nowhere"
}
resource node n0 { ep = platform[env].endpoint }
resource node n1 { ep = "x" }
"#,
    );
    let r = s.run(&["plan", "apps"]).success();
    assert!(
        r.stdout.contains(&format!(
            "net.vpc n0.v\n        until stacks.platform[env=lab].endpoint is known {AFTER}"
        )) && r.stdout.contains("1 hold · 1 undetermined"),
        "{}",
        r.stdout
    );
}
