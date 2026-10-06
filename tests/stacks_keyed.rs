//! Keyed stacks: `key env: T` makes each value of the key its own
//! deployment, with its own state, lock, registry entry and controller.
//! Inputs outside the key are parameters of a deployment.

mod common;
use common::{Scratch, repo};

/// A vpc whose cidr (force_new) and name depend on the key, and whose
/// size is a parameter.
const APP: &str = r#"
key env: enum("staging", "stg", "prod") = "staging"
input size: int = 1
use fake
net_of("staging", "10.1.0.0/16")
net_of("stg", "10.1.0.0/16")
net_of("prod", "10.2.0.0/16")
resource net.vpc main {
  name = "main-${env}"
  cidr = net_of[env]
  size = size
}
"#;

/// The headline: with one state for every environment, planning prod
/// after applying staging replaced staging's objects. Keyed, prod is its
/// own deployment: it creates.
#[test]
fn planning_prod_after_applying_staging_proposes_creates() {
    let s = Scratch::project("keyed-headline");
    s.write("app.df", APP);
    s.run(&["apply", "app.df", "env=staging"]).success();
    let prod = s.run(&["plan", "app.df", "env=prod"]).success();
    assert_eq!(
        prod.summary(),
        "plan: 1 change (1 create) over 1 tick",
        "{}",
        prod.stdout
    );
    // Staging is still what was applied; a parameter deforms it in place.
    let staging = s.run(&["plan", "app.df"]).success();
    assert_eq!(
        staging.summary(),
        "stack app is up to date",
        "{}",
        staging.stdout
    );
    let size = s
        .run(&["plan", "--why=none", "--set", "size=2", "app.df"])
        .success();
    assert!(
        size.stdout
            .contains("~ net.vpc[\"main\"]\n  size: 1 -> 2\n"),
        "{}",
        size.stdout
    );
}

/// dform.df is keyed by env: prod's plan after staging's apply creates.
#[test]
fn dform_df_plans_prod_after_staging_as_creates() {
    let s = Scratch::project("keyed-dform");
    let file = repo().join("examples/demo/stacks/dform.df");
    let file = file.to_str().unwrap();
    s.run(&["apply", file, "env=staging"]).success();
    assert!(s.path("dform.state/dform/env=staging/state.json").exists());
    let prod = s.run(&["plan", file, "env=prod"]).success();
    let summary = prod.summary();
    assert!(
        summary.starts_with("plan: ") && summary.ends_with(" create) over 1 tick"),
        "{}",
        prod.stdout
    );
}

/// Each deployment has its own directory and registry entry, named by the
/// key.
#[test]
fn the_key_names_the_state_and_the_registry_entry() {
    let s = Scratch::project("keyed-dirs");
    s.write("app.df", APP);
    s.run(&["apply", "app.df", "env=staging"]).success();
    s.run(&["apply", "app.df", "env=prod"]).success();
    assert!(s.path("dform.state/app/env=staging/state.json").exists());
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    assert!(!s.path("dform.state/app/state.json").exists());
    let registry: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/stacks.json")).unwrap();
    for (name, dir) in [
        ("app[env=staging]", "env=staging"),
        ("app[env=prod]", "env=prod"),
    ] {
        let state = registry[name].as_str().unwrap_or_default();
        assert!(
            state.ends_with(&format!("app/{dir}/state.json")),
            "{registry}"
        );
    }
}

/// A key value is escaped for the file system: `/`, a space, `=`, `,` and
/// a leading `.` never reach a path raw. Two keys are joined by `,`.
#[test]
fn key_values_are_escaped_and_joined() {
    let s = Scratch::project("keyed-escape");
    s.write(
        "app.df",
        r#"
key team: string
key region: string = "us-east1"
use fake
resource net.vpc main {
  name = "main-${team}-${region}"
}
"#,
    );
    s.run(&["apply", "app.df", "team=a/b c,=.x", "region=us-east1"])
        .success();
    assert!(
        s.path("dform.state/app/team=a%2Fb%20c%2C%3D.x,region=us-east1/state.json")
            .exists()
    );
    let registry = s.read("dform.state/stacks.json");
    assert!(
        registry.contains("\"app[team=a%2Fb%20c%2C%3D.x,region=us-east1]\""),
        "{registry}"
    );
    s.run(&["apply", "app.df", "team=..", "region=us-east1"])
        .success();
    assert!(
        s.path("dform.state/app/team=%2E.,region=us-east1/state.json")
            .exists()
    );
}

