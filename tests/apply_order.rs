//! The stack is the unit of partial work (R-30): `apply X` applies the
//! deployments X reads (`use stacks.net as network`, then
//! `network[env=env].cidr`) first, each its own run with its
//! own plan, confirmation and state, then X, and nothing that reads X.

mod common;
use common::Scratch;

const NET: &str = r#"edition 2026
key env: string = "dev"
provider fake
resource net.vpc main { cidr = "10.0.0.0/16" }
output cidr = main.cidr
"#;

const APP: &str = r#"edition 2026
key env: string = "dev"
provider fake
use stacks.net as network
resource net.subnet a {
  cidr = c
} where c = network[env=env].cidr
output subnet = a.cidr
"#;

const WEB: &str = r#"edition 2026
provider fake
use stacks.app
resource net.subnet w {
  cidr = c
} where c = app[env="prod"].subnet
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/net.df", NET);
    s.write("stacks/app.df", APP);
    s.write("stacks/web.df", WEB);
    s
}

#[test]
fn apply_applies_what_the_stack_reads_first() {
    let s = project("order");
    let r = s.run(&["apply", "app", "env=prod"]).success();
    assert!(
        r.stdout.starts_with(
            "apply app[env=prod]: net[env=prod] first, each with its own plan and state: \
             app[env=prod] reads its outputs\n== net[env=prod]\ndeployment: net[env=prod]\n"
        ),
        "{}",
        r.stdout
    );
    let app = r
        .stdout
        .split("== app[env=prod]\n")
        .nth(1)
        .unwrap_or_default();
    assert!(
        app.contains("+ net.subnet[\"a\"]\n  cidr = \"10.0.0.0/16\"\n"),
        "{}",
        r.stdout
    );
    assert!(s.path("dform.state/net/env=prod/state.json").exists());
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    // Nothing that reads app, nor another deployment of net.
    assert!(!s.path("dform.state/web").exists());
    assert!(!s.path("dform.state/net/env=dev").exists());

    // Through two stacks: web reads app, which reads net.
    let s = project("order-web");
    let r = s.run(&["apply", "web"]).success();
    assert!(
        r.stdout
            .starts_with("apply web: net[env=prod], then app[env=prod] first"),
        "{}",
        r.stdout
    );
    let world = s.read("dform.state/web/remote.json");
    assert!(world.contains("10.0.0.0/16"), "{world}");
}

/// A dependency's apply that fails stops the run before its reader.
#[test]
fn a_failed_dependency_stops_the_apply() {
    let s = project("order-fail");
    s.write(
        "stacks/net.df",
        &format!("{NET}deny \"no net in prod\" where env == \"prod\"\n"),
    );
    let r = s.run(&["apply", "app", "env=prod"]).failure();
    assert!(r.stderr.contains("no net in prod"), "{}", r.stderr);
    assert!(!r.stdout.contains("== app[env=prod]"), "{}", r.stdout);
    assert!(!s.path("dform.state/app").exists());
}

#[test]
fn stacks_that_read_each_other_are_a_cycle() {
    let s = project("order-cycle");
    s.write(
        "stacks/net.df",
        &format!("{NET}tag(t) where stack_output(\"app[env=${{env}}]\", \"subnet\", t)\n"),
    );
    let r = s.run(&["apply", "app", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "apply app[env=prod]: the stacks read each other's outputs in a cycle: \
             app[env=prod] -> net[env=prod] -> app[env=prod]"
        ),
        "{}",
        r.stderr
    );
}

/// A `--set` goes to each stack of the run that declares the input.
#[test]
fn a_set_goes_to_the_stack_that_declares_it() {
    let s = project("order-set");
    s.write(
        "stacks/net.df",
        &NET.replace("provider fake\n", "input cidr: string\nprovider fake\n")
            .replace("{ cidr = \"10.0.0.0/16\" }", "{ cidr }"),
    );
    let r = s
        .run(&["apply", "app", "env=prod", "--set", "cidr=10.9.0.0/16"])
        .success();
    let app = r
        .stdout
        .split("== app[env=prod]\n")
        .nth(1)
        .unwrap_or_default();
    assert!(app.contains("  cidr = \"10.9.0.0/16\"\n"), "{}", r.stdout);
    // One no stack of the run declares is the target's error.
    let r = s
        .run(&[
            "apply",
            "app",
            "env=prod",
            "--set",
            "cidr=10.9.0.0/16",
            "--set",
            "size=1",
        ])
        .failure();
    assert!(
        r.stderr
            .contains("--set size: the program declares no input size"),
        "{}",
        r.stderr
    );
}

/// The keyed read takes the pun (R-33): a bare name in `[ ]` is `name =
/// name`, alone or beside a `k = v`.
#[test]
fn a_keyed_read_takes_the_pun() {
    let s = Scratch::project("order-pun");
    s.write(
        "stacks/net.df",
        "edition 2026\nkey env: string = \"dev\"\nkey region: string = \"r1\"\nprovider fake\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }\noutput cidr = main.cidr\n",
    );
    let app = |read: &str| {
        format!(
            "edition 2026\nkey env: string = \"dev\"\nprovider fake\nuse stacks.net as network\n\
             resource net.subnet a {{\n  cidr = c\n}} where c = {read}.cidr\n"
        )
    };
    s.write("stacks/app.df", &app("network[env, region = \"r1\"]"));
    let r = s.run(&["apply", "app", "env=dev"]).success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"a\"]\n  cidr = \"10.0.0.0/16\"\n"),
        "{}",
        r.stdout
    );
    // One key, the pun alone.
    s.write(
        "stacks/net.df",
        "edition 2026\nkey env: string = \"dev\"\nprovider fake\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }\noutput cidr = main.cidr\n",
    );
    s.write("stacks/app.df", &app("network[env]"));
    let r = s.run(&["apply", "app", "env=dev"]).success();
    assert!(r.stdout.starts_with("apply app[env=dev]: net[env=dev] first"), "{}", r.stdout);
}
