//! A targeted plan covers the target's dependencies, as a targeted apply
//! does (R-200, R-30): `dform plan apps env=lab` plans
//! stacks.platform[env=lab] first, then apps against platform's planned
//! outputs, one tree; a dependency up to date is one line; the chain stops
//! where the apply would.

mod common;
use common::Scratch;

/// Its address it knows before an apply (`cidr`); its endpoint the
/// provider computes.
const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
resource db.postgres db { name = "db" }
output cidr = edge.cidr
output endpoint = db.endpoint
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc known { cidr = "10.1.0.0/16", name = platform[env].cidr }
resource net.vpc later { cidr = "10.2.0.0/16", name = platform[env].endpoint }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s
}

/// The tree's header lines, whitespace folded.
fn headers(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| ["+ ", "~ ", "= ", "- "].iter().any(|m| l.starts_with(m)))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn a_dependency_never_applied_is_planned_first_and_its_known_outputs_flow() {
    let s = project("deps-never");
    let r = s.run(&["plan", "apps", "env=lab"]).success();
    assert_eq!(
        r.summary(),
        "plan: 3 changes (3 create); 1 create after stacks.platform[env=lab] is applied",
        "{}",
        r.stdout
    );
    assert_eq!(
        headers(&r.stdout),
        [
            "+ stacks.platform[env=lab] stacks/platform.df never applied, 2 changes (2 create) over 1 tick",
            "+ stacks.apps[env=lab] stacks/apps.df never applied, 1 change (1 create) over 1 tick; 1 create after stacks.platform[env=lab] is applied",
        ],
        "{}",
        r.stdout
    );
    // The value platform's plan knows flows; the one its apply computes
    // waits on that apply.
    assert!(
        r.stdout.contains(
            "    + net.vpc known  stacks/apps.df:5\n        cidr = \"10.1.0.0/16\"\n        name = \"10.0.0.0/16\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  later\n    waits on  stack stacks.platform[env=lab]\n    + net.vpc later  "
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("deployment: "), "{}", r.stdout);
    assert!(!r.stdout.contains("== "), "{}", r.stdout);
}

#[test]
fn a_dependency_up_to_date_is_one_line() {
    let s = project("deps-up-to-date");
    s.run(&["apply", "platform", "env=lab"]).success();
    let r = s.run(&["plan", "apps", "env=lab"]).success();
    assert_eq!(r.summary(), "plan: 2 changes (2 create)", "{}", r.stdout);
    assert!(
        r.stdout.starts_with(
            "plan: 2 changes (2 create)\n\n\
             = stacks.platform[env=lab]  stacks/platform.df  up to date\n\n\
             + stacks.apps[env=lab]"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("name = \"db.db.fake\""), "{}", r.stdout);
}

#[test]
fn a_dependency_s_plan_failing_stops_the_chain() {
    let s = project("deps-failing");
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}let broken = io.read(\"missing.txt\")\nresource net.vpc x {{ cidr = broken }}\n"),
    );
    let r = s.run(&["plan", "apps", "env=lab"]);
    assert_eq!(r.code, Some(1), "{}\n{}", r.stdout, r.stderr);
    assert_eq!(
        headers(&r.stdout),
        [
            "+ stacks.platform[env=lab] stacks/platform.df failed",
            "+ stacks.apps[env=lab] stacks/apps.df not planned: stacks.platform[env=lab] failed",
        ],
        "{}",
        r.stdout
    );
    assert!(r.stderr.contains("missing.txt"), "{}", r.stderr);
    assert!(!r.stdout.contains("net.vpc known"), "{}", r.stdout);
}

/// A dependency whose file does not parse: its reader's run says the
/// parse error, where it is, not what reading a stack with no outputs
/// would be.
#[test]
fn a_dependency_that_does_not_parse_is_its_parse_error() {
    let s = project("deps-unparsed");
    s.write(
        "stacks/platform.df",
        &PLATFORM.replace("cidr = \"10.0.0.0/16\" }", "cidr = \"10.0.0.0/16\""),
    );
    for args in [
        &["plan", "apps", "env=lab"][..],
        &["apply", "apps", "env=lab", "--yes"],
    ] {
        let r = s.run(args);
        assert_eq!(r.code, Some(1), "{}\n{}", r.stdout, r.stderr);
        assert!(r.stderr.contains("stacks/platform.df:"), "{}", r.stderr);
        assert!(!r.stderr.contains("is deployed"), "{}", r.stderr);
    }
}