/// A key with no value is an error that names it and says to give it with
/// the target; a key is never `--set`.
#[test]
fn a_key_needs_a_value_from_the_target() {
    let s = Scratch::new("keyed-errors");
    s.write(
        "app.df",
        r#"
key env: string
use fake
"#,
    );
    let r = s.run(&["plan", "app.df"]).failure();
    assert!(
        r.stderr
            .contains("stack app is keyed by env, which has no value"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("`app env=...`"), "{}", r.stderr);
    let r = s.run(&["plan", "app.df", "--set", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "--set env=prod: env is stack app's key; name the deployment in the target: \
             `dform plan app env=prod`"
        ),
        "{}",
        r.stderr
    );
    s.run(&["plan", "app.df", "env=prod"]).success();
}

/// A key is a value: not a secret, and not inside a component.
#[test]
fn a_key_is_the_stacks_and_not_a_secret() {
    let s = Scratch::new("keyed-secret");
    s.write("app.df", "\nkey env: secret(string)\nuse fake\n");
    let r = s.run(&["plan", "app.df", "env=prod"]).failure();
    assert!(r.stderr.contains("key env is a secret"), "{}", r.stderr);
    assert!(r.stderr.contains("app.df:2:1"), "{}", r.stderr);
    s.write(
        "app.df",
        "\ncomponent m {\n  key env: string\n}\nuse fake\n",
    );
    let r = s.run(&["plan", "app.df"]).failure();
    assert!(r.stderr.contains("key env inside a block"), "{}", r.stderr);
}

/// A deployment is an instance of its stack (R-73): `app[env=prod].url` is
/// the keyed read of a copy's output, `instance_of` and `output`, those
/// facts served from what the deployment published, and `why` says so.
#[test]
fn a_deployment_read_is_the_keyed_read_of_an_instance() {
    let s = Scratch::project("keyed-instance");
    s.write(
        "stacks/app.df",
        "\nkey env: string = \"staging\"\nuse fake\noutput url = \"https://${env}.example\"\n",
    );
    s.write(
        "stacks/web.df",
        "\nuse fake\nuse stacks.app\nresource net.vpc edge { name = app[env=\"prod\"].url }\n",
    );
    s.run(&["apply", "app", "env=prod", "--yes"]).success();
    let r = s.run(&["why", "net.vpc[\"edge\"].name", "web"]).success();
    assert!(
        r.stdout
            .contains("resource stacks.app app[env=prod]   published by app[env=prod]\n")
            && r.stdout
                .contains("output app[env=prod].url = \"https://prod.example\"\n"),
        "{}",
        r.stdout
    );
    let r = s
        .run(&["why", "--core", "net.vpc[\"edge\"].name", "web"])
        .success();
    assert!(
        r.stdout.contains(
            "instance_of(\"stacks.app\", \"\", \"app[env=prod]\")   published by app[env=prod]"
        ),
        "{}",
        r.stdout
    );
}

/// `use stacks.app` binds a stack's deployments (R-65): `app[env=e].url`
/// reads one, each key given once by name; a keyed stack is never read
/// without its key, and never instanced.
#[test]
fn use_of_a_stack_reads_one_deployment() {
    let s = Scratch::project("keyed-use");
    s.write(
        "stacks/app.df",
        "\nkey env: string = \"staging\"\nuse fake\noutput url = \"https://${env}.example\"\n",
    );
    let web = |read: &str| {
        format!(
            "\nuse fake\nuse stacks.app\nresource net.vpc edge {{\n  name = u\n}} where u = {read}\n"
        )
    };
    s.write("stacks/web.df", &web("app[env=\"prod\"].url"));
    s.run(&["apply", "app", "env=prod"]).success();
    let r = s.run(&["plan", "--why=none", "web"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"edge\"]\n  name = \"https://prod.example\"\n"),
        "{}",
        r.stdout
    );
    s.write("stacks/web.df", &web("app.url"));
    let r = s.run(&["plan", "web"]).failure();
    assert!(
        r.stderr
            .contains("stacks.app is deployed, keyed by env: read an output of one deployment, `app[env=..].OUTPUT`"),
        "{}",
        r.stderr
    );
    s.write("stacks/web.df", &web("app[region=\"eu\"].url"));
    let r = s.run(&["plan", "web"]).failure();
    assert!(
        r.stderr
            .contains("a deployment of stacks.app is named by each of its keys once"),
        "{}",
        r.stderr
    );
}

/// `stack rekey` moves one deployment's state to another key value and
/// first lists what the new key renames; nothing else changes.
#[test]
fn rekey_lists_what_the_key_renames_and_moves_the_state() {
    let s = Scratch::project("keyed-rekey");
    s.write("app.df", APP);
    s.run(&["apply", "app.df", "env=staging"]).success();
    let r = s
        .run(&["stack", "rekey", "app", "env=staging", "env=stg"])
        .success();
    assert!(
        r.stdout.contains(
            "these name-like attributes depend on the key (env); the next plan of \
             app[env=stg] renames them, usually a replace:\n  net.vpc[\"main\"].name = \"main-staging\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!s.path("dform.state/app/env=staging").exists());
    assert!(s.path("dform.state/app/env=stg/state.json").exists());
    let registry = s.read("dform.state/stacks.json");
    assert!(registry.contains("\"app[env=stg]\""), "{registry}");
    assert!(!registry.contains("env=staging"), "{registry}");
    let r = s
        .run(&["plan", "--why=none", "app.df", "env=stg"])
        .success();
    assert!(
        r.stdout
            .contains("~ net.vpc[\"main\"]\n  name: \"main-staging\" -> \"main-stg\"\n"),
        "{}",
        r.stdout
    );
}

/// With no name that depends on the key, the rekeyed deployment is
/// undeformed.
#[test]
fn rekey_moves_state_and_the_next_plan_is_undeformed() {
    let s = Scratch::project("keyed-rekey-same");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.app]\nisolated = true\n",
    );
    s.write(
        "app.df",
        r#"
key env: string = "staging"
use fake
resource net.vpc main {
  name = "main"
}
"#,
    );
    s.run(&["apply", "app.df", "env=staging"]).success();
    let r = s
        .run(&["stack", "rekey", "app", "env=staging", "env=stg"])
        .success();
    assert!(
        r.stdout
            .contains("no name-like attribute depends on the key (env)"),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "app.df", "env=stg"]).success();
    assert_eq!(
        r.summary(),
        "stack app is up to date",
        "{}{}",
        r.stdout,
        r.stderr
    );
}

