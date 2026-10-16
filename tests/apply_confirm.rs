//! Apply asks before it changes anything, also when it resumes: the
//! interrupted apply's remaining actions and the uncertain creates it
//! would send again are resolved read-only and shown, marked, before the
//! prompt; state and the world are written only after `y`. On a pty, as
//! a person answers it.

use crate::common;
use common::Scratch;
use std::collections::BTreeSet;

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc peer { cidr = "10.1.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
use fake
"#;

/// The same stack with `net.vpc.main` renamed (`moved/3`): a resume that
/// also rewrites state's identity.
const RENAMED: &str = r#"

resource net.vpc core { cidr = "10.0.0.0/16" }
resource net.vpc peer { cidr = "10.1.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "core", "id"), cidr = "10.0.1.0/24" }
moved(net.vpc, "main", core)
use fake
"#;

/// The resume's marks, whatever the window: a `main` whose answer was
/// lost is found by its key and mapped before the plan, so only the two
/// not made are retried.
const MARKS: &str = "resumed from the apply interrupted at tick 1:\n  \
    net.vpc peer  (retried with its idempotency key: nothing it made was found)\n  \
    net.subnet a  (retried with its idempotency key: nothing it made was found)\n\
    Apply these 2 changes to p? [y/N] ";

/// The windows the interrupted apply runs under: serial (`--parallel 1`)
/// and the host's ceiling.
const WINDOWS: [usize; 2] = [1, dform_host::ratelimit::CEILING];

/// An apply with at most `window` Apply calls in flight that the provider
/// crashed under at `net.vpc.peer`; then the program renames
/// `net.vpc.main`. Which of `main` and `peer` answered before the crash is
/// the provider's when both are in flight; what holds either way: every
/// create of the tick state does not map is uncertain, with its
/// idempotency key, and remains in flight, the crashed one among them;
/// the calls whose answers the crash lost are at most the window; and
/// each object the world has is mapped or uncertain. Serially the crash
/// at `peer` leaves `main`, sent before it, mapped.
fn interrupted(window: usize) -> Scratch {
    let s = Scratch::new("confirm-resume");
    s.write("p.df", PROG);
    let parallel = window.to_string();
    s.run(&common::on(
        "p.df",
        &["--world", "w.json", "--chaos", "crash=net.vpc[\"peer\"]"],
        &["apply", "--parallel", &parallel],
    ))
    .failure();
    let st = s.json("w.state.json");
    let mapped = keys(&st["resources"]);
    let uncertain = keys(&st["uncertain"]);
    let creates =
        BTreeSet::from(["net.vpc::main", "net.vpc::peer", "net.subnet::a"].map(String::from));
    assert_eq!(uncertain, &creates - &mapped, "window {window}: {st}");
    assert!(uncertain.contains("net.vpc::peer"), "window {window}: {st}");
    assert_eq!(keys(&st["in_flight"]["remaining"]), uncertain, "{st}");
    for (a, u) in st["uncertain"].as_object().unwrap() {
        assert_eq!(u["op"], "create", "{a}: {st}");
        assert!(
            u["key"].as_str().is_some_and(|k| !k.is_empty()),
            "{a}: {st}"
        );
    }
    let lost = audit_kinds(&s)
        .iter()
        .filter(|(k, r)| k == "action" && r == "failed")
        .count();
    assert!((1..=window).contains(&lost), "window {window}: {lost} lost");
    let made = keys(&s.json("w.json")["resources"]);
    assert!(made.is_subset(&(&mapped | &uncertain)), "{made:?}: {st}");
    if window == 1 {
        assert_eq!(
            mapped,
            BTreeSet::from(["net.vpc::main".to_string()]),
            "{st}"
        );
    }
    s.write("p.df", RENAMED);
    s
}

/// An object's keys.
fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// `dform dev --world w.json apply p.df` on a pty, answering `answer`:
/// what it printed up to the prompt and after it, and its exit code.
fn answer(s: &Scratch, answer: &str) -> (String, String, i32) {
    let (mut said, code) = answers(s, &[answer]);
    let after = said.pop().unwrap();
    (said.pop().unwrap(), after, code)
}

/// The same, answering each prompt in turn: what it printed up to each
/// prompt and after the last, and its exit code.
fn answers(s: &Scratch, answers: &[&str]) -> (Vec<String>, i32) {
    let mut cmd = common::dform();
    cmd.args(["dev", "--world", "w.json", "apply", "p.df"])
        .current_dir(s.path(""));
    common::answering(&s.dir, cmd, answers)
}

fn audit_kinds(s: &Scratch) -> Vec<(String, String)> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|e| {
            let r = e["result"].as_str().unwrap_or_default().to_string();
            (e["kind"].as_str().unwrap().to_string(), r)
        })
        .collect()
}

