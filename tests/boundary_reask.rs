//! A tick re-planned at its boundary against the tick as the first plan
//! showed it (After R-156): the same changes with the same values, where
//! the plan knew them, apply without a word; a tick that differs (a value
//! the provider reports otherwise after tick 1) is printed with what
//! differs, `a → b`, and asked for again, unless `--yes`; a plan file
//! stops before it, exit 5, naming what differs.
//!
//! The namespace's update in tick 1 is followed by chaos `mutate` of its
//! `metadata.resourceVersion`, as a cluster bumps it; the vm's update,
//! in tick 2 (it waits on the database's endpoint), reads it. The plan
//! showed `rv = <none> → "115"`; tick 2 plans `"200"`.

mod common;
use common::Scratch;
use expectrl::{Eof, Expect, Session};

const BEFORE: &str = r#"use fake
use k8s
resource k8s.namespace ns { metadata.name = "app", metadata.labels = { v: "1" } }
resource compute.vm app { db_host = "old.db.fake" }
"#;

const PROG: &str = r#"use fake
use k8s
resource k8s.namespace ns { metadata.name = "app", metadata.labels = { v: "2" } }
resource db.postgres main { size = 1 }
resource compute.vm app { db_host = main.endpoint, rv = ns.metadata.resourceVersion }
"#;

const MUTATE: &str = "mutate=k8s.namespace[\"ns\"].metadata.resourceVersion=\"200\"";

const DIFFERS: &str =
    "tick 2 differs from the plan shown:\n  ~ compute.vm app  rv = \"115\" → \"200\"\n";

/// The namespace and the vm applied as `BEFORE` has them; `p.df` is
/// `PROG`.
fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("before.df", BEFORE);
    dform(
        &s,
        &["dev", "--world", "w.json", "apply", "--yes", "before.df"],
    );
    s.write("p.df", PROG);
    s
}

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    let out = command(s, args).output().unwrap();
    common::Run::from(out)
}

fn command(s: &Scratch, args: &[&str]) -> std::process::Command {
    let mut c = common::dform();
    c.args(args).env("NO_COLOR", "1").current_dir(&s.dir);
    c
}

/// `dform dev --world w.json [--chaos MUTATE] apply p.df` on a pty,
/// answering each prompt in turn: what it printed up to each prompt and
/// after the last, and its exit code.
fn answers(s: &Scratch, mutate: bool, answers: &[&str]) -> (Vec<String>, i32) {
    let mut args = vec!["dev", "--world", "w.json"];
    if mutate {
        args.extend(["--chaos", MUTATE]);
    }
    args.extend(["apply", "p.df"]);
    let mut p = Session::spawn(command(s, &args)).unwrap();
    p.set_expect_timeout(Some(std::time::Duration::from_secs(60)));
    let text = |b: &[u8]| String::from_utf8_lossy(b).replace('\r', "");
    let all = |c: &expectrl::Captures| c.matches().fold(text(c.before()), |t, m| t + &text(m));
    let mut said = Vec::new();
    for a in answers {
        said.push(all(&p.expect("[y/N] ").unwrap()));
        p.send_line(a).unwrap();
    }
    said.push(all(&p.expect(Eof).unwrap()));
    let code = match p.get_process().wait().unwrap() {
        expectrl::process::unix::WaitStatus::Exited(_, code) => code,
        other => panic!("{other:?}"),
    };
    (said, code)
}

fn vm(s: &Scratch) -> serde_json::Value {
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]["compute.vm::app"]["attrs"].clone()
}

/// The plan showed both ticks and was answered once; tick 2 re-planned
/// as shown is applied without asking again.
#[test]
fn a_tick_as_shown_is_not_asked_again() {
    let s = scratch("reask-same");
    let (said, code) = answers(&s, false, &["y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].contains("tick 2  1 change\n  waits on  main.endpoint\n"),
        "{}",
        said[0]
    );
    assert!(!said[1].contains("differs"), "{}", said[1]);
    assert!(!said[1].contains("[y/N]"), "{}", said[1]);
    assert_eq!(vm(&s)["rv"], "115");
}

/// The resourceVersion the vm reads is reported otherwise after tick 1:
/// tick 2 is printed again, what differs below it, and asked for.
#[test]
fn a_tick_that_differs_is_asked_again_with_what_differs() {
    let s = scratch("reask-differs");
    let (said, code) = answers(&s, true, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].contains("      rv = <none> → \"115\"\n"),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("tick 2  1 change\n  ~ compute.vm app  p.df:5\n"),
        "{}",
        said[1]
    );
    assert!(
        said[1].ends_with(&format!("{DIFFERS}Apply tick 2 to p? [y/N] ")),
        "{}",
        said[1]
    );
    assert_eq!(vm(&s)["rv"], "200");
}

/// `n` at the second question: tick 1 stays applied, tick 2 does not run,
/// exit 3.
#[test]
fn declining_a_tick_that_differs_keeps_tick_one() {
    let s = scratch("reask-declined");
    let (said, code) = answers(&s, true, &["y", "n"]);
    assert_eq!(code, 3, "{said:?}");
    assert!(
        said[2].contains("apply p: not confirmed at tick 2; ticks 1 to 1 were applied"),
        "{}",
        said[2]
    );
    assert_eq!(vm(&s)["db_host"], "old.db.fake");
    let w = s.read("w.json");
    assert!(w.contains("db.postgres::main"), "{w}");
}

/// `--yes` answers the question: what differs is printed, and applied.
#[test]
fn yes_prints_what_differs_and_applies_it() {
    let s = scratch("reask-yes");
    let r = dform(
        &s,
        &[
            "dev", "--world", "w.json", "--chaos", MUTATE, "apply", "--yes", "p.df",
        ],
    )
    .success();
    assert!(r.stdout.contains(DIFFERS), "{}", r.stdout);
    assert_eq!(vm(&s)["rv"], "200");
}

/// A plan file covers the ticks whose re-plan is the file's: tick 2
/// differs, so the apply stops after tick 1, exit 5, naming the attribute,
/// the state consistent; the next plan has the value the world now has.
#[test]
fn a_plan_file_stops_before_a_tick_that_differs() {
    let s = scratch("reask-plan-file");
    dform(
        &s,
        &[
            "dev",
            "--world",
            "w.json",
            "plan",
            "--out",
            "plan.json",
            "p.df",
        ],
    )
    .success();
    let r = dform(&s, &["dev", "--chaos", MUTATE, "apply", "plan.json"]).failure();
    assert_eq!(r.code, Some(5), "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains(DIFFERS), "{}", r.stdout);
    assert!(
        r.stderr.contains(
            "apply stopped after tick 1: tick 2 differs from the plan it applies: compute.vm \
             app.rv; run apply again"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(vm(&s)["db_host"], "old.db.fake");
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert!(st.get("in_flight").is_none(), "{st}");
    let r = dform(&s, &["dev", "--world", "w.json", "plan", "p.df"]).success();
    assert!(r.stdout.contains("rv = <none> → \"200\""), "{}", r.stdout);
}
