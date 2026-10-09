//! The apply's ticks as a run of the binary shows them (R-206, After
//! R-206): what ticks.rs decides to print and ask on stdout, and how the
//! run ends. Under each tick a boundary follows, the policy block as it
//! re-derived it, `policy after tick 1   2 hold  (was 1 · 1
//! undetermined)`; a later tick's plan printed again from that tick on,
//! its values as the boundary learned them; a policy failing at the
//! boundary its line above the refusal, exit 4; a later tick that adds to
//! the plan shown asked on its header line, `tick 2  1 change   apply?
//! [y/N]`; a failure said once. What the block on stderr prints is the
//! printer's (tests/apply_events.rs, over a fixed list of events; the
//! lines themselves report/progress.rs's).

mod common;
use common::Scratch;

/// Two ticks: the vm reads the database's endpoint, known once tick 1
/// made it; one policy holds, one is undetermined until tick 2.
const TWO: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource db.postgres db { size = 1 }
resource compute.vm app { db_host = db.endpoint }
deny "a network is no bigger than a /16" where v in net.vpc, v.cidr == "10.2.0.0/8"
deny "the vm reads no endpoint nowhere" where m in compute.vm, m.db_host == "nowhere"
"#;

fn scratch(name: &str, program: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", program);
    s
}

/// `dform dev --world w.json [--chaos C].. apply ARGS.. p.df`.
fn apply(s: &Scratch, chaos: &[&str], args: &[&str]) -> std::process::Command {
    let mut mock = vec!["--world", "w.json"];
    for c in chaos {
        mock.extend(["--chaos", c]);
    }
    let mut verb = vec!["apply"];
    verb.extend_from_slice(args);
    let mut c = common::dform();
    c.args(common::on("p.df", &mock, &verb))
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    c
}

/// Under `--yes`: the policies after tick 1, then tick 2's plan again,
/// the endpoint tick 1 made in place of what it waited on; the vm's
/// update applied with it.
#[test]
fn a_later_tick_prints_its_plan_again_after_the_policies() {
    let s = scratch("block-two", TWO);
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    let r = common::Run::from(out).success();
    let (_, after) = r
        .stdout
        .split_once("\npolicy after tick 1   2 hold  (was 1 · 1 undetermined)\n")
        .unwrap_or_else(|| panic!("{}", r.stdout));
    assert!(
        after.starts_with(
            "\nplan: 1 change (1 create) over 1 tick; policy: 2 hold\n\n\
             tick 2  1 change\n  \
             + compute.vm app  p.df:5\n      \
             db_host = \"db.db.fake\"\n"
        ),
        "{}",
        r.stdout
    );
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]["compute.vm::app"]["attrs"]["db_host"], "db.db.fake",
        "{w}"
    );
}

/// A policy that fails at the boundary: its line under tick 1, then the
/// refusal; tick 2 never runs.
#[test]
fn a_policy_failing_at_the_boundary_is_its_line_above_the_refusal() {
    let s = scratch(
        "block-refused",
        &TWO.replace("== \"nowhere\"", "== \"db.db.fake\""),
    );
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    let r = common::Run::from(out);
    assert_eq!(r.code, Some(4), "{}{}", r.stdout, r.stderr);
    let (_, after) = r
        .stdout
        .split_once("\npolicy after tick 1   1 hold · 1 fails ")
        .unwrap_or_else(|| panic!("{}", r.stdout));
    let mut lines = after.lines();
    assert_eq!(
        lines.next().map(str::trim),
        Some("(was 1 · 1 undetermined)"),
        "{}",
        r.stdout
    );
    assert!(
        lines
            .next()
            .is_some_and(|l| l.starts_with("  fails  the vm reads no endpoint nowhere  p.df:7")),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr
            .contains("constraint violations after tick 1:\n- the vm reads no endpoint nowhere"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("tick 2"), "{}", r.stderr);
}

/// A later tick that adds to the plan shown asks on its header line; its
/// block follows.
#[test]
fn a_later_tick_asks_on_its_header_line() {
    let s = scratch(
        "block-asks",
        r#"
use fake
resource db.postgres orders { size = 1 }
resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
"#,
    );
    let mut cmd = common::dform();
    cmd.args(["dev", "--world", "w.json", "apply", "p.df"])
        .current_dir(&s.dir);
    let (said, code) = common::answering(&s.dir, cmd, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[1].ends_with("\ntick 2  1 change   apply? [y/N] "),
        "{}",
        said[1]
    );
}

/// A failure at tick 2 is said once, below the block; the run ends
/// naming what failed.
#[test]
fn a_failure_is_said_once() {
    let s = scratch("block-fails", TWO);
    let out = apply(&s, &["fail=compute.vm[\"app\"]"], &["--yes"])
        .output()
        .unwrap();
    let r = common::Run::from(out).failure();
    assert_eq!(
        r.stderr.matches("injected failure").count(),
        1,
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .ends_with("Error: apply p: tick 2 failed: compute.vm app\n"),
        "{}",
        r.stderr
    );
}