/// A stack that reads no other plans as it did: its deployment line,
/// its headline, its ticks.
#[test]
fn a_stack_that_reads_none_plans_alone() {
    let s = project("deps-alone");
    let r = s.run(&["plan", "platform", "env=lab"]).success();
    assert!(
        r.stdout.starts_with(
            "deployment: stacks.platform[env=lab]\nplan: 2 changes (2 create) over 1 tick\n"
        ),
        "{}",
        r.stdout
    );
}

/// A target by its full name is the stack its short name is; a keyed
/// read by the full name reads the deployment as the short one does.
#[test]
fn a_stack_is_named_by_its_full_name_or_its_short_one() {
    let s = project("deps-full-name");
    s.write(
        "stacks/apps.df",
        &APPS.replace("platform[env].cidr", "stacks.platform[env].cidr"),
    );
    let short = s.run(&["plan", "apps", "env=lab"]).success();
    let full = s.run(&["plan", "stacks.apps", "env=lab"]).success();
    assert_eq!(short.stdout, full.stdout);
    assert!(
        full.stdout.contains("name = \"10.0.0.0/16\""),
        "{}",
        full.stdout
    );
    let list = s.run(&["stack", "list"]).success();
    assert!(
        list.stdout.contains("stacks.apps[env]      stacks/apps.df"),
        "{}",
        list.stdout
    );
}

/// `--json` nests the same: the tree's summary, each deployment's line,
/// and its own plan document under it.
#[test]
fn json_nests_each_deployment_s_plan() {
    let s = project("deps-json");
    let r = s.run(&["plan", "apps", "env=lab", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    assert_eq!(
        j["summary"],
        "plan: 3 changes (3 create); 1 create after stacks.platform[env=lab] is applied",
        "{j:#}"
    );
    let names: Vec<&str> = j["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["deployment"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["stacks.platform[env=lab]", "stacks.apps[env=lab]"],
        "{j:#}"
    );
    assert_eq!(
        j["deployments"][1]["plan"]["deployment"], "stacks.apps[env=lab]",
        "{j:#}"
    );
    assert_eq!(j["outcome"], "done", "{j:#}");
}

/// Not yet (R-200): `plan X --out F` writes the target's plan file alone.
/// The closure's file needs a design: the reader's plan was made against
/// its dependency's *planned* outputs, which its file's outputs digest
/// cannot match until that dependency is applied, so `apply F` would
/// refuse it as stale. A decision for the ticket.
#[test]
#[ignore = "R-200: the plan file does not carry the closure; `apply PLAN` applies one deployment"]
fn a_plan_file_carries_the_closure_and_apply_applies_it_in_order() {
    let s = project("deps-plan-file");
    s.run(&["plan", "apps", "env=lab", "--out", "p.json"])
        .success();
    let r = s.run(&["apply", "p.json", "--yes"]).success();
    assert!(
        r.stdout.contains("stacks.platform[env=lab]"),
        "{}",
        r.stdout
    );
    assert!(s.path("dform.state/platform/env=lab/state.json").exists());
    assert!(s.path("dform.state/apps/env=lab/state.json").exists());
}

/// Not yet (R-200): state, the registry and outputs.json name a
/// deployment by its short name (`dform.state/platform/env=lab`); moving
/// them to the full name moves applied state, a migration of its own.
#[test]
#[ignore = "R-200: state keeps a deployment's short name"]
fn state_names_a_deployment_by_its_full_name() {
    let s = project("deps-state-name");
    s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    let published = s.read("dform.state/platform/env=lab/outputs.json");
    assert!(
        published.contains("\"stacks.platform[env=lab]\""),
        "{published}"
    );
}