/// `n`: no Apply call reaches the world, state is as the interrupted apply
/// left it (the rename is not written either), and the audit log's plan
/// ends `declined`. A decline is not an error: nothing is said after the
/// answer, the exit is 3 (R-147), and the stack is unlocked.
#[test]
fn declining_a_resumed_apply_changes_nothing() {
    for window in WINDOWS {
        declining_a_resumed_apply_changes_nothing_after(window);
    }
}

fn declining_a_resumed_apply_changes_nothing_after(window: usize) {
    let s = interrupted(window);
    let (state, world) = (s.read("w.state.json"), s.read("w.json"));
    let log = audit_kinds(&s);
    let (before, after, code) = answer(&s, "n");
    assert!(before.ends_with(MARKS), "{before}");
    assert!(
        before.contains("moved net.vpc[\"main\"] -> net.vpc[\"core\"]\n"),
        "{before}"
    );
    assert_eq!(after.trim(), "", "nothing is said after the answer");
    assert_eq!(code, 3, "a decline exits 3 (R-147)");
    assert!(
        !s.path("w.state.lock").exists(),
        "the stack is still locked"
    );
    assert_eq!(s.read("w.json"), world, "the world changed");
    assert_eq!(s.read("w.state.json"), state, "state changed");
    let now = audit_kinds(&s);
    assert_eq!(now[..log.len()], log[..]);
    assert_eq!(
        now[log.len()..],
        [
            ("plan".to_string(), String::new()),
            ("apply_end".to_string(), "declined".to_string())
        ]
    );
}

/// `y`: the remaining actions are applied, with the rename.
#[test]
fn confirming_a_resumed_apply_finishes_it() {
    for window in WINDOWS {
        confirming_a_resumed_apply_finishes_it_after(window);
    }
}

fn confirming_a_resumed_apply_finishes_it_after(window: usize) {
    let s = interrupted(window);
    let (before, after, code) = answer(&s, "y");
    assert!(before.ends_with(MARKS), "{before}");
    assert_eq!(code, 0, "{after}");
    assert!(!after.contains("apply: complete"), "{after}");
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert!(st.get("in_flight").is_none(), "{st}");
    assert!(st.get("uncertain").is_none(), "{st}");
    let mut names: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(
        names,
        ["net.subnet::a", "net.vpc::core", "net.vpc::peer"],
        "{st}"
    );
    let log = audit_kinds(&s);
    assert_eq!(
        log.last().unwrap(),
        &("apply_end".to_string(), "ok".to_string())
    );
}

/// A policy named for the database's endpoint: tick 1 makes the database
/// and lists the policy only as a pending group, `iam.policy[?]`.
const GROUP: &str = r#"

resource db.postgres orders { size = 1 }

resource iam.policy "connect-${host}" {
  statements = [{ action = "db.connect", resource = host }]
} where pg in db.postgres, host = pg.endpoint
use fake
"#;

