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

/// The endpoint is absent from its first 400 Reads: no budget here
/// reaches it.
const NEVER: &str = "not-ready=db.postgres[\"d\"].endpoint:400";

/// A project of `p.df` whose dform.toml adds `toml` to its `[project]`.
fn project(name: &str, toml: &str) -> Scratch {
    let s = Scratch::new(name);
    write_toml(&s, toml);
    s.write("p.df", PROG);
    s
}

fn write_toml(s: &Scratch, toml: &str) {
    s.write(
        "dform.toml",
        &format!("[project]\nedition = \"2026\"\n{toml}"),
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

/// A wait the world ends within `[apply] wait` goes on to the next tick.
#[test]
fn a_wait_that_ends_within_the_budget_goes_on() {
    let s = project("deadline-within", "\n[apply]\nwait = \"30s\"\n");
    let r = apply(&s, &["--chaos", "not-ready=db.postgres[\"d\"].endpoint:3"]).success();
    assert!(
        r.stderr
            .contains("waiting on db.postgres d.endpoint since "),
        "{}",
        r.stderr
    );
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
        r.stderr
            .contains("waiting on db.postgres d.endpoint since "),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "Error: apply stopped at tick 2: db.postgres d.endpoint not reached in 1s \
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
    let r = apply(&s, &["--wait-timeout", "30s"]).success();
    assert!(r.stderr.contains("waiting on "), "{}", r.stderr);
    assert!(s.json("w.json")["resources"].get("net.vpc::v").is_some());
}

/// The flag wins over the stack's `wait`, the stack's over the
/// provider's, the provider's over `[apply] wait`: each case makes the
/// winner 1s and the rest 30m, and the stop names the winner.
#[test]
fn the_flag_then_the_stack_then_the_provider_then_the_project() {
    let long = "\"30m\"";
    let fake = |w: &str| format!("\n[providers]\nfake = {{ source = \"fake\", wait = {w} }}\n");
    let cases = [
        (
            format!(
                "\n[apply]\nwait = {long}\n[stacks.p]\nwait = {long}\n{}",
                fake(long)
            ),
            vec!["--wait-timeout", "1s"],
            "(`--wait-timeout`)",
        ),
        (
            format!(
                "\n[apply]\nwait = {long}\n[stacks.p]\nwait = \"1s\"\n{}",
                fake(long)
            ),
            vec![],
            "(`[stacks.p] wait` in dform.toml)",
        ),
        (
            format!("\n[apply]\nwait = {long}\n{}", fake("\"1s\"")),
            vec![],
            "(`[providers.fake] wait` in dform.toml)",
        ),
    ];
    for (toml, flags, said) in cases {
        let s = project("deadline-precedence", &toml);
        let mut args = vec!["--chaos", NEVER];
        args.extend(flags);
        let r = apply(&s, &args);
        assert_eq!(r.code, Some(1), "{toml}\n{}", r.stderr);
        assert!(
            r.stderr.contains(&format!("not reached in 1s {said}")),
            "{toml}\n{}",
            r.stderr
        );
    }
}

/// A wait that is not a duration is an error naming its setting, in
/// dform.toml and on the command line.
#[test]
fn a_wait_is_a_duration() {
    let s = project("deadline-bad", "\n[apply]\nwait = \"soon\"\n");
    let r = apply(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("[apply] wait = \"soon\": a duration, `500ms`, `30s` or `10m`"),
        "{}",
        r.stderr
    );
    write_toml(&s, "\n[stacks.p]\nwait = \"0s\"\n");
    let r = apply(&s, &[]).failure();
    assert!(
        r.stderr.contains("[stacks.p] wait = \"0s\": a duration"),
        "{}",
        r.stderr
    );
    write_toml(&s, "");
    let r = apply(&s, &["--wait-timeout", "soon"]);
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(
        r.stderr.contains("\"soon\" is not a duration"),
        "{}",
        r.stderr
    );
}
