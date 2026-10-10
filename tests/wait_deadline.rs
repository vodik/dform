//! A wait's deadline (R-201): a tick held on a value the world has not
//! reached yet waits up to `--wait-timeout`, else the stack's `[stacks.NAME]
//! wait`, else the `wait` of the provider that answers the value, else the
//! project's `[apply] wait`, else 10m. Past it the apply stops, exit 1,
//! naming the value and the setting; what was applied stays applied, and
//! the wait is the next apply's.

mod common;
use common::Scratch;

/// The database's endpoint gates the vpc: until the world has it, tick 2
/// waits on it.
const PROG: &str = r#"

use fake
resource db.postgres d { size = 1 }
resource net.vpc v { cidr = "10.0.0.0/16" } where d.endpoint == "d.db.fake"
"#;

/// A project of `p.df` whose dform.toml adds `toml` to its `[project]`.
fn project(name: &str, toml: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write(
        "dform.toml",
        &format!("[project]\nedition = \"2026\"\n{toml}"),
    );
    s.write("p.df", PROG);
    s
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

/// A wait the world ends within the stack's `wait` goes on to the next
/// tick, though the project's `[apply] wait` is shorter: about 40 Reads
/// at 50ms each is 2s.
#[test]
fn a_wait_that_ends_within_the_stacks_budget_goes_on() {
    let toml = "\n[apply]\nwait = \"1s\"\n\n[stacks.p]\nwait = \"30s\"\n";
    let s = project("deadline-within", toml);
    apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:40"]).success();
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_some());
}

/// Past `[apply] wait` the apply stops, exit 1, naming the value, the
/// budget and the setting; tick 1 stays applied and nothing is in
/// flight, so the next apply waits again.
#[test]
fn past_the_deadline_the_apply_stops_and_the_wait_is_the_next_applys() {
    let s = project("deadline-past", "\n[apply]\nwait = \"1s\"\n");
    // About 20 Reads in the 1s; the rest are the next apply's.
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:60"]);
    assert_eq!(r.code, Some(1), "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "error  apply stopped at tick 2: db.postgres d.endpoint not reached in 1s \
             (`[apply] wait` in dform.toml); state is consistent: run apply again to \
             wait again\n"
        ),
        "{}",
        r.stderr
    );
    let state = s.json("w.state.json");
    assert!(state.get("in_flight").is_none(), "{state}");
    assert!(
        state["resources"].get("db.postgres::d").is_some(),
        "{state}"
    );
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_none());
    // The rest, about 40 Reads, outlast `[apply] wait` but not the flag.
    apply(&s, &["--wait-timeout", "30s"]).success();
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_some());
}