/// A stack keyed after it was applied: its state is not any deployment's,
/// and `rekey` with the new key only moves it.
#[test]
fn rekey_moves_the_state_from_before_the_stack_was_keyed() {
    let s = Scratch::project("keyed-legacy");
    let unkeyed = APP.replace("key env:", "input env:");
    s.write("app.df", &unkeyed);
    s.run(&["apply", "app.df"]).success();
    s.write("app.df", APP);
    s.run(&["stack", "rekey", "app", "env=staging"]).success();
    assert!(!s.path("dform.state/app/state.json").exists());
    let r = s.run(&["plan", "app.df"]).success();
    assert_eq!(r.summary(), "stack app is up to date", "{}", r.stdout);
}

const FIXED: &str = r#"
key env: string = "staging"
use fake
resource net.vpc logs {
  bucket = "company-logs"
}
resource net.vpc main {
  name = "main-${env}"
}
"#;

/// In a keyed stack, a name-like attribute that does not depend on the key
/// is written the same by every deployment: a warning at the field.
#[test]
fn a_fixed_bucket_name_in_a_keyed_stack_is_a_warning() {
    let s = Scratch::project("keyed-lint");
    s.write("app.df", FIXED);
    let r = s.run(&["plan", "app.df"]).success();
    assert!(
        r.stderr.contains(
            "warning: app.df:5:3: net.vpc[\"logs\"].bucket = \"company-logs\" does not depend on \
             the stack's key (env)"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("net.vpc[\"main\"]"), "{}", r.stderr);
    // Isolated deployments do not share names.
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.app]\nisolated = true\n",
    );
    let r = s.run(&["plan", "app.df"]).success();
    assert!(!r.stderr.contains("does not depend"), "{}", r.stderr);
}

