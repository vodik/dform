//! Tick 2 of a provider configured from what tick 1 makes (R-45): the
//! plan lists that provider's resources under `later`, waiting on its
//! settings; apply makes tick 1, configures the provider at the boundary
//! (waiting, up to `--wait`, while the value is not there yet), plans what
//! `later` held against it, and asks before applying it as it asked
//! before tick 1, unless `--yes`. A plan file or an approval did not see
//! that diff: the apply stops before it. A value no wait brings stops it,
//! naming the provider.
//!
//! The server is the mock's `db.postgres` (the mock run as a plugin, a
//! second provider process), the settings its endpoint behind an
//! `env.var`, the provider configured from them the mock's `k8s`.

mod common;
use common::{Run, STOPPED, Scratch};
use expectrl::{Eof, Expect, Session};

const PROG: &str = r#"
use env
use fake { source = "prov" }
resource db.postgres server { name = "server" }
let kc = format("%s@%s", env.var("R45_KUBECONFIG"), server.endpoint)
provider k8s { kubeconfig = kc }
resource k8s.namespace ns { metadata.name = "app" }
"#;

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> std::process::Command {
    let mut c = common::dform();
    c.args(args)
        .env("R45_KUBECONFIG", "kc")
        .env("DFORM_WAIT_POLL_MS", "50")
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    c
}

/// `dform dev --world w.json ARGS` (ARGS from the verb on).
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    Run::from(dform(s, &all).output().unwrap())
}

fn audit(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// On a pty, answering each prompt in turn: what it printed up to each
/// prompt and after the last, and its exit code.
fn answers(s: &Scratch, answers: &[&str]) -> (Vec<String>, i32) {
    let cmd = dform(s, &["dev", "--world", "w.json", "apply", "p.df"]);
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

/// Tick 1 asks for the server; the boundary configures `k8s`; tick 2's
/// plan, now against it, is printed and asked for again.
#[test]
fn apply_asks_again_for_what_waited_on_the_provider() {
    let s = scratch("tick2-asks");
    let (said, code) = answers(&s, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].contains(
            "  waits on  provider k8s (kubeconfig from kc)  which this plan does not resolve"
        ),
        "{}",
        said[0]
    );
    assert!(
        said[0].ends_with("Apply this change to p? [y/N] "),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("provider k8s: configured after tick 1: kubeconfig = (sensitive)\n"),
        "{}",
        said[1]
    );
    assert!(
        said[1].contains("tick 2  1 change, now that tick 1 reported\n  + k8s.namespace ns"),
        "{}",
        said[1]
    );
    assert!(
        said[1].ends_with("Apply tick 2 to p? [y/N] "),
        "{}",
        said[1]
    );
    assert!(said[2].ends_with("apply: complete\n"), "{}", said[2]);
    assert!(s.read("w.json").contains("k8s.namespace"));
}

/// `n` at tick 2: the server stays made, the namespace is not; the next
/// apply resumes, the provider's settings known at plan time, and makes
/// it at its tick 1.
#[test]
fn declining_tick_two_keeps_tick_one() {
    let s = scratch("tick2-declined");
    let (said, code) = answers(&s, &["y", "n"]);
    assert_ne!(code, 0);
    assert!(
        said[2].contains("apply p: not confirmed at tick 2"),
        "{}",
        said[2]
    );
    assert!(s.read("w.fakecloud.json").contains("db.postgres"));
    assert!(!s.read("w.json").contains("k8s.namespace"));
    let r = dev(&s, &["apply", "--yes", "p.df"]).success();
    assert!(
        r.stdout.starts_with(
            "resuming the apply interrupted at tick 1; remaining: k8s.namespace[\"ns\"]\n\
             plan: 1 change (1 create) over 1 tick\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("  + k8s.namespace ns"), "{}", r.stdout);
    assert!(!r.stdout.contains("configured after"), "{}", r.stdout);
}

/// `--yes` applies both ticks: the addresses were in the printed plan.
#[test]
fn yes_applies_tick_two() {
    let s = scratch("tick2-yes");
    let r = dev(&s, &["apply", "--yes", "p.df"]).success();
    assert!(
        r.stdout
            .contains("tick 2  1 change, now that tick 1 reported"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let c = audit(&s, "configure");
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        c[0],
        serde_json::json!({
            "kind": "configure", "tick": 1, "provider": "k8s", "settings": ["kubeconfig"],
            "hash": c[0]["hash"], "prev": c[0]["prev"], "seq": c[0]["seq"], "time": c[0]["time"],
        })
    );
}

/// A plan file shows tick 1 and `later` without a diff for it: applying
/// the file makes tick 1 and stops before what it did not show.
#[test]
fn a_plan_file_stops_before_what_it_did_not_show() {
    let s = scratch("tick2-plan-file");
    dev(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    let r = Run::from(dform(&s, &["apply", "plan.json"]).output().unwrap()).failure();
    assert!(
        r.stderr.contains(
            "apply stopped after tick 1: tick 2 plans 1 change `later` held for a provider's \
             settings, which the approved plan did not show; "
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains(STOPPED), "{}", r.stderr);
    assert!(s.read("w.fakecloud.json").contains("db.postgres"));
    assert!(!s.read("w.json").contains("k8s.namespace"));
}

/// The server's endpoint not there yet after tick 1 (chaos `not-ready`, a
/// host still booting): the tick waits on it, then configures the
/// provider and applies what waited on it.
#[test]
fn the_boundary_waits_for_the_settings_then_configures() {
    let s = scratch("tick2-waits");
    let r = dev(
        &s,
        &[
            "--chaos",
            "not-ready=db.postgres[\"server\"].endpoint:3",
            "apply",
            "--yes",
            "p.df",
        ],
    )
    .success();
    assert!(
        r.stderr
            .contains("waiting on db.postgres[\"server\"].endpoint since "),
        "{}",
        r.stderr
    );
    assert!(
        r.stdout
            .contains("provider k8s: configured after tick 2: kubeconfig = (sensitive)\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("  + k8s.namespace ns"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let w = audit(&s, "wait");
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["result"], "resolved");
}

/// `--wait 0s`, the endpoint not there: the apply stops after tick 1,
/// naming the provider `later` waits on, as the plan's footer says.
#[test]
fn without_a_wait_the_apply_stops_naming_the_provider() {
    let s = scratch("tick2-no-wait");
    let r = dev(
        &s,
        &[
            "--chaos",
            "not-ready=db.postgres[\"server\"].endpoint:100",
            "apply",
            "--yes",
            "--wait",
            "0s",
            "p.df",
        ],
    )
    .failure();
    assert!(
        r.stderr.contains(
            "apply stopped at tick 2: nothing definite to apply, still waiting on \
             ?db.postgres[\"server\"].endpoint provider k8s (kubeconfig from kc)"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stdout.contains("configured after"), "{}", r.stdout);
}
