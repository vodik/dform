//! Remote outputs: a project reads another project's stack outputs through
//! that project's backend (`[remotes]` in dform.toml, README "Remote
//! outputs"): the object a deployment publishes beside its state,
//! `outputs.json`, never the state. A plan records the digests of what it
//! read and `apply PLAN` refuses once one moved; a secret output crosses as
//! its label only.

mod common;
use common::Scratch;

const CLUSTER: &str = r#"edition 2026
input env: string = "dev"
input token: secret(string)
stack cluster[env] {}
output endpoint = "https://{env}.cluster.example"
output token: secret(string)
output token = token
"#;

const APP: &str = r#"edition 2026
stack app {}
resource net.vpc edge {
  for stack_output("platform.cluster[env=prod]", "endpoint", e)
  name = e
}
"#;

const SECRET: &str = "s3cr3t-token-value";

/// The upstream project, its `cluster[env=prod]` applied; and the reader,
/// whose `[remotes]` names it (`remotes` is the manifest's table, with
/// `PLATFORM` the upstream's state root).
fn projects(name: &str, remotes: &str) -> (Scratch, Scratch) {
    let platform = Scratch::project(&format!("{name}-platform"));
    platform.write("stacks/cluster.df", CLUSTER);
    let set = format!("token={SECRET}");
    platform
        .run(&["apply", "cluster", "env=prod", "--set", &set])
        .success();
    let app = Scratch::project(&format!("{name}-app"));
    let root = platform.path("dform.state").display().to_string();
    app.write(
        "dform.toml",
        &format!("[remotes]\n{}", remotes.replace("PLATFORM", &root)),
    );
    app.write("stacks/app.df", APP);
    (platform, app)
}

#[test]
fn a_project_reads_another_projects_outputs_through_a_local_remote() {
    let (platform, app) = projects(
        "remote-local",
        "platform = { backend = 'local(\"PLATFORM\")' }\n",
    );
    // What crosses: the outputs object, a secret by its label only.
    let published = platform.read("dform.state/cluster/env=prod/outputs.json");
    assert!(
        published.contains("\"deployment\": \"cluster[env=prod]\""),
        "{published}"
    );
    assert!(
        published.contains("\"label\": \"output/#token\""),
        "{published}"
    );
    assert!(!published.contains(SECRET), "{published}");

    let r = app.run(&["plan", "app"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"edge\"]\n  name = \"https://prod.cluster.example\"\n"),
        "{}",
        r.stdout
    );
    app.run(&["apply", "app"]).success();
    let r = app.run(&["plan", "app"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);

    // A saved plan records the digest of what it read; the upstream's
    // output moves, and the plan is stale.
    app.write(
        "stacks/app.df",
        &APP.replace("name = e", "name = e\n  cidr = \"10.1.0.0/16\""),
    );
    app.run(&["plan", "app", "--out", "plan.json"]).success();
    let plan = app.read("plan.json");
    assert!(
        plan.contains("\"deployment\": \"platform.cluster[env=prod]\""),
        "{plan}"
    );
    platform.write(
        "stacks/cluster.df",
        &CLUSTER.replace("cluster.example", "cluster2.example"),
    );
    let set = format!("token={SECRET}");
    platform
        .run(&["apply", "cluster", "env=prod", "--set", &set])
        .success();
    let r = app.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "- stack_output of platform.cluster[env=prod]: its published outputs changed since \
             the plan"
        ) && r.stderr.contains("stale plan"),
        "{}",
        r.stderr
    );
    // Planned again, it reads the new value.
    let r = app.run(&["plan", "app"]).success();
    assert!(
        r.stdout.contains(
            "name: \"https://prod.cluster.example\" -> \"https://prod.cluster2.example\""
        ),
        "{}",
        r.stdout
    );
}

