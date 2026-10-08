//! `dform status` (R-203): each object whose provider answers Health for
//! its type, as it judges it now, one line each, then a summary; `-` for
//! a type it does not judge; exit 0 when each is healthy or suspended, 1
//! otherwise; `--json`; with no target, each deployment the project
//! module lists. Health is a call of its own: nothing of it reaches the
//! plan, the apply or state.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::{self, Server};
use serde_json::Value as Json;

const APP: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource compute.vm web { size = "small" }
resource compute.vm worker { size = "small" }
resource db.postgres db { version = "16" }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/app.df", APP);
    s.run(&["apply", "app"]).success();
    s
}

/// Set the health of the mock's object `typ` `name` in the deployment's
/// world (`dform.state/STACK/remote.json`).
fn judge(s: &Scratch, stack: &str, typ: &str, name: &str, state: &str, reason: &str) {
    let rel = format!("dform.state/{stack}/remote.json");
    let mut w = s.json(&rel);
    let (_, o) = w["resources"]
        .as_object_mut()
        .unwrap()
        .iter_mut()
        .find(|(_, o)| o["typ"] == typ && o["name"].as_str().unwrap().ends_with(name))
        .unwrap_or_else(|| panic!("no {typ} {name} in the world"));
    o["health"] = serde_json::json!({"state": state, "reason": reason});
    s.write(&rel, &w.to_string());
}

