//! Apply asks before it changes anything, also when it resumes: the
//! interrupted apply's remaining actions and the uncertain creates it
//! would send again are resolved read-only and shown, marked, before the
//! prompt; state and the world are written only after `y`. On a pty, as
//! a person answers it.

mod common;
use common::Scratch;
use expectrl::{Eof, Expect, Session};

const PROG: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc peer { cidr = "10.1.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
"#;

/// The same stack with `net.vpc.main` renamed (`moved/3`): a resume that
/// also rewrites state's identity.
const RENAMED: &str = r#"edition 2026

resource net.vpc core { cidr = "10.0.0.0/16" }
resource net.vpc peer { cidr = "10.1.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "core", "id"), cidr = "10.0.1.0/24" }
moved(net.vpc, "main", "core")
"#;

const MARKS: &str = "resumed from the apply interrupted at tick 1:\n  \
    net.vpc[\"peer\"]  (retried with its idempotency key: nothing it made was found)\n  \
    net.subnet[\"a\"]  (retried with its idempotency key: nothing it made was found)\n\
    Apply these 2 deformations to p? [y/N] ";

/// An apply the provider crashed under at `net.vpc.peer`: `net.vpc.main`
/// made, the other two remaining in flight, each an uncertain create;
/// then the program renames `net.vpc.main`.
fn interrupted() -> Scratch {
    let s = Scratch::new("confirm-resume");
    s.write("p.df", PROG);
    s.run(&common::on(
        "p.df",
        &["--world", "w.json", "--chaos", "crash=net.vpc[\"peer\"]"],
        &["apply"],
    ))
    .failure();
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert_eq!(st["in_flight"]["remaining"].as_object().unwrap().len(), 2);
    assert_eq!(st["uncertain"].as_object().unwrap().len(), 2, "{st}");
    s.write("p.df", RENAMED);
    s
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
    let mut p = Session::spawn(cmd).unwrap();
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
/// ends `declined`.
#[test]
fn declining_a_resumed_apply_changes_nothing() {
    let s = interrupted();
    let (state, world) = (s.read("w.state.json"), s.read("w.json"));
    let log = audit_kinds(&s);
    let (before, after, code) = answer(&s, "n");
    assert!(before.ends_with(MARKS), "{before}");
    assert!(
        before.contains("moved net.vpc[\"main\"] -> net.vpc[\"core\"]\n"),
        "{before}"
    );
    assert!(
        after.contains("apply p: not confirmed; nothing was applied"),
        "{after}"
    );
    assert_ne!(code, 0);
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
    let s = interrupted();
    let (before, after, code) = answer(&s, "y");
    assert!(before.ends_with(MARKS), "{before}");
    assert_eq!(code, 0, "{after}");
    assert!(after.ends_with("apply: complete\n"), "{after}");
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
const GROUP: &str = r#"edition 2026

resource db.postgres orders { size = 1 }

resource iam.policy "connect-${host}" {
  if pg in db.postgres, host = pg.endpoint
  statements = [{ action: "db.connect", resource: host }]
}
"#;

/// Tick 2 names the group's member, which the first answer did not see:
/// apply asks again, for it alone.
#[test]
fn a_tick_that_adds_an_address_asks_again() {
    let s = Scratch::new("confirm-tick2");
    s.write("p.df", GROUP);
    let (said, code) = answers(&s, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].ends_with("Apply these 2 deformations to p? [y/N] "),
        "{}",
        said[0]
    );
    assert!(
        said[1].ends_with(
            "new at tick 2:\n  + iam.policy[\"connect-orders.db.fake\"]\n\
             Apply these 1 new deformation to p? [y/N] "
        ),
        "{}",
        said[1]
    );
    assert!(said[2].ends_with("apply: complete\n"), "{}", said[2]);
    assert!(s.read("w.json").contains("connect-orders.db.fake"));
}

/// `n` at tick 2: tick 1's database stays made, the policy is not, and
/// the audit log's apply ends `declined` at tick 2.
#[test]
fn declining_at_a_later_tick_keeps_what_ran() {
    let s = Scratch::new("confirm-tick2-no");
    s.write("p.df", GROUP);
    let (said, code) = answers(&s, &["y", "n"]);
    assert_ne!(code, 0);
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
