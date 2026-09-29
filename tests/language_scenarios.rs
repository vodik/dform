//! Scenarios: named hypothetical facts plus deny rules. `dform test` runs
//! each against an empty mock world and fails on any deny; `plan
//! --scenario NAME` is the same program as a what-if plan.

mod common;
use common::{Scratch, repo};

const P: &str = r#"edition 2027
input env: enum("dev", "prod")
input size: int = 1
resource net.vpc main {
  if size(s)
  cidr = "10.0.0.0/16"
  size = s
}
resource db.postgres main {
  if env("prod")
  multi_az = true
}
deny "size is small" if size(s), s > 5

scenario prod {
  set env = "prod"
  deny "prod has a database" if not want(db.postgres, "main")
}

scenario dev_is_small {
  set env = "dev"
  set size = 9
  deny "dev has no database" if want(db.postgres, _)
}
"#;

#[test]
fn test_runs_every_scenario_and_fails_on_a_deny() {
    let s = Scratch::new("lang-scenarios");
    s.write("p.df", P);
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "scenario prod: ok\nscenario dev_is_small: denied\n  - size is small\ntest: 2 scenarios, 1 failed\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stderr.contains("1 of 2 scenarios failed"), "{}", r.stderr);
    // Nothing touched: no state, no world.
    assert!(!s.path("dform.state").exists());
}

/// A scenario is the program only when selected: a plain plan has no
/// value for the required input, a scenario plan has one.
#[test]
fn plan_with_a_scenario_is_a_what_if() {
    let s = Scratch::new("lang-scenario-plan");
    s.write("p.df", P);
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(r.stderr.contains("input env is required"), "{}", r.stderr);
    let r = s.run(&["plan", "--scenario", "prod", "p.df"]).success();
    assert!(r.stdout.contains("+ db.postgres[\"main\"]"), "{}", r.stdout);
    let r = s.run(&["plan", "--scenario", "qa", "p.df"]).failure();
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
    let file = repo().join("examples/demo/stacks/dform.df");
    let r = s.run(&["test", file.to_str().unwrap()]).success();
    assert!(
        r.stdout.contains("test: 2 scenarios, 0 failed"),
        "{}",
        r.stdout
    );
}
