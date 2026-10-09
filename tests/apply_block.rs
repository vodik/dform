//! The apply's ticks as a run of the binary decides them (R-206, After
//! R-206): what ticks.rs says on stdout, read back as events
//! (`common::brief`; their words are tests/apply_said.rs's), and how the
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
use common::{Scratch, brief};

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
    common::saying(&mut c, s)
        .args(common::on("p.df", &mock, &verb))
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    c
}

/// The ticks the audit log says ran.
fn ticks(s: &Scratch) -> Vec<u64> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "tick")
        .filter_map(|e| e["tick"].as_u64())
        .collect()
}

/// Under `--yes`: the policies after tick 1, then tick 2's plan again,
/// the endpoint tick 1 made in place of what it waited on; the vm's
/// update applied with it.
#[test]
fn a_later_tick_prints_its_plan_again_after_the_policies() {
    let s = scratch("block-two", TWO);
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    common::Run::from(out).success();
    let said = s.said();
    assert_eq!(
        said.iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "policies 1", "plan 2"]
    );
    assert_eq!(
        said[1]["text"],
        "policy after tick 1   2 hold  (was 1 · 1 undetermined)\n"
    );
    // The value tick 1 made known, in place of what tick 2 waited on.
    let tick2 = said[2]["text"].as_str().unwrap();
    assert!(
        tick2.contains("  + compute.vm app  p.df:5\n      db_host = \"db.db.fake\"\n"),
        "{tick2}"
    );
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]["compute.vm::app"]["attrs"]["db_host"], "db.db.fake",
        "{w}"
    );
}

/// A policy that fails at the boundary: its line under tick 1, then the
/// violation that refuses tick 2, which never runs.
#[test]
fn a_policy_failing_at_the_boundary_is_its_line_above_the_refusal() {
    let s = scratch(
        "block-refused",
        &TWO.replace("== \"nowhere\"", "== \"db.db.fake\""),
    );
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    let r = common::Run::from(out);
    assert_eq!(r.code, Some(4), "{}{}", r.stdout, r.stderr);
    let said = s.said();
    assert_eq!(
        said.iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "policies 1", "violations"]
    );
    let policies = said[1]["text"].as_str().unwrap();
    assert!(
        policies.starts_with("policy after tick 1   1 hold · 1 fails ")
            && policies.contains("\n  fails  the vm reads no endpoint nowhere  p.df:7"),
        "{policies}"
    );
    assert_eq!(said[2]["after"], 1);
    assert!(
        said[2]["lines"][0]
            .as_str()
            .unwrap()
            .starts_with("the vm reads no endpoint nowhere"),
        "{}",
        said[2]
    );
    assert_eq!(ticks(&s), [1]);
}

/// A later tick that adds to the plan shown (a pending group's member,
/// named only now: what the group waited on, not a difference) asks on
/// its header line; its block follows.
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
    common::saying(&mut cmd, &s)
        .args(["dev", "--world", "w.json", "apply", "p.df"])
        .current_dir(&s.dir);
    let (said, code) = common::answering(&s.dir, cmd, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    let events = s.said();
    assert_eq!(
        events.iter().map(brief).collect::<Vec<_>>()[3..],
        ["plan 2", "asked tick 2", "answered yes"],
        "{said:?}"
    );
    assert_eq!(events[4]["changes"], 1);
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

/// A later tick whose re-plan has nothing left to change (the subnet
/// waited on the vpc's new id, which the replacement kept) prints no plan
/// mid-apply, where it said `stack p is up to date` as if the apply had
/// ended: its block says `tick 2  nothing to do` (the printer's,
/// tests/apply_events.rs), and the apply ends as it does.
#[test]
fn a_later_tick_with_nothing_to_do_prints_no_plan() {
    const NET: &str = "use fake\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\"), tier = \"web\" }\n";
    let s = scratch("block-nothing", NET);
    apply(&s, &[], &["--yes"]).status().unwrap();
    s.said();
    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    common::Run::from(out).success();
    assert_eq!(
        s.said().iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "differs 2"]
    );
    assert_eq!(ticks(&s), [1, 1]);
}