/// A remote's backend may name each stack's place with `{stack}`; a
/// deployment the remote has not applied has no outputs.
#[test]
fn a_remote_backend_takes_the_stack_name() {
    let (_platform, app) = projects(
        "remote-template",
        "platform = { backend = 'local(\"PLATFORM/{stack}\")' }\n",
    );
    let r = app.run(&["plan", "app"]).success();
    assert!(
        r.stdout.contains("name = \"https://prod.cluster.example\""),
        "{}",
        r.stdout
    );
    app.write("stacks/app.df", &APP.replace("env=prod", "env=staging"));
    let r = app.run(&["plan", "app"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);
}

/// A secret output reaches the reader as its label: into a public field it
/// is the static secret error, and its bytes never cross.
#[test]
fn a_secret_output_cannot_be_read_into_a_public_field() {
    let (_platform, app) = projects(
        "remote-secret",
        "platform = { backend = 'local(\"PLATFORM\")' }\n",
    );
    app.write("stacks/app.df", &APP.replace("\"endpoint\"", "\"token\""));
    let r = app.run(&["plan", "app"]).failure();
    assert!(
        r.stderr
            .contains("E0304: a secret reaches net.vpc .name, not marked sensitive in the schema"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains(SECRET), "{}", r.stderr);
}

/// A remote named in the manifest is checked there.
#[test]
fn a_remote_needs_a_backend_term() {
    let s = Scratch::project("remote-bad");
    s.write(
        "dform.toml",
        "[remotes]\nplatform = { backend = 'gcs(\"x\")' }\n",
    );
    s.write("stacks/app.df", APP);
    let r = s.run(&["plan", "app"]).failure();
    assert!(
        r.stderr
            .contains("[remotes] platform backend = \"gcs(\\\"x\\\")\""),
        "{}",
        r.stderr
    );
}

/// An output of a configured attribute (`net.vpc.main.cidr`, a ref the
/// program keeps) is published resolved, as the program set it; one nobody
/// knows yet is published pending, and its reader waits on the null until
/// the output is known.
#[test]
fn an_output_of_a_configured_attribute_is_published_resolved_or_pending() {
    let s = Scratch::project("outputs-resolved");
    let net = "edition 2026\nstack net {}\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
               output c = net.vpc.main.cidr\noutput n = net.vpc.main.name\n";
    s.write("stacks/net.df", net);
    s.write(
        "stacks/app.df",
        "edition 2026\nstack app {}\nresource net.vpc edge {\n  \
         for stack_output(\"net\", \"c\", c)\n  cidr = c\n}\nresource net.vpc other {\n  \
         for stack_output(\"net\", \"n\", n)\n  name = n\n}\n",
    );
    s.run(&["apply", "net"]).success();
    let published: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/net/outputs.json")).unwrap();
    assert_eq!(
        published["outputs"]["c"],
        serde_json::json!({"t": "Str", "v": "10.0.0.0/16"}),
        "{published}"
    );
    assert_eq!(
        published["pending"],
        serde_json::json!(["n"]),
        "{published}"
    );

    let r = s.run(&["plan", "app"]).success();
    assert!(
        r.stdout.contains(
            "definite:\n+ net.vpc[\"edge\"]\n  cidr = \"10.0.0.0/16\"\npending on \
             ?stack_output[\"net\"].n:\n+ net.vpc[\"other\"]\n  name = ?stack_output[\"net\"].n\n"
        ),
        "{}",
        r.stdout
    );
    let r = s.run(&["apply", "app"]).failure();
    assert!(
        r.stderr
            .contains("nothing definite to apply, still waiting on ?stack_output[\"net\"].n"),
        "{}",
        r.stderr
    );

    // The producer sets it: published, and the reader applies.
    s.write(
        "stacks/net.df",
        &net.replace(
            "cidr = \"10.0.0.0/16\"",
            "cidr = \"10.0.0.0/16\"\n  name = \"main\"",
        ),
    );
    s.run(&["apply", "net"]).success();
    let r = s.run(&["apply", "app"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"other\"]\n  name = \"main\"\n"),
        "{}",
        r.stdout
    );
}
