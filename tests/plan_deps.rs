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

/// `plan X --out F` writes the closure in apply order (R-200 after, the
/// user's decision of 2026-10-09): each deployment by its full name, its
/// plan and digest, what it is applied after and what it read; `apply F`
/// applies them in turn, the reader's plan, made against its dependency's
/// planned outputs, valid once that dependency's apply produced them.
#[test]
fn a_plan_file_carries_the_closure_and_apply_applies_it_in_order() {
    let s = project("deps-plan-file");
    let r = s
        .run(&["plan", "apps", "env=lab", "--out", "p.json"])
        .success();
    assert!(
        r.stderr
            .contains("plan file: p.json (plan digests: stacks.platform[env=lab] sha256:"),
        "{}",
        r.stderr
    );
    let f = s.json("p.json");
    assert_eq!(f["version"], 5, "{f:#}");
    let steps = f["deployments"].as_array().unwrap();
    let names: Vec<&str> = steps
        .iter()
        .map(|d| d["deployment"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["stacks.platform[env=lab]", "stacks.apps[env=lab]"]);
    assert_eq!(
        steps[1]["after"],
        serde_json::json!(["stacks.platform[env=lab]"])
    );
    assert!(steps.iter().all(|d| d["digest"].is_string()), "{f:#}");
    assert_eq!(
        steps[1]["inputs"]["stack_outputs"],
        serde_json::json!([{"deployment": "platform[env=lab]", "digest": "absent"}]),
        "{f:#}"
    );
    // `--json` of the closure is one document of both.
    let j: serde_json::Value = serde_json::from_str(
        &s.run(&["plan", "apps", "env=lab", "--json", "--out", "q.json"])
            .success()
            .stdout,
    )
    .unwrap();
    assert_eq!(j["deployments"].as_array().unwrap().len(), 2, "{j:#}");
    assert_eq!(
        j["deployments"][1]["plan"]["digest"], steps[1]["digest"],
        "{j:#}"
    );

    let r = s.run(&["apply", "p.json", "--yes"]).success();
    assert!(
        r.stdout.starts_with(
            "stacks: stacks.platform[env=lab], then stacks.apps[env=lab], in apply order as \
             p.json planned them; each is confirmed and applied in turn\n== \
             stacks.platform[env=lab]\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("== stacks.apps[env=lab]\n"),
        "{}",
        r.stdout
    );
    assert!(
        s.path("dform.state/stacks.platform/env=lab/state.json")
            .exists()
    );
    let world = s.read("dform.state/stacks.apps/env=lab/remote.json");
    assert!(world.contains("db.db.fake"), "{world}");
    let r = s.run(&["plan", "apps", "env=lab"]).success();
    assert_eq!(r.summary(), "plan: 0 changes", "{}", r.stdout);
}

/// One confirmation per deployment: answered no for the reader, the
/// dependency is applied and the reader is not, exit 3; a deployment
/// that reads none writes a file of one, applied as before, unasked.
#[test]
fn a_plan_file_s_deployments_are_each_confirmed() {
    let s = project("deps-plan-file-ask");
    s.run(&["plan", "apps", "env=lab", "--out", "p.json"])
        .success();
    let mut cmd = common::dform();
    cmd.args(["apply", "p.json"]).current_dir(s.path(""));
    let (said, code) = common::answering(&s.dir, cmd, &["y", "n"]);
    assert_eq!(code, 3, "{}", said[0]);
    assert!(
        said[0].contains("Apply these 2 changes to platform[env=lab]?"),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("Apply these 2 changes to apps[env=lab]?"),
        "{}",
        said[1]
    );
    assert!(
        s.path("dform.state/stacks.platform/env=lab/state.json")
            .exists()
    );
    assert!(
        !s.path("dform.state/stacks.apps/env=lab/state.json")
            .exists()
    );

    let s = project("deps-plan-file-one");
    s.run(&["plan", "platform", "env=lab", "--out", "p.json"])
        .success();
    let f = s.json("p.json");
    assert_eq!(f["deployments"].as_array().unwrap().len(), 1, "{f:#}");
    assert_eq!(
        f["deployments"][0]["deployment"],
        "stacks.platform[env=lab]"
    );
    s.run(&["apply", "p.json"]).success();
}

/// Each deployment is checked against its plan at its turn: one whose
/// state moved since the plan stops the sequence there, its status the
/// command's, and nothing after it is applied.
#[test]
fn a_plan_file_stops_at_the_first_stale_deployment() {
    let s = project("deps-plan-file-stale");
    s.run(&["plan", "apps", "env=lab", "--out", "p.json"])
        .success();
    // Someone applies the platform with another cidr in between.
    s.write(
        "stacks/platform.df",
        &PLATFORM.replace("10.0.0.0/16", "10.9.0.0/16"),
    );
    s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    s.write("stacks/platform.df", PLATFORM);
    let r = s.run(&["apply", "p.json", "--yes"]);
    assert_eq!(r.code, Some(1), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("plan file p.json is stale"),
        "{}",
        r.stderr
    );
    assert!(
        !r.stdout.contains("== stacks.apps[env=lab]"),
        "{}",
        r.stdout
    );
    assert!(
        !s.path("dform.state/stacks.apps/env=lab/state.json")
            .exists()
    );
}