/// The lines before the summary, their spaces collapsed.
fn lines(r: &Run) -> Vec<String> {
    r.stdout
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn status_says_each_objects_health_and_exits_by_it() {
    let s = project("status-mock");
    let state = s.read("dform.state/app/state.json");
    let r = s.run(&["status", "app"]).success();
    assert_eq!(
        lines(&r),
        [
            "compute.vm web healthy",
            "compute.vm worker healthy",
            "db.postgres db healthy",
            "net.vpc main -",
            "status: 3 healthy, 1 without health",
        ],
        "{}",
        r.stderr
    );
    // Suspended is settled: still 0.
    judge(&s, "app", "compute.vm", "worker", "suspended", "SHUTOFF");
    let r = s.run(&["status", "app"]).success();
    assert!(
        lines(&r).contains(&"compute.vm worker suspended SHUTOFF".to_string()),
        "{}",
        r.stdout
    );
    // Each of the others is not.
    for (state, reason, summary) in [
        (
            "degraded",
            "1 of 3 available",
            "1 healthy, 1 degraded, 1 suspended",
        ),
        (
            "progressing",
            "BUILD",
            "1 healthy, 1 progressing, 1 suspended",
        ),
        ("unknown", "RESCUE", "1 healthy, 1 suspended, 1 unknown"),
    ] {
        judge(&s, "app", "compute.vm", "web", state, reason);
        let r = s.run(&["status", "app"]).failure();
        assert_eq!(r.code, Some(1), "{state}: {}", r.stderr);
        assert!(
            lines(&r).contains(&format!("compute.vm web {state} {reason}")),
            "{state}: {}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains(&format!("status: {summary}, 1 without health")),
            "{}",
            r.stdout
        );
    }
    // A status writes nothing, and the plan knows nothing of health.
    assert_eq!(s.read("dform.state/app/state.json"), state);
    let plan = s.run(&["plan", "app"]).success();
    assert_eq!(plan.summary(), "stack app is up to date", "{}", plan.stdout);
}

#[test]
fn status_json_is_each_object_and_the_counts() {
    let s = project("status-json");
    judge(
        &s,
        "app",
        "compute.vm",
        "web",
        "degraded",
        "1 of 3 available",
    );
    let r = s.run(&["status", "app", "--json"]).failure();
    assert_eq!(r.code, Some(1));
    let doc: Json = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(doc["deployment"], "app");
    assert_eq!(doc["settled"], false);
    let web = doc["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["address"] == "compute.vm web")
        .unwrap();
    assert_eq!(web["health"], "degraded");
    assert_eq!(web["reason"], "1 of 3 available");
    assert_eq!(web["provider"], "fakecloud");
    let vpc = doc["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["type"] == "net.vpc")
        .unwrap();
    assert_eq!(vpc["health"], Json::Null);
    assert_eq!(doc["summary"]["healthy"], 2);
    assert_eq!(doc["summary"]["degraded"], 1);
    assert_eq!(doc["summary"]["without_health"], 1);
}

/// A provider whose handshake lists no type is asked nothing: every line
/// is `-`, and nothing it does not judge fails the status.
#[test]
fn a_provider_that_answers_no_health_is_asked_none() {
    let s = project("status-none");
    judge(
        &s,
        "app",
        "compute.vm",
        "web",
        "degraded",
        "1 of 3 available",
    );
    let mut c = common::dform();
    c.args(["status", "app"])
        .current_dir(&s.dir)
        .env("DFORM_TEST_FAKE_NO_HEALTH", "1");
    let r = Run::from(c.output().unwrap()).success();
    assert!(
        lines(&r).contains(&"compute.vm web -".to_string()),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("status: 4 without health"),
        "{}",
        r.stdout
    );
}

/// With no target, each deployment the project module lists, headed by
/// its name; the run fails when one is not settled; `--json` an array.
#[test]
fn status_with_no_target_says_each_listed_deployment() {
    let s = project("status-matrix");
    s.write(
        "stacks/edge.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\nresource compute.vm gw { size = \"small\", tags = { env } }\n",
    );
    s.write(
        "project.df",
        "resource stacks.app main {}\nresource stacks.edge lab { env = \"lab\" }\nresource stacks.edge prod { env = \"prod\" }\n",
    );
    s.run(&["apply", "edge", "env=lab"]).success();
    let r = s.run(&["status"]).success();
    let l = lines(&r);
    let at = |h: &str| {
        l.iter()
            .position(|x| x == h)
            .unwrap_or_else(|| panic!("{h}: {l:?}"))
    };
    assert!(at("== app") < at("== edge[env=lab]"));
    assert_eq!(l[at("== edge[env=lab]") + 1], "compute.vm gw healthy");
    assert_eq!(
        l[at("== edge[env=prod]") + 1],
        "status: never applied, no objects"
    );
    judge(
        &s,
        "edge/env=lab",
        "compute.vm",
        "gw",
        "progressing",
        "BUILD",
    );
    let r = s.run(&["status"]).failure();
    assert_eq!(r.code, Some(1));
    let r = s.run(&["status", "--json"]).failure();
    let docs: Json = serde_json::from_str(&r.stdout).unwrap();
    let settled: Vec<(&str, bool)> = docs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["deployment"].as_str().unwrap(), d["settled"] == true))
        .collect();
    assert_eq!(
        settled,
        [
            ("app", true),
            ("edge[env=lab]", false),
            ("edge[env=prod]", true)
        ]
    );
}

/// The OVH provider judges an instance from its status word as the API
/// answers it now; an SSH key it does not judge.
#[test]
fn ovh_judges_an_instance_by_its_status() {
    let server = Server::start();
    let s = Scratch::project("status-ovh");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-ovh")
        ),
    );
    s.write(
        "main.df",
        &format!(
            r#"
use ovh {{ endpoint = "{}", project = "{}" }}
resource ovh.ssh_key admin {{ name = "lab-admin", public_key = "ssh-ed25519 AAAAC3Nz lab" }}
resource ovh.instance server {{
  name = "lab-server"
  region = "ca-east-tor"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  ssh_key = admin
}}
"#,
            server.endpoint,
            fake::DESCRIPTION
        ),
    );
    let dform = |args: &[&str]| {
        let mut c = common::dform();
        c.args(common::yes(args))
            .current_dir(&s.dir)
            .env("HOME", &s.dir)
            .env("XDG_CONFIG_HOME", s.path("config"))
            .env_remove("OVH_CLOUD_PROJECT_SERVICE");
        for (k, v) in server.env() {
            c.env(k, v);
        }
        Run::from(c.output().unwrap())
    };
    dform(&["apply", "main.df"]).success();
    let r = dform(&["status", "main.df"]).success();
    assert_eq!(
        lines(&r),
        [
            "ovh.instance server healthy ACTIVE",
            "ovh.ssh_key admin -",
            "status: 1 healthy, 1 without health",
        ],
        "{}",
        r.stderr
    );
    let set = |status: &str| {
        let mut w = server.world.lock().unwrap();
        for o in w.instances.values_mut() {
            o["status"] = serde_json::json!(status);
        }
    };
    set("SHUTOFF");
    let r = dform(&["status", "main.df"]).success();
    assert_eq!(lines(&r)[0], "ovh.instance server suspended SHUTOFF");
    set("ERROR");
    let r = dform(&["status", "main.df"]).failure();
    assert_eq!(lines(&r)[0], "ovh.instance server degraded ERROR");
    server.world.lock().unwrap().instances.clear();
    let r = dform(&["status", "main.df"]).failure();
    assert_eq!(lines(&r)[0], "ovh.instance server degraded not found");
}
