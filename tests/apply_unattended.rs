//! An apply that applies a plan it was shown (a plan file, an approval)
//! applies only the ticks whose addresses that plan named, and stops
//! before the first tick that would add one, the state consistent: the
//! next apply plans them as its tick 1 (R-30). `--yes` answers every
//! question: it plans each later tick as the one before reports and
//! applies it (R-122). The controller keeps running through ticks.

mod common;
use common::{Scratch, copy_dir, repo};

const STOPPED: &str = "apply stopped after tick 1: tick 2 adds 1 change the plan could \
    not name (iam.policy ? on db.postgres orders.endpoint); run apply again to plan \
    them against the world as it now is";

/// The tour's prod: the database's endpoint names a policy only tick 2 can
/// name. `--yes` makes the database, plans tick 2 once it reports, and
/// makes the policy.
#[test]
fn yes_applies_a_tick_the_plan_could_not_name() {
    let s = Scratch::new("unattended-tour");
    copy_dir(&repo().join("examples/tour"), &s.dir);
    let r = s.run(&["apply", "tour", "env=prod"]).success();
    let (_, tick2) = r.stdout.split_once("\ntick 2  ").unwrap_or_else(|| {
        panic!("{}", r.stdout);
    });
    assert!(
        tick2.contains("  + iam.policy \"connect-orders.db.fake\"  "),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let state = s.read("dform.state/tour/env=prod/state.json");
    assert!(state.contains("iam.policy"), "{state}");
}

/// A plan file of the same: applying it makes the database and stops
/// before the policy it did not show; the next apply plans the policy as
/// its tick 1 and completes.
#[test]
fn a_plan_file_stops_before_a_tick_the_plan_could_not_name() {
    let s = Scratch::new("unattended-tour-file");
    copy_dir(&repo().join("examples/tour"), &s.dir);
    s.run(&["plan", "--out", "plan.json", "tour", "env=prod"])
        .success();
    let r = s.run(&["apply", "plan.json"]).stopped();
    assert!(r.stderr.contains(STOPPED), "{}", r.stderr);
    let state = s.read("dform.state/tour/env=prod/state.json");
    assert!(state.contains("db.postgres"), "{state}");
    assert!(!state.contains("iam.policy"), "{state}");
    let st: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert!(st["in_flight"].is_null(), "{st}");
    let audit = s.read("dform.state/tour/env=prod/state.audit.jsonl");
    let end: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(end["kind"], "apply_end", "{end}");
    assert_eq!(end["result"], "stopped", "{end}");

    let r = s.run(&["apply", "tour", "env=prod"]).success();
    assert!(
        r.stdout
            .contains("  + iam.policy \"connect-orders.db.fake\"  "),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("tick 2"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// The controller is its own loop: it runs both ticks in one event.
#[test]
fn the_controller_still_runs_two_ticks() {
    let s = Scratch::project("unattended-controller");
    s.write(
        "p.df",
        r#"

resource db.postgres orders { size = 1 }

resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
use fake
"#,
    );
    s.run(&["controller", "run", "--once", "p.df"]).success();
    let world = s.read("dform.state/p/remote.json");
    assert!(world.contains("connect-orders.db.fake"), "{world}");
}