/// A block that reads the key only to gate itself, or for another field,
/// still writes the same bucket in every deployment: the lint follows what
/// flows into the value, not what the rule reads. A ref to a name that
/// depends on the key does too.
const GATED: &str = r#"
key env: string = "staging"
use fake
resource net.vpc logs {
  bucket = "company-logs"
  tags = { env: env }
} where env != "dev"
resource net.vpc main {
  name = "main-${env}"
}
resource net.vpc peer {
  name = main.name
}
"#;

#[test]
fn a_fixed_bucket_in_a_block_that_reads_the_key_is_a_warning() {
    let s = Scratch::project("keyed-lint-gated");
    s.write("app.df", GATED);
    let r = s.run(&["plan", "app.df"]).success();
    assert!(
        r.stderr.contains(
            "warning: app.df:5:3: net.vpc[\"logs\"].bucket = \"company-logs\" does not depend on \
             the stack's key (env)"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("net.vpc[\"main\"]"), "{}", r.stderr);
    // A ref's value is its attribute's.
    assert!(!r.stderr.contains("net.vpc[\"peer\"]"), "{}", r.stderr);
    // Rekey lists what the key renames: main's name, not the bucket.
    s.run(&["apply", "app.df", "env=staging"]).success();
    let r = s
        .run(&["stack", "rekey", "app", "env=staging", "env=stg"])
        .success();
    assert!(
        r.stdout
            .contains("  net.vpc[\"main\"].name = \"main-staging\"\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("net.vpc[\"logs\"]"), "{}", r.stdout);
}

/// One controller per deployment: the target names the key value.
#[test]
fn the_controller_runs_one_deployment() {
    let s = Scratch::project("keyed-controller");
    s.write("app.df", APP);
    let r = s
        .run(&["controller", "run", "--once", "app.df", "env=prod"])
        .success();
    assert!(
        r.stdout.contains("controller app[env=prod]"),
        "{}",
        r.stdout
    );
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    assert!(!s.path("dform.state/app/env=staging").exists());
    // Unlike apply, the controller runs unattended: it takes no default,
    // it names every key value.
    let r = s.run(&["controller", "run", "--once", "app.df"]).failure();
    assert!(
        r.stderr
            .contains("controller run names its deployment: stack app is keyed by env"),
        "{}",
        r.stderr
    );
}

/// `handover` takes a deployment: its directory moves, the others stay.
#[test]
fn handover_takes_a_key() {
    let s = Scratch::project("keyed-handover");
    s.write("app.df", APP);
    s.run(&["apply", "app.df", "env=staging"]).success();
    s.run(&["apply", "app.df", "env=prod"]).success();
    let r = s
        .run(&[
            "stack",
            "handover",
            "app[env=prod]",
            "--to",
            "local(\"moved\")",
        ])
        .success();
    assert!(
        r.stdout.contains("stack app[env=prod] handed over"),
        "{}",
        r.stdout
    );
    assert!(s.path("moved/state.json").exists());
    assert!(s.path("dform.state/app/env=staging/state.json").exists());
    let r = s.run(&["apply", "app.df", "env=prod"]).failure();
    assert!(
        r.stderr.contains("stack app[env=prod] was handed over"),
        "{}",
        r.stderr
    );
    s.run(&["apply", "app.df", "env=staging"]).success();
}

/// Plan and apply say first which deployment they are of, and which key
/// values are defaults; `--json` names it too. Apply takes the default as
/// plan does.
#[test]
fn plan_and_apply_name_the_deployment_first() {
    let s = Scratch::project("keyed-deployment-line");
    s.write("app.df", APP);
    let r = s.run(&["plan", "app.df"]).success();
    assert!(
        r.stdout
            .starts_with("deployment: app[env=staging] (env from its default)\nplan: "),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "app.df", "env=prod"]).success();
    assert!(
        r.stdout.starts_with("deployment: app[env=prod]\nplan: "),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "--json", "app.df"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["deployment"], "app[env=staging]");
    assert_eq!(j["key_defaults"], serde_json::json!(["env"]));
    let r = s.run(&["plan", "--json", "app.df", "env=prod"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["deployment"], "app[env=prod]");
    assert_eq!(j["key_defaults"], serde_json::json!([]));

    let r = s.run(&["apply", "app.df"]).success();
    assert!(
        r.stdout
            .starts_with("deployment: app[env=staging] (env from its default)\n"),
        "{}",
        r.stdout
    );
    assert!(s.path("dform.state/app/env=staging/state.json").exists());
    assert!(!s.path("dform.state/app/env=prod").exists());
}

/// `apply` asks before it changes anything; with no terminal to ask on
/// it refuses at once, naming `--yes`, and applies nothing. `--yes` and
/// `-y` skip the question; an undeformed apply and `apply PLAN.json` ask
/// nothing.
#[test]
fn apply_asks_unless_yes() {
    let s = Scratch::project("keyed-confirm");
    s.write("app.df", APP);
    let apply = |args: &[&str]| {
        common::Run::from(
            common::dform()
                .args(args)
                .current_dir(&s.dir)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap(),
        )
    };
    let r = apply(&["apply", "app.df", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "apply app[env=prod]: nothing to ask on at tick 1 (stdin is not a terminal); \
             pass --yes to apply without asking"
        ),
        "{}",
        r.stderr
    );
    // The plan was shown; nothing was applied.
    assert!(r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
    assert!(!s.path("dform.state/app/env=prod/state.json").exists());

    apply(&["apply", "-y", "app.df", "env=prod"]).success();
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    // Undeformed: nothing to confirm.
    let r = apply(&["apply", "app.df", "env=prod"]).success();
    assert!(r.stdout.contains("stack app is up to date"), "{}", r.stdout);

    // A reviewed plan file is applied without asking.
    apply(&["plan", "--out", "stg.json", "app.df", "env=stg"]).success();
    apply(&["apply", "stg.json"]).success();
    assert!(s.path("dform.state/app/env=stg/state.json").exists());
    apply(&["apply", "--yes", "app.df"]).success();
    assert!(s.path("dform.state/app/env=staging/state.json").exists());
}

/// A key input that defaults to production is a lint warning: a run that
/// names no value of the key is of production.
#[test]
fn a_key_defaulting_to_production_is_warned() {
    let s = Scratch::project("keyed-prod-default");
    s.write(
        "app.df",
        &APP.replace(
            r#"key env: enum("staging", "stg", "prod") = "staging""#,
            r#"key env: enum("staging", "stg", "prod") = "prod""#,
        ),
    );
    let r = s.run(&["plan", "app.df"]).success();
    assert!(
        r.stderr.contains(
            "warning: app.df:2:1: key env of stack app defaults to \"prod\": a plan \
             or apply that names no env is of app[env=prod]; default to another value, or give none"
        ),
        "{}",
        r.stderr
    );
    let r = s.run(&["plan", "app.df", "env=stg"]).success();
    assert!(r.stderr.contains("defaults to \"prod\""), "{}", r.stderr);
    s.write("app.df", APP);
    let r = s.run(&["plan", "app.df"]).success();
    assert!(!r.stderr.contains("defaults to"), "{}", r.stderr);
}

/// `state show` (and `log`) read the deployment's own objects: they need
/// its key, not the program's other inputs (a required secret here).
#[test]
fn state_show_needs_the_key_not_the_other_inputs() {
    let s = Scratch::project("keyed-state-show");
    s.write(
        "stacks/app.df",
        "\n\
         key env: string\n\
         input pw: secret(string)\n\
         use fake\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
         ",
    );
    s.run(&["apply", "app", "env=prod", "--set", "pw=x"])
        .success();
    let r = s.run(&["state", "show", "app", "env=prod"]).success();
    assert!(
        r.stdout.contains("\nnet.vpc[\"main\"]  fakecloud  main\n"),
        "{}",
        r.stdout
    );
    s.run(&["log", "app", "env=prod"]).success();
    let r = s.run(&["state", "show", "app"]).failure();
    assert!(
        r.stderr
            .contains("stack app is keyed by env, which has no value"),
        "{}",
        r.stderr
    );
}
