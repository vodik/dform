//! Controller mode (README "Controller mode"): input relations, events,
//! drift as facts and the policy gate, driven one event at a time with
//! `--once`, and the polling loop once with `--max-events`.

mod common;
use common::Scratch;
use std::process::Command;

const WORKLOAD: &str = include_str!("../examples/bootstrap/workload.df");
const WORLD: &str = ".dform/renfry.workload/remote.json";

fn release(s: &Scratch, image: &str) {
    s.write(
        "release.facts",
        &format!("edition 2026\n\nrelease(\"{image}\")\n"),
    );
}

fn setup(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("workload.df", WORKLOAD);
    release(&s, "gcr.io/renfry/web:1.0");
    s.write("approvals.facts", "edition 2026\n");
    s
}

/// The controller's log without its `HH:MM:SS ` stamps, and without the
/// first line (which names the files).
fn log(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| {
            assert!(
                l.len() > 9 && l.as_bytes()[2] == b':' && l.as_bytes()[8] == b' ',
                "not a log line: {l}"
            );
            l[9..].to_string()
        })
        .filter(|l| !l.starts_with("controller "))
        .collect()
}

fn once(s: &Scratch, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["--file", "workload.df"];
    args.extend_from_slice(extra);
    args.extend(["controller", "--stack", "renfry.workload", "--once"]);
    log(&s.run(&args).success().stdout)
}

fn edit_world(s: &Scratch, from: &str, to: &str) {
    let w = s.read(WORLD);
    assert!(w.contains(from), "{w}");
    s.write(WORLD, &w.replace(from, to));
}

