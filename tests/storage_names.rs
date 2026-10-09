//! Storage by full names (R-200, after R-200 item 2): a deployment's
//! state directory, its registry entry, its published outputs and its
//! lock are named `stacks.platform[env=lab]`; a deployment still stored
//! under its short name (`dform.state/platform/env=lab`, before) is read
//! where it is by every run, listed as such, and moved by its next apply,
//! which logs one entry: `renamed storage platform[env=lab] →
//! stacks.platform[env=lab]`.

mod common;
use common::Scratch;
use dform_core::store::Store;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
output cidr = edge.cidr
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc rec { cidr = "10.1.0.0/16", name = platform[env].cidr }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s
}

/// The deployment's storage as dform kept it before R-200: its directory,
/// its registry key and its published outputs by its short name.
fn as_before(s: &Scratch) {
    let (new, old) = (
        s.path("dform.state/stacks.platform"),
        s.path("dform.state/platform"),
    );
    std::fs::rename(&new, &old).unwrap();
    let registry = s
        .read("dform.state/stacks.json")
        .replace("stacks.platform[env=lab]", "platform[env=lab]")
        .replace("/stacks.platform/", "/platform/");
    s.write("dform.state/stacks.json", &registry);
    let outputs = s
        .read("dform.state/platform/env=lab/outputs.json")
        .replace("\"stacks.platform[env=lab]\"", "\"platform[env=lab]\"");
    s.write("dform.state/platform/env=lab/outputs.json", &outputs);
}

#[test]
fn state_names_a_deployment_by_its_full_name() {
    let s = project("names-full");
    s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    let published = s.read("dform.state/stacks.platform/env=lab/outputs.json");
    assert!(
        published.contains("\"deployment\": \"stacks.platform[env=lab]\""),
        "{published}"
    );
    let registry: serde_json::Value = s.json("dform.state/stacks.json");
    assert!(
        registry["stacks.platform[env=lab]"].is_string(),
        "{registry}"
    );
    assert!(!s.path("dform.state/platform").exists());
}

/// A deployment stored by its short name: a plan reads it where it is (no
/// change), a reader reads its outputs, `stack list` says so, and its
/// next apply moves it, said once and logged once.
#[test]
fn a_deployment_stored_by_its_short_name_moves_on_its_next_apply() {
    let s = project("names-legacy");
    s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    as_before(&s);

    let r = s.run(&["plan", "platform", "env=lab"]).success();
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
    assert!(s.path("dform.state/platform/env=lab/state.json").exists());
    let r = s.run(&["plan", "--why=none", "apps", "env=lab"]).success();
    assert!(r.stdout.contains("name = \"10.0.0.0/16\""), "{}", r.stdout);
    let list = s.run(&["stack", "list"]).success();
    assert!(
        list.stdout.contains(
            "stacks.platform[env=lab]  stored as platform[env=lab]: its next apply renames it"
        ),
        "{}",
        list.stdout
    );

    let r = s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    assert!(
        r.stderr
            .contains("renamed storage platform[env=lab] → stacks.platform[env=lab]\n"),
        "{}",
        r.stderr
    );
    assert!(!s.path("dform.state/platform").exists());
    let audit = s.read("dform.state/stacks.platform/env=lab/state.audit.jsonl");
    let renamed: Vec<serde_json::Value> = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == "renamed")
        .collect();
    assert_eq!(renamed.len(), 1, "{audit}");
    assert_eq!(
        renamed[0]["storage"], "platform[env=lab] → stacks.platform[env=lab]",
        "{audit}"
    );
    let registry: serde_json::Value = s.json("dform.state/stacks.json");
    assert!(
        registry["stacks.platform[env=lab]"]
            .as_str()
            .is_some_and(|p| p.ends_with("/stacks.platform/env=lab/state.json")),
        "{registry}"
    );
    assert!(registry.get("platform[env=lab]").is_none(), "{registry}");
    let published = s.read("dform.state/stacks.platform/env=lab/outputs.json");
    assert!(
        published.contains("\"deployment\": \"stacks.platform[env=lab]\""),
        "{published}"
    );
    // The next run finds it by its full name: nothing to move, nothing
    // said.
    let r = s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    assert!(!r.stderr.contains("renamed storage"), "{}", r.stderr);
    let list = s.run(&["stack", "list"]).success();
    assert!(!list.stdout.contains("stored as"), "{}", list.stdout);
}

