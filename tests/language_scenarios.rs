//! Scenarios: named hypothetical facts plus deny rules. `dform test` runs
//! each against an empty mock world and fails on any deny; `plan
//! --scenario NAME` is the same program as a what-if plan.

mod common;
use common::{Scratch, repo};

const P: &str = r#"edition 2026.
input env: enum(dev, prod).
input size: int = 1.
resource net.vpc main { cidr = "10.0.0.0/16", size = S } :- size(S).
resource db.postgres main { multi_az = true } :- env(prod).
deny("size is small") :- size(S), S > 5.

scenario prod {
  input("env", prod).
  deny("prod has a database") :- not want(db.postgres, main).
}.

scenario dev_is_small {
  input("env", dev).
  input("size", 9).
  deny("dev has no database") :- want(db.postgres, _).
}.
"#;

#[test]
fn test_runs_every_scenario_and_fails_on_a_deny() {
    let s = Scratch::new("lang-scenarios");
    s.write("p.df", P);
    let r = s.run(&["--file", "p.df", "test"]).failure();
    assert!(
        r.stdout.contains(
            "scenario prod: ok\nscenario dev_is_small: denied\n  - size is small\ntest: 2 scenarios, 1 failed\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stderr.contains("1 of 2 scenarios failed"), "{}", r.stderr);
    // Nothing touched: no state, no world.
    assert!(!s.path(".dform").exists());
}

/// A scenario is the program only when selected: a plain plan has no
/// value for the required input, a scenario plan has one.
#[test]
fn plan_with_a_scenario_is_a_what_if() {
    let s = Scratch::new("lang-scenario-plan");
    s.write("p.df", P);
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(r.stderr.contains("input env is required"), "{}", r.stderr);
    let r = s
        .run(&["--file", "p.df", "plan", "--scenario", "prod"])
        .success();
    assert!(r.stdout.contains("+ db.postgres.main"), "{}", r.stdout);
    let r = s
        .run(&["--file", "p.df", "plan", "--scenario", "qa"])
        .failure();
    assert!(
        r.stderr
            .contains("no scenario qa: the program's scenarios are prod, dev_is_small"),
        "{}",
        r.stderr
    );
}

#[test]
fn the_demo_scenarios_pass() {
    let s = Scratch::new("lang-scenario-demo");
    let file = repo().join("dform.df");
    let r = s.run(&["--file", file.to_str().unwrap(), "test"]).success();
    assert!(
        r.stdout.contains("test: 2 scenarios, 0 failed"),
        "{}",
        r.stdout
    );
}