#[test]
fn input_changes_deploy_and_world_drift_is_gated_by_policy() {
    let s = setup("ctl-gate");
    assert_eq!(
        once(&s, &[]),
        [
            "event start",
            "tick 1: plan: 3 deformations (3 create)",
            "stack renfry.workload is undeformed",
        ]
    );
    // Nothing changed: a resync that finds nothing to do.
    assert_eq!(
        once(&s, &[]),
        ["event resync", "stack renfry.workload is undeformed"]
    );

    // A release is an input change: it deploys.
    release(&s, "gcr.io/renfry/web:1.1");
    assert_eq!(
        once(&s, &[]),
        [
            "input release changed (file release.facts)",
            "event input release",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("web:1.1"));

    // Replicas drift: the policy's auto_reconcile corrects it.
    edit_world(&s, "\"replicas\": 3", "\"replicas\": 5");
    assert_eq!(
        once(&s, &[]),
        [
            &format!("event world {WORLD} changed"),
            "drift k8s.deployment.web spec.replicas: 3 -> 5 (auto_reconcile)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("\"replicas\": 3"));

    // Image drift: no auto_reconcile, so it is held, and held again at the
    // next event, until approved.
    edit_world(&s, "gcr.io/renfry/web:1.1", "evil:latest");
    let held = [
        "drift k8s.deployment.web spec.template.spec.containers[0].image: \
         \"gcr.io/renfry/web:1.1\" -> \"evil:latest\" (held until approve or an input change)",
        "tick 1: plan: 1 deformation (1 update)",
        "tick 1: proceed: held, drift at spec.template.spec.containers[0].image needs approval: \
         k8s.deployment.web",
        "stack renfry.workload is deformed: k8s.deployment.web held",
    ];
    let mut want = vec![format!("event world {WORLD} changed")];
    want.extend(held.iter().map(|l| l.to_string()));
    assert_eq!(once(&s, &[]), want);
    assert!(s.read(WORLD).contains("evil:latest"));
    let mut want = vec!["event resync".to_string()];
    want.extend(held.iter().map(|l| l.to_string()));
    assert_eq!(once(&s, &[]), want);
}

#[test]
fn approve_lets_a_world_event_correct_drift() {
    let s = setup("ctl-approve");
    once(&s, &[]);
    // approve(T, A) from its input relation: stated before the drift, so
    // the drift arrives with a world event, not an input change.
    s.write(
        "approvals.facts",
        "edition 2026\n\napprove(\"k8s.deployment\", \"web\")\n",
    );
    assert_eq!(
        once(&s, &[]),
        [
            "input approve release_approved changed (file approvals.facts)",
            "event input approve release_approved",
            "stack renfry.workload is undeformed",
        ]
    );
    edit_world(&s, "gcr.io/renfry/web:1.0", "evil:latest");
    assert_eq!(
        once(&s, &[]),
        [
            &format!("event world {WORLD} changed"),
            "drift k8s.deployment.web spec.template.spec.containers[0].image: \
             \"gcr.io/renfry/web:1.0\" -> \"evil:latest\" (approved)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("web:1.0"));
}

#[test]
fn an_input_change_reconciles_held_drift() {
    let s = setup("ctl-input-releases");
    once(&s, &[]);
    edit_world(&s, "gcr.io/renfry/web:1.0", "evil:latest");
    assert_eq!(
        once(&s, &[]).last().unwrap(),
        "stack renfry.workload is deformed: k8s.deployment.web held"
    );
    release(&s, "gcr.io/renfry/web:1.2");
    let got = once(&s, &[]);
    assert!(
        got.contains(
            &"drift k8s.deployment.web spec.template.spec.containers[0].image: \
              \"gcr.io/renfry/web:1.0\" -> \"evil:latest\" (reconciled with the input change)"
                .to_string()
        ),
        "{got:#?}"
    );
    assert_eq!(got.last().unwrap(), "stack renfry.workload is undeformed");
    assert!(s.read(WORLD).contains("web:1.2"));
}

#[test]
fn a_prod_rollout_is_held_until_its_release_is_approved() {
    let s = setup("ctl-hold");
    let prod = ["--set", "env=prod"];
    // The hold covers a create as well; the rest applies, and the tick
    // after holds it again.
    assert_eq!(
        once(&s, &prod),
        [
            "event start",
            "tick 1: plan: 3 deformations (3 create)",
            "tick 1: proceed: held, needs approval: k8s.deployment.web",
            "tick 2: plan: 1 deformation (1 create)",
            "tick 2: proceed: held, needs approval: k8s.deployment.web",
            "stack renfry.workload is deformed: k8s.deployment.web held",
        ]
    );
    assert!(!s.read(WORLD).contains("k8s.deployment"));
    s.write(
        "approvals.facts",
        "edition 2026\n\nrelease_approved(\"gcr.io/renfry/web:1.0\")\n",
    );
    assert_eq!(
        once(&s, &prod),
        [
            "input approve release_approved changed (file approvals.facts)",
            "event input approve release_approved",
            "tick 1: plan: 1 deformation (1 create)",
            "stack renfry.workload is undeformed",
        ]
    );
    // The next release is held again.
    release(&s, "gcr.io/renfry/web:1.1");
    assert_eq!(
        once(&s, &prod)[3],
        "tick 1: proceed: held, needs approval: k8s.deployment.web"
    );
}

#[test]
fn plan_reads_an_input_relation_and_rejects_a_stray_fact() {
    let s = setup("ctl-plan");
    let r = s.run(&["--file", "workload.df", "plan"]).success();
    assert!(
        r.stdout
            .contains("spec.template.spec.containers[name=web].image = \"gcr.io/renfry/web:1.0\""),
        "{}",
        r.stdout
    );
    s.write(
        "release.facts",
        "edition 2026\n\nrelease(\"a\")\nrelaese(\"b\")\n",
    );
    let r = s.run(&["--file", "workload.df", "plan"]).failure();
    assert!(
        r.stderr
            .contains("relaese/1 is not an input relation declared from file release.facts"),
        "{}",
        r.stderr
    );
    s.write(
        "bad.df",
        "edition 2026\n\ninput relation r/1 from url(\"http://x\")\nq(x) if r(x)\n",
    );
    let r = s.run(&["--file", "bad.df", "plan"]).failure();
    assert!(
        r.stderr.contains("input relation r/1: unknown source"),
        "{}",
        r.stderr
    );
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_git_source_is_read_at_its_ref() {
    let s = setup("ctl-git");
    git(&s.dir, &["init", "-q", "--bare", "releases.git"]);
    git(&s.dir, &["clone", "-q", "releases.git", "work"]);
    let commit = |image: &str| {
        s.write(
            "work/web.facts",
            &format!("edition 2026\n\nrelease(\"{image}\")\n"),
        );
        let w = s.path("work");
        git(&w, &["add", "web.facts"]);
        git(&w, &["commit", "-q", "-m", image]);
        git(&w, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    };
    commit("gcr.io/renfry/web:2.0");
    s.write(
        "workload.df",
        &WORKLOAD.replace(
            "input relation release/1 from file(\"release.facts\")",
            "input relation release/1 from git(\"releases.git\", \"main\", \"web.facts\")",
        ),
    );
    let got = once(&s, &[]);
    assert_eq!(got.last().unwrap(), "stack renfry.workload is undeformed");
    assert!(s.read(WORLD).contains("web:2.0"));
    commit("gcr.io/renfry/web:2.1");
    let got = once(&s, &[]);
    assert_eq!(
        got[0],
        "input release changed (git releases.git main:web.facts)"
    );
    assert!(s.read(WORLD).contains("web:2.1"));
}

#[test]
fn the_polling_loop_runs_an_event_per_change() {
    use std::io::BufRead;
    let s = setup("ctl-loop");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dform"))
        .args([
            "--file",
            "workload.df",
            "controller",
            "--poll",
            "20",
            "--max-events",
            "2",
        ])
        .current_dir(&s.dir)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut lines = Vec::new();
    let mut changed = false;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) => {
                let done = l.ends_with("is undeformed");
                lines.push(l);
                if done && !changed {
                    release(&s, "gcr.io/renfry/web:3.0");
                    changed = true;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(e) => {
                let _ = child.kill();
                panic!("controller did not finish: {e}; log so far: {lines:#?}");
            }
        }
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(
        log(&lines.join("\n")),
        [
            "event start",
            "tick 1: plan: 3 deformations (3 create)",
            "stack renfry.workload is undeformed",
            "input release changed (file release.facts)",
            "event input release",
            "tick 1: plan: 1 deformation (1 update)",
            "stack renfry.workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("web:3.0"));
}

#[test]
fn the_controller_refuses_a_stack_that_is_not_the_programs() {
    let s = setup("ctl-stack");
    let r = s
        .run(&[
            "--file",
            "workload.df",
            "controller",
            "--stack",
            "other",
            "--once",
        ])
        .failure();
    assert!(
        r.stderr
            .contains("the program (workload.df) owns stack renfry.workload"),
        "{}",
        r.stderr
    );
}