/// Tick 2 names the group's member, which the first answer did not see:
/// apply prints tick 2's plan and asks again, counting the new ones.
#[test]
fn a_tick_that_adds_an_address_asks_again() {
    let s = Scratch::new("confirm-tick2");
    s.write("p.df", GROUP);
    let (said, code) = answers(&s, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].ends_with("Apply this change to p? [y/N] "),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("plan: 1 change (1 create) over 1 tick\n\ntick 2  1 change\n"),
        "{}",
        said[1]
    );
    assert!(
        said[1].contains("  + iam.policy \"connect-orders.db.fake\"  "),
        "{}",
        said[1]
    );
    assert!(
        said[1].ends_with("tick 2  1 change   apply? [y/N] "),
        "{}",
        said[1]
    );
    assert!(!said[2].contains("apply: complete"), "{}", said[2]);
    assert!(s.read("w.json").contains("connect-orders.db.fake"));
}

/// `n` at tick 2: tick 1's database stays made, the policy is not, and
/// the audit log's apply ends `declined` at tick 2.
#[test]
fn declining_at_a_later_tick_keeps_what_ran() {
    let s = Scratch::new("confirm-tick2-no");
    s.write("p.df", GROUP);
    let (said, code) = answers(&s, &["y", "n"]);
    assert_eq!(code, 3, "a decline exits 3 (R-147)");
    assert!(!said[2].contains("Error"), "{}", said[2]);
    assert!(
        !s.path("w.state.lock").exists(),
        "the stack is still locked"
    );
    assert!(
        said[2].contains(
            "apply p: not confirmed at tick 2; ticks 1 to 1 were applied, and the next apply \
             resumes from there"
        ),
        "{}",
        said[2]
    );
    let world = s.read("w.json");
    assert!(world.contains("db.postgres"), "{world}");
    assert!(!world.contains("iam.policy"), "{world}");
    let end: serde_json::Value =
        serde_json::from_str(s.read("w.state.audit.jsonl").lines().last().unwrap()).unwrap();
    assert_eq!(end["kind"], "apply_end");
    assert_eq!(end["result"], "declined");
    assert_eq!(end["tick"], 2);
}

/// `n` to the tree's one question ends the command there, exit 3: nothing
/// of either deployment is applied, each apply's end says `declined`, and
/// neither is left locked.
#[test]
fn declining_the_tree_applies_nothing() {
    let s = Scratch::project("confirm-dependency");
    s.write(
        "stacks/platform.df",
        "key env: enum(\"lab\") = \"lab\"\nuse fake\nresource net.vpc edge { cidr = \"10.0.0.0/16\" }\noutput ip = edge.cidr\n",
    );
    s.write(
        "stacks/apps.df",
        "key env: enum(\"lab\") = \"lab\"\nuse fake\nuse stacks.platform\nresource net.vpc rec { cidr = \"10.1.0.0/16\", name = platform[env].ip }\n",
    );
    let mut cmd = common::dform();
    cmd.args(["apply", "apps"]).current_dir(s.path(""));
    let (said, code) = common::answering(&s.dir, cmd, &["n"]);
    assert!(
        said[0].contains("== stacks.platform[env=lab]")
            && said[0].contains("== stacks.apps[env=lab]")
            && said[0].contains(
                "Apply these 2 changes to stacks.platform[env=lab], stacks.apps[env=lab]?"
            ),
        "{}",
        said[0]
    );
    assert_eq!(code, 3, "a decline exits 3 (R-147)");
    for d in ["platform", "apps"] {
        let dir = s.path(&format!("dform.state/stacks.{d}/env=lab"));
        let log = std::fs::read_to_string(dir.join("state.audit.jsonl")).unwrap();
        assert!(log.contains("\"result\":\"declined\""), "{d}: {log}");
        assert!(!dir.join("state.lock").exists(), "{d} is still locked");
        assert!(!dir.join("state.json").exists(), "{d} applied");
    }
}
