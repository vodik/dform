//! Each exit status means one thing (R-147, docs/reference.md "Exit
//! status"): 0 done, 1 failed, 2 usage, 3 declined, 4 refused by the
//! program, 5 stopped, 6 locked, 128 + N after signal N (tests/interrupt.rs).
//! One run per status on the mock; `plan --json` says the word in
//! `outcome`.

mod common;
use common::{Scratch, mock};
use std::process::Stdio;

const PROG: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
use fake
"#;

/// A deny over a deformation of the vpc: the program refuses the plan.
const DENIED: &str = r#"
resource net.vpc main { cidr = "10.1.0.0/16" }
deny "a wide vpc: ${v}" where deformation(_, v, _), v in net.vpc, v.cidr == "10.1.0.0/16"
use fake
"#;

/// Tick 2 adds a change the plan could not name: a plan file stops.
const GROUP: &str = r#"
resource db.postgres orders { size = 1 }

resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
use fake
"#;

fn project(name: &str, prog: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", prog);
    s
}

#[test]
fn done_is_0_and_a_plan_with_changes_too() {
    let s = project("exit-done", PROG);
    let r = mock(&s, &["plan"]);
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    let r = mock(&s, &["plan", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["outcome"], "done", "{j}");
    let r = mock(&s, &["apply"]);
    assert_eq!(r.code, Some(0), "{}", r.stderr);
}

#[test]
fn a_failure_is_1() {
    let s = project("exit-failed", PROG);
    let r = mock(&s, &["apply", "--chaos", "fail=net.subnet[\"a\"]"]);
    assert_eq!(r.code, Some(1), "{}", r.stderr);
}

#[test]
fn a_usage_error_is_2() {
    let s = project("exit-usage", PROG);
    let r = s.run(&["plan", "--no-such-flag"]);
    assert_eq!(r.code, Some(2), "{}", r.stderr);
}

/// `n` at the prompt (a pty): 3, nothing printed as an error.
#[test]
fn a_decline_is_3() {
    use expectrl::{Eof, Expect, Session};
    let s = project("exit-declined", PROG);
    let mut cmd = common::dform();
    cmd.args(["dev", "--world", "w.json", "apply", "p.df"])
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(std::time::Duration::from_secs(60)));
    p.expect("[y/N] ").unwrap();
    p.send_line("n").unwrap();
    let after = String::from_utf8_lossy(p.expect(Eof).unwrap().before()).replace('\r', "");
    assert_eq!(after.trim(), "", "{after}");
    match p.get_process().wait().unwrap() {
        expectrl::process::unix::WaitStatus::Exited(_, code) => assert_eq!(code, 3),
        other => panic!("{other:?}"),
    }
}

/// A deny: `plan` and `apply` both 4, the plan's `--json` `refused`.
#[test]
fn a_refusal_is_4() {
    let s = project("exit-refused", DENIED);
    let r = mock(&s, &["plan"]);
    assert_eq!(r.code, Some(4), "{}{}", r.stdout, r.stderr);
    let r = mock(&s, &["plan", "--json"]);
    assert_eq!(r.code, Some(4), "{}", r.stderr);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["outcome"], "refused", "{j}");
    let r = mock(&s, &["apply"]);
    assert_eq!(r.code, Some(4), "{}", r.stderr);
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

/// A plan file stops before a change it could not name: 5.
#[test]
fn a_stop_is_5() {
    let s = project("exit-stopped", GROUP);
    mock(&s, &["plan", "--out", "plan.json"]).success();
    let r = s.run(&["apply", "plan.json"]).stopped();
    assert!(
        r.stderr.contains("apply stopped after tick 1"),
        "{}",
        r.stderr
    );
}

/// Another apply holds the stack: 6, naming it.
#[test]
fn a_held_lock_is_6() {
    let s = project("exit-locked", PROG);
    let release = s.path("release");
    let mut first = common::dform()
        .args(common::yes(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["apply"],
        )))
        .current_dir(&s.dir)
        .env("DFORM_TEST_HOLD_LOCK", &release)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lock = s.path("w.state.lock");
    let start = std::time::Instant::now();
    while !lock.exists() {
        assert!(
            start.elapsed().as_secs() < 30,
            "the first apply never locked"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let r = mock(&s, &["apply"]);
    s.write("release", "");
    first.wait().unwrap();
    assert_eq!(r.code, Some(6), "{}", r.stderr);
    assert!(
        r.stderr.contains("is locked by another apply"),
        "{}",
        r.stderr
    );
}
