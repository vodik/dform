//! Waiting (R-81): a tick with nothing definite to apply, held on open
//! nulls that waiting can resolve (a computed value the world has not
//! reached yet, an extern that answered "not yet"), waits for them up to
//! its budget (`apply --wait`, `[stacks.NAME] wait`, 10m), saying so on
//! stderr; past the budget the apply stops, saying what it waited on.

mod common;
use common::{STOPPED, Scratch};

/// The database's endpoint gates the vpc: until the world has it, the
/// vpc is a pending group on it.
const PROG: &str = r#"

use fake
resource db.postgres d { size = 1 }
resource net.vpc v { cidr = "10.0.0.0/16" } where d.endpoint == "d.db.fake"
"#;

/// `dform dev --world w.json apply --yes ARGS p.df`, polling every 50ms.
fn apply(s: &Scratch, args: &[&str]) -> common::Run {
    let mut all = vec!["dev", "--world", "w.json", "apply", "--yes"];
    all.extend_from_slice(args);
    all.push("p.df");
    let out = common::dform()
        .args(&all)
        .env("DFORM_WAIT_POLL_MS", "50")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    common::Run::from(out)
}

/// The `wait` entries of the audit log beside `w.json`.
fn waits(s: &Scratch) -> Vec<serde_json::Value> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "wait")
        .collect()
}

/// Tick 2 has nothing definite: it waits on the endpoint, says so, and
/// once the world has it plans the vpc (an unattended apply then stops
/// before the tick that adds what its plan could not name, R-30; the next
/// apply makes it).
#[test]
fn a_tick_waits_until_the_world_reaches_the_value() {
    let s = Scratch::new("wait-resolves");
    s.write("p.df", PROG);
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:3"]).failure();
    assert!(
        r.stderr
            .contains("waiting on db.postgres[\"d\"].endpoint since "),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains(STOPPED), "{}", r.stderr);
    assert!(
        r.stdout.contains(
            "plan: 1 change (1 create) over 1 tick\n\ntick 3  1 change, now that tick 2 reported\n"
        ),
        "{}",
        r.stdout
    );
    let w = waits(&s);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["result"], "resolved");
    assert_eq!(w[0]["tick"], 2);
    assert_eq!(
        w[0]["on"],
        serde_json::json!(["db.postgres[\"d\"].endpoint"])
    );
    let r = apply(&s, &[]).success();
    assert!(r.stdout.contains("+ net.vpc v"), "{}", r.stdout);
}

/// Past its budget the apply stops, saying what it waited on and how to
/// go on; nothing of the tick is in flight, so the next apply plans it
/// again and waits again.
#[test]
fn past_the_wait_budget_the_apply_stops_saying_what_it_waited_on() {
    let s = Scratch::new("wait-expires");
    s.write("p.df", PROG);
    let r = apply(
        &s,
        &[
            "--wait",
            "200ms",
            "--chaos",
            "not-ready=db.postgres[\"d\"].endpoint:12",
        ],
    )
    .failure();
    assert!(
        r.stderr.contains(
            "Error: apply stopped at tick 2: waited 200ms (--wait) on \
             db.postgres[\"d\"].endpoint, still unknown; state is consistent: run apply again \
             to wait again (`--wait` for longer)\n"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(waits(&s)[0]["result"], "expired");
    let state = s.json("w.state.json");
    assert!(state.get("in_flight").is_none(), "{state}");
    assert!(
        state["resources"].get("db.postgres::d").is_some(),
        "{state}"
    );
    // The next apply waits again, long enough this time.
    let r = apply(&s, &["--wait", "30s"]).failure();
    assert!(r.stderr.contains(STOPPED), "{}", r.stderr);
    apply(&s, &[]).success();
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_some());
}

/// `--wait 0s` does not wait: the tick stops as it did before waits.
#[test]
fn a_wait_of_zero_does_not_wait() {
    let s = Scratch::new("wait-zero");
    s.write("p.df", PROG);
    let r = apply(
        &s,
        &[
            "--wait",
            "0s",
            "--chaos",
            "not-ready=db.postgres[\"d\"].endpoint:3",
        ],
    )
    .failure();
    assert!(
        r.stderr.contains(
            "apply stopped at tick 2: nothing definite to apply, still waiting on \
             ?db.postgres[\"d\"].endpoint"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("waiting on db"), "{}", r.stderr);
    assert!(waits(&s).is_empty());
}

/// `[stacks.NAME] wait` is the stack's budget when `--wait` is not given.
#[test]
fn a_stack_sets_its_wait_budget() {
    let s = Scratch::project("wait-stack");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.p]\nwait = \"0s\"\n",
    );
    s.write("p.df", PROG);
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:3"]).failure();
    assert!(
        r.stderr
            .contains("nothing definite to apply, still waiting on ?db.postgres[\"d\"].endpoint"),
        "{}",
        r.stderr
    );
}

/// An extern that answers "not yet" (an open null in its output columns,
/// where an error is a refusal) is asked again while the tick waits.
#[test]
fn an_extern_that_says_not_yet_is_asked_again() {
    let s = Scratch::new("wait-extern");
    s.write(
        "p.df",
        "\nuse aws { region = \"us-east-1\" }\n\n\
         resource aws.vpc \"v-${availability_zone}\" {\n  cidr_block = \"10.${n}.0.0/16\"\n\
         } where aws.availability_zone(\"available\", availability_zone, n)\n",
    );
    let out = common::dform()
        .args([
            "dev",
            "--provider",
            "aws-mock",
            "--world",
            "w.json",
            "apply",
            "--yes",
            "--chaos",
            "not-yet=aws.availability_zone:2",
            "p.df",
        ])
        .env("DFORM_WAIT_POLL_MS", "50")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    let r = common::Run::from(out).failure();
    assert!(
        r.stderr
            .contains("waiting on aws.availability_zone[\"available\"]"),
        "{}",
        r.stderr
    );
    assert!(
        r.stdout
            .contains("plan: 3 changes (3 create) over 1 tick\n\ntick 2  3 changes, now that tick 1 reported\n"),
        "{}",
        r.stdout
    );
}