/// The fake S3 server: a backend that names the stack (`state/{stack}`)
/// keeps a deployment under its full name, and one under its short name
/// is copied to it and deleted under the lease by its next apply.
#[test]
fn an_s3_deployment_stored_by_its_short_name_moves_in_the_bucket() {
    let server = dform_s3::fake::Server::start();
    let s = project("names-s3");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[defaults]\nbackend = 's3(\"names\", \"state/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
            server.endpoint
        ),
    );
    let spec = dform_core::store::S3Spec {
        bucket: "names".into(),
        prefix: "state".into(),
        endpoint: Some(server.endpoint.clone()),
        region: Some("us-east-1".into()),
    };
    let bucket =
        dform_s3::S3Store::with_credentials(&spec, "", rusty_s3::Credentials::new("fake", "fake"))
            .unwrap();
    bucket.create_bucket().unwrap();
    let run = |args: &[&str]| {
        let out = common::dform()
            .args(args)
            .current_dir(&s.dir)
            .env("DFORM_S3_ACCESS_KEY_ID", "fake")
            .env("DFORM_S3_SECRET_ACCESS_KEY", "fake")
            .env("DFORM_TEST_PASSPHRASE", "names")
            .output()
            .unwrap();
        common::Run::from(out)
    };
    run(&["apply", "platform", "env=lab", "--yes"]).success();
    let keys = bucket.list("").unwrap();
    assert!(
        keys.iter()
            .any(|k| k == "stacks.platform/env=lab/state.json"),
        "{keys:?}"
    );
    // As before R-200: every object under the short name, the published
    // outputs saying it, and the mock's world in its old home.
    for k in keys {
        let o = bucket.get(&k).unwrap().unwrap();
        let old = k.replacen("stacks.platform/", "platform/", 1);
        let bytes = match k.ends_with("outputs.json") {
            true => String::from_utf8(o.bytes)
                .unwrap()
                .replace("\"stacks.platform[env=lab]\"", "\"platform[env=lab]\"")
                .into_bytes(),
            false => o.bytes,
        };
        bucket
            .put(&old, &bytes, &dform_core::store::Cond::Any)
            .unwrap();
        bucket.delete(&k).unwrap();
    }
    std::fs::rename(
        s.path("dform.state/stacks.platform"),
        s.path("dform.state/platform"),
    )
    .unwrap();
    let registry = s
        .read("dform.state/stacks.json")
        .replace("stacks.platform[env=lab]", "platform[env=lab]")
        .replace("/stacks.platform/", "/platform/");
    s.write("dform.state/stacks.json", &registry);

    let r = run(&["plan", "platform", "env=lab"]).success();
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
    let r = run(&["plan", "--why=none", "apps", "env=lab"]).success();
    assert!(r.stdout.contains("name = \"10.0.0.0/16\""), "{}", r.stdout);

    let r = run(&["apply", "platform", "env=lab", "--yes"]).success();
    assert!(
        r.stderr
            .contains("renamed storage platform[env=lab] → stacks.platform[env=lab]"),
        "{}",
        r.stderr
    );
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
    let keys = bucket.list("").unwrap();
    assert!(
        keys.iter().all(|k| k.starts_with("stacks.platform/")),
        "{keys:?}"
    );
    assert!(
        keys.iter()
            .any(|k| k == "stacks.platform/env=lab/state.json"),
        "{keys:?}"
    );
    let r = run(&["plan", "platform", "env=lab"]).success();
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
}
