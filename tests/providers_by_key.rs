//! Provider configuration chosen by the deployment key: a `provider` block
//! reads the key, inputs a `set` gives and `env.var`, the provider reports the
//! account its credentials reach, and `expect_account` refuses a plan
//! that would reach another deployment's.

mod common;
use common::{Backend, Run, Scratch};

/// The mock (`fake`) configured per env: its account from the
/// environment, the account each env expects from its `set`.
const APP: &str = r#"
key env: environment = "dev"
input cloud { account: string, region: string }
type environment = enum("dev", "prod")
provider env
provider fake {
  account = env.var("FAKE_ACCOUNT_${env}")
  region = cloud.region
  expect_account = cloud.account
}
set { cloud.account = "acct-dev", cloud.region = "r-dev" } where env == "dev"
set { cloud.account = "acct-prod", cloud.region = "r-prod" } where env == "prod"
resource net.vpc main {
  cidr = "10.0.0.0/16"
}
"#;

fn project(name: &str, program: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/app.df", program);
    s
}

/// `dform ARGS` in `s` with the environment variables `env`.
fn run_with_env(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = Backend::Process.command();
    c.args(args).current_dir(&s.dir);
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

const RIGHT: &[(&str, &str)] = &[
    ("FAKE_ACCOUNT_dev", "acct-dev"),
    ("FAKE_ACCOUNT_prod", "acct-prod"),
];

#[test]
fn each_deployment_configures_the_provider_from_its_key() {
    let s = project("bykey-configured", APP);
    for env in ["env=dev", "env=prod"] {
        let r = run_with_env(&s, RIGHT, &["plan", "stacks/app.df", env]).success();
        assert_eq!(
            r.summary(),
            "plan: 1 change (1 create) over 1 tick",
            "{}",
            r.stdout
        );
    }
}

/// The same shell pointing prod's plan at dev's account: refused before
/// anything is planned, naming both accounts and the deployment.
#[test]
fn a_mismatched_account_refuses_to_plan() {
    let s = project("bykey-mismatch", APP);
    let wrong = [("FAKE_ACCOUNT_prod", "acct-dev")];
    let r = run_with_env(&s, &wrong, &["plan", "stacks/app.df", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "provider fake reports account acct-dev, but the program expects acct-prod \
             (expect_account)"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("deployment app[env=prod]"),
        "{}",
        r.stderr
    );
    assert!(!r.stdout.contains("plan:"), "{}", r.stdout);
}

/// A provider that cannot tell its account (the mock with no `account`
/// setting) is refused when the program expects one.
#[test]
fn an_expected_account_the_provider_does_not_report_is_refused() {
    let s = project(
        "bykey-silent",
        "\n\
         provider fake { expect_account = \"acct\" }\n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    let r = s.run(&["plan", "stacks/app.df"]).failure();
    assert!(
        r.stderr
            .contains("provider fake reports no account, but the program expects acct"),
        "{}",
        r.stderr
    );
}

/// A provider configured from a value it serves itself is a compile error
/// naming the chain.
#[test]
fn a_provider_configured_from_what_it_serves_is_a_cycle() {
    let s = project(
        "bykey-cycle",
        "\n\
         provider fake { zone = z }\n\
         let z = main.cidr\n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    let r = s.run(&["plan", "stacks/app.df"]).failure();
    assert!(
        r.stderr.contains(
            "provider fake is configured from net.vpc[\"main\"].cidr, which it serves itself: a cycle"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "the cycle: provider fake's configuration -> reads z -> reads net.vpc[\"main\"].cidr \
             -> which provider fake serves"
        ),
        "{}",
        r.stderr
    );
}

/// `env.var` is a secret: the plan file names the variable by its label
/// and never holds its value, nor does state.
#[test]
fn an_env_var_is_in_the_plan_file_only_as_its_label() {
    let s = project(
        "bykey-label",
        "\n\
         provider env\n\
         provider fake { token = env.var(\"FAKE_TOKEN\") }\n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    let env = [("FAKE_TOKEN", "tok-5ecret")];
    run_with_env(&s, &env, &["plan", "--out", "plan.json", "stacks/app.df"]).success();
    let f = s.read("plan.json");
    let j: serde_json::Value = serde_json::from_str(&f).unwrap();
    let env_in = &j["inputs"]["env"];
    assert_eq!(env_in[0]["sensitive"], "env.var/FAKE_TOKEN", "{f}");
    assert!(env_in[0]["digest"].is_string(), "{f}");
    assert!(!f.contains("tok-5ecret"), "{f}");
    run_with_env(&s, &env, &["apply", "plan.json"]).success();
    let state = s.read("dform.state/app/state.json");
    assert!(
        !state.contains("tok-5ecret") && !state.contains("FAKE_TOKEN"),
        "{state}"
    );
    // Unset, it is an error naming the variable.
    let r = s.run(&["plan", "stacks/app.df"]).failure();
    assert!(
        r.stderr
            .contains("env.var: FAKE_TOKEN is not set in the environment"),
        "{}",
        r.stderr
    );
}

/// A variable changed between plan and apply makes the saved plan stale:
/// the plan file holds its digest keyed with the stack's plan key.
#[test]
fn a_changed_env_var_makes_a_saved_plan_stale() {
    let s = project(
        "bykey-stale",
        "\n\
         provider env\n\
         provider fake { token = env.var(\"FAKE_TOKEN\") }\n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    run_with_env(
        &s,
        &[("FAKE_TOKEN", "tok-one")],
        &["plan", "--out", "plan.json", "stacks/app.df"],
    )
    .success();
    let r = run_with_env(&s, &[("FAKE_TOKEN", "tok-two")], &["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("env.var/FAKE_TOKEN: changed since the plan"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("tok-"), "{}", r.stderr);
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("env.var/FAKE_TOKEN: in the plan file, not set now"),
        "{}",
        r.stderr
    );
    run_with_env(&s, &[("FAKE_TOKEN", "tok-one")], &["apply", "plan.json"]).success();
}

/// An expected account read from the environment is a secret: the
/// refusal names it by its label, never its value.
#[test]
fn a_secret_expected_account_is_refused_by_its_label() {
    let s = project(
        "bykey-secret-account",
        "\n\
         provider env\n\
         provider fake {\n\
           account = \"acct-real\"\n\
           expect_account = env.var(\"WANT_ACCOUNT\")\n\
         }\n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    let r = run_with_env(
        &s,
        &[("WANT_ACCOUNT", "hunter2")],
        &["plan", "stacks/app.df"],
    )
    .failure();
    assert!(!r.stderr.contains("hunter2"), "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "provider fake reports account acct-real, but the program expects \
             provider/fake#expect_account (a secret)"
        ),
        "{}",
        r.stderr
    );
}
