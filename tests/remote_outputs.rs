//! Remote outputs: a project reads another project's stack outputs through
//! that project's backend, the project mounted as a package (`[packages]`
//! in dform.toml, R-65) and its stack used, `use platform.stacks.cluster`: the object a deployment publishes beside its state,
//! `outputs.json`, never the state. A plan records the digests of what it
//! read and `apply PLAN` refuses once one moved; a secret output crosses as
//! its label only.

mod common;
use common::Scratch;

const CLUSTER: &str = r#"
key env: string = "dev"
input token: secret(string)
use fake
output endpoint = "https://${env}.cluster.example"
output token: secret(string) = token
"#;

const APP: &str = r#"
use fake
use platform.stacks.cluster
resource net.vpc edge {
  name = e
} where e = cluster[env="prod"].endpoint
"#;

const SECRET: &str = "s3cr3t-token-value";

/// The upstream project, its `cluster[env=prod]` applied (`manifest` its
/// dform.toml); and the reader, which mounts it as the package
/// `platform`.
fn projects(name: &str, manifest: &str) -> (Scratch, Scratch) {
    let platform = Scratch::project(&format!("{name}-platform"));
    platform.write("dform.toml", manifest);
    platform.write("stacks/cluster.df", CLUSTER);
    let set = format!("token={SECRET}");
    platform
        .run(&["apply", "cluster", "env=prod", "--set", &set])
        .success();
    let app = Scratch::project(&format!("{name}-app"));
    app.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[packages.platform]\npath = {:?}\n",
            platform.dir.display().to_string()
        ),
    );
    app.write("stacks/app.df", APP);
    (platform, app)
}

#[test]
fn a_project_reads_another_projects_outputs_through_a_local_remote() {
    let (platform, app) = projects("remote-local", "[project]\nedition = \"2026\"\n");
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

    let r = app.run(&["plan", "--why=none", "app"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"edge\"]\n  name = \"https://prod.cluster.example\"\n"),
        "{}",
        r.stdout
    );
    app.run(&["apply", "app"]).success();
    let r = app.run(&["plan", "app"]).success();
    assert_eq!(r.summary(), "stack app is up to date", "{}", r.stdout);

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
            "- the outputs of platform.cluster[env=prod]: published again with other values \
             since the plan"
        ) && r.stderr.contains("stale plan"),
        "{}",
        r.stderr
    );
    // Planned again, it reads the new value.
    let r = app.run(&["plan", "app"]).success();
    assert!(
        r.stdout.contains(
            "name = \"https://prod.cluster.example\" → \"https://prod.cluster2.example\""
        ),
        "{}",
        r.stdout
    );
}

/// A package's backend may name each stack's place with `{stack}`; a
/// deployment the package has not applied has no outputs.
#[test]
fn a_remote_backend_takes_the_stack_name() {
    let (_platform, app) = projects(
        "remote-template",
        "[project]\nedition = \"2026\"\n\n[defaults]\nbackend = 'local(\"state/{stack}\")'\n",
    );
    let r = app.run(&["plan", "app"]).success();
    assert!(
        r.stdout.contains("name = \"https://prod.cluster.example\""),
        "{}",
        r.stdout
    );
    app.write(
        "stacks/app.df",
        &APP.replace("env=\"prod\"", "env=\"staging\""),
    );
    // Staging is not applied: what reads it waits on it (R-121).
    let r = app.run(&["plan", "app"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 create after platform.stacks.cluster[env=staging] is applied",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  waits on  stack platform.stacks.cluster[env=staging]\n"),
        "{}",
        r.stdout
    );
}

/// A secret output reaches the reader as its label: into a public field it
/// is the static secret error, and its bytes never cross.
#[test]
fn a_secret_output_cannot_be_read_into_a_public_field() {
    let (_platform, app) = projects("remote-secret", "[project]\nedition = \"2026\"\n");
    app.write("stacks/app.df", &APP.replace(".endpoint", ".token"));
    let r = app.run(&["plan", "app"]).failure();
    assert!(
        r.stderr
            .contains("E0304: a secret reaches net.vpc .name, not marked sensitive in the schema"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains(SECRET), "{}", r.stderr);
}

/// A package is a project: a path to none names no module.
#[test]
fn a_package_is_a_path_to_a_project() {
    let s = Scratch::project("remote-bad");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[packages.platform]\npath = \"../nowhere\"\n",
    );
    s.write("stacks/app.df", APP);
    let r = s.run(&["plan", "app"]).failure();
    assert!(
        r.stderr.contains("no module `platform.stacks.cluster`"),
        "{}",
        r.stderr
    );
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[packages.\"a.b\"]\npath = \"..\"\n",
    );
    let r = s.run(&["plan", "app"]).failure();
    assert!(
        r.stderr.contains("a package's name is the first segment"),
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
    let net = "\n\
               use fake\n\
               resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
               output c = main.cidr\n\
               output n = main.name\n\
               ";
    s.write("stacks/net.df", net);
    s.write(
        "stacks/app.df",
        "\n\
         use fake\n\
         use stacks.net as network\n\
         resource net.vpc edge { cidr = network.c }\n\
         resource net.vpc other { name = network.n }\n\
         ",
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

    let r = s.run(&["plan", "--why=none", "app"]).success();
    assert!(
        r.stdout.contains(
            "definite:\n+ net.vpc[\"edge\"]\n  cidr = \"10.0.0.0/16\"\npending on \
             ?net.n:\n+ net.vpc[\"other\"]\n  name = ?net.n\n"
        ),
        "{}",
        r.stdout
    );
    let r = s.run(&["apply", "app"]).failure();
    assert!(
        r.stderr
            .contains("nothing definite to apply, still waiting on net.n"),
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
    let r = s.run(&["apply", "app", "--why=none"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"other\"]\n  name = \"main\"\n"),
        "{}",
        r.stdout
    );
}
