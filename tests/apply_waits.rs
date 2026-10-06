//! Waiting (R-81, R-122): a tick with nothing definite to apply, held on
//! open nulls that waiting can resolve (a computed value the world has
//! not reached yet, an extern that answered "not yet"), waits for them up
//! to the `timeout` of the provider that answers them (`[providers.NAME]
//! timeout`, 60s by default), saying so on stderr; past it the apply
//! stops, saying what it waited on.

mod common;
use common::Scratch;

/// The database's endpoint gates the vpc: until the world has it, the
/// vpc is a pending group on it.
const PROG: &str = r#"

use fake
resource db.postgres d { size = 1 }
resource net.vpc v { cidr = "10.0.0.0/16" } where d.endpoint == "d.db.fake"
"#;

/// A project whose fake provider's `timeout` is `timeout`.
fn project(name: &str, timeout: &str) -> Scratch {
    let s = Scratch::new(name);
    set_timeout(&s, timeout);
    s.write("p.df", PROG);
    s
}

fn set_timeout(s: &Scratch, timeout: &str) {
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = {{ source = \"fake\", \
             timeout = \"{timeout}\" }}\n"
        ),
    );
}

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
/// once the world has it plans the vpc, which `--yes` applies at tick 3
/// (R-122).
#[test]
fn a_tick_waits_until_the_world_reaches_the_value() {
    let s = Scratch::new("wait-resolves");
    s.write("p.df", PROG);
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:3"]).success();
    assert!(
        r.stderr
            .contains("waiting on db.postgres d.endpoint since "),
        "{}",
        r.stderr
    );
    assert!(
        r.stdout
            .contains("\ntick 3  1 change, now that tick 2 reported\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("+ net.vpc v"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let w = waits(&s);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["result"], "resolved");
    assert_eq!(w[0]["tick"], 2);
    assert_eq!(
        w[0]["on"],
        serde_json::json!(["db.postgres[\"d\"].endpoint"])
    );
}

/// Past the provider's `timeout` the apply stops, saying what it waited
/// on and how to go on; nothing of the tick is in flight, so the next
/// apply plans it again and waits again.
#[test]
fn past_the_providers_timeout_the_apply_stops_saying_what_it_waited_on() {
    let s = project("wait-expires", "1s");
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:40"]).failure();
    assert!(
        r.stderr.contains(
            "Error: apply stopped at tick 2: waited 1s on db.postgres d.endpoint, still \
             unknown (the provider's `timeout` in dform.toml); state is consistent: run \
             apply again to wait again\n"
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
    set_timeout(&s, "30s");
    apply(&s, &[]).success();
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_some());
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
    let r = common::Run::from(out).success();
    assert!(
        r.stderr
            .contains("waiting on aws.availability_zone(\"available\")"),
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
