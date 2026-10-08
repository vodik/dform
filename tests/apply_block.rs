//! The apply's block (R-206, as sketched on 2026-10-08), with no
//! terminal or under `--yes`: no bar, the plan's lines, each change once
//! as its call starts and once with its call's word and time (`made
//! 0.1s`), nested as the plan nests them; the tick's end `tick 1  done
//! 0.4s`; under each tick a boundary follows, the policy block as it
//! re-derived it, `policy after tick 1   2 hold  (was 1 · 1
//! undetermined)`, a policy failing there its line above the refusal; a
//! later tick's question on its header line, `tick 2  1 change   apply?
//! [y/N]`; a failure once below the block. The terminal's lines (the bar,
//! the dim word while a call runs) are report/progress.rs's unit tests.

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

/// `text` with each time `T`: the same line for line whatever the
/// machine's speed.
fn timeless(text: &str) -> String {
    text.lines()
        .map(|l| {
            l.split(' ')
                .map(|w| match w.strip_suffix('s').map(str::parse::<f64>) {
                    Some(Ok(_)) => "T",
                    _ => w,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Under `--yes`: tick 1's block, the policies after it, tick 2's block;
/// the vm's update applied with the endpoint tick 1 made.
#[test]
fn two_ticks_print_their_blocks_and_the_policies_between() {
    let s = scratch("block-two", TWO);
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    let r = common::Run::from(out).success();
    assert_eq!(
        timeless(&r.stderr),
        "\n\
         tick 1  2 changes\n  \
         + db.postgres db\n  \
         + db.postgres db  made T\n  \
         + net.vpc main\n  \
         + net.vpc main    made T\n\
         tick 1  done T\n\
         \n\
         tick 2  1 change\n  \
         + compute.vm app\n  \
         + compute.vm app  made T\n\
         tick 2  done T",
        "{}",
        r.stderr
    );
    // The policies as the boundary re-derived them, under tick 1: the
    // count, and what it was before (the words the line says left out).
    assert!(
        r.stdout
            .ends_with("\npolicy after tick 1   2 hold  (was 1 · 1 undetermined)\n"),
        "{}",
        r.stdout
    );
    // Tick 2 is its block: its plan is not printed again.
    assert_eq!(
        r.stdout.matches("tick 2  1 change").count(),
        1,
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
    assert!(
        timeless(&said[2]).contains(
            "tick 2  1 change\n  \
             + iam.policy \"connect-orders.db.fake\"\n  \
             + iam.policy \"connect-orders.db.fake\"  made T\n\
             tick 2  done T"
        ),
        "{}",
        said[2]
    );
}

/// A failure at tick 2: its line `!` and `failed`, the tick's end, then
/// the error once, in the plan's form, below the block.
#[test]
fn a_failure_is_said_once_below_the_block() {
    let s = scratch("block-fails", TWO);
    let out = apply(&s, &["fail=compute.vm[\"app\"]"], &["--yes"])
        .output()
        .unwrap();
    let r = common::Run::from(out).failure();
    let (_, tick2) = r
        .stderr
        .split_once("\ntick 2  1 change\n")
        .unwrap_or_else(|| panic!("{}", r.stderr));
    assert_eq!(
        timeless(tick2),
        "  + compute.vm app\n  \
         ! compute.vm app  failed T\n\
         tick 2  failed T\n\
         ! apply compute.vm app: refused, nothing changed\n    \
         injected failure (chaos fail=compute.vm app)\n    \
         p.df:5\n\
         Error: apply p: tick 2 failed: compute.vm app",
        "{}",
        r.stderr
    );
    assert_eq!(
        r.stderr.matches("injected failure").count(),
        1,
        "{}",
        r.stderr
    );
}

/// A copy's changes nest under its header as the plan's do, each line
/// its full name; the header once, before its first change (a line per
/// change of state: in the order the calls answer).
#[test]
fn a_copy_nests_in_the_block_as_in_the_plan() {
    let s = scratch(
        "block-copy",
        r#"
use fake
component node {
  input index: int
  resource net.vpc vm { cidr = "10.${index}.0.0/16" }
  resource net.subnet sub { vpc_id = ref(vm), cidr = "10.${index}.1.0/24" }
}
resource node "agent-0" { index = 0 }
resource net.vpc edge { cidr = "10.9.0.0/16" }
"#,
    );
    let out = apply(&s, &[], &["--yes"]).output().unwrap();
    let r = common::Run::from(out).success();
    let lines: Vec<String> = timeless(&r.stderr)
        .lines()
        .filter(|l| l.ends_with("made T") || l.contains("node"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines,
        [
            "  + node agent-0",
            "    + net.vpc agent-0.vm      made T",
            "  + net.vpc edge              made T",
            "    + net.subnet agent-0.sub  made T",
        ],
        "{}",
        r.stderr
    );
}
