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
            "+ stacks.apps[env=lab] stacks/apps.df never applied, 1 change (1 create) over 1 tick; 1 create after stacks.platform[env=lab] is applied after stacks.platform[env=lab]",
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
