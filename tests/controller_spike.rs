//! Controller mode (docs/experimental/controller.md): program files and tables as
//! sources, events,
//! drift as facts and the policy gate, driven one event at a time with
//! `--once`, and the polling loop once with `--max-events`.

mod common;
use common::Scratch;
use std::process::Command;

const WORKLOAD: &str = include_str!("../examples/bootstrap/stacks/workload.df");
/// The workload's table of examples/bootstrap/dform.toml.
const MANIFEST: &str = "[project]\nedition = \"2026\"\n\n[stacks.workload]\napprovals = 'jwks_file(\"approvers.jwks.json\")'\n";
const WORLD: &str = "dform.state/workload/remote.json";

fn release(s: &Scratch, image: &str) {
    s.write("data/releases.df", &format!("\n\nrelease(\"{image}\")\n"));
}

/// data/approvals.df, a module of facts (R-39), holding `facts`.
fn approvals(s: &Scratch, facts: &str) {
    s.write(
        "data/approvals.df",
        &format!("\n\ndecl approve(type, address)\ndecl approval(token)\n{facts}"),
    );
}

fn setup(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("dform.toml", MANIFEST);
    s.write("stacks/workload.df", WORKLOAD);
    release(&s, "gcr.io/renfry/web:1.0");
    approvals(&s, "");
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
    let mut args = extra.to_vec();
    args.extend(["controller", "run", "stacks/workload.df", "--once"]);
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
            "stack workload is undeformed",
        ]
    );
    // Nothing changed: a resync that finds nothing to do.
    assert_eq!(
        once(&s, &[]),
        ["event resync", "stack workload is undeformed"]
    );

    // A release is an input change: it deploys.
    release(&s, "gcr.io/renfry/web:1.1");
    assert_eq!(
        once(&s, &[]),
        [
            "input data.releases changed (file data/releases.df)",
            "event input data.releases",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("web:1.1"));

    // Replicas drift: the policy's auto_reconcile corrects it.
    edit_world(&s, "\"replicas\": 3", "\"replicas\": 5");
    assert_eq!(
        once(&s, &[]),
        [
            &format!("event world {WORLD} changed"),
            "drift k8s.deployment[\"web\"].spec.replicas: 3 -> 5 (auto_reconcile)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("\"replicas\": 3"));

    // Image drift: no auto_reconcile, so it is held, and held again at the
    // next event, until approved.
    edit_world(&s, "gcr.io/renfry/web:1.1", "evil:latest");
    let held = [
        "drift k8s.deployment[\"web\"].spec.template.spec.containers[0].image: \
         \"gcr.io/renfry/web:1.1\" -> \"evil:latest\" (held until approve or an input change)",
        "tick 1: plan: 1 deformation (1 update)",
        "tick 1: proceed: held, drift at spec.template.spec.containers[0].image needs approval: \
         k8s.deployment[\"web\"]",
        "stack workload is deformed: k8s.deployment[\"web\"] held",
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
    // approve(T, A) from its module: stated before the drift, so the drift
    // arrives with a world event, not an input change.
    approvals(&s, "approve(\"k8s.deployment\", \"web\")\n");
    assert_eq!(
        once(&s, &[]),
        [
            "input data.approvals changed (file data/approvals.df)",
            "event input data.approvals",
            "stack workload is undeformed",
        ]
    );
    edit_world(&s, "gcr.io/renfry/web:1.0", "evil:latest");
    assert_eq!(
        once(&s, &[]),
        [
            &format!("event world {WORLD} changed"),
            "drift k8s.deployment[\"web\"].spec.template.spec.containers[0].image: \
             \"gcr.io/renfry/web:1.0\" -> \"evil:latest\" (approved)",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
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
        "stack workload is deformed: k8s.deployment[\"web\"] held"
    );
    release(&s, "gcr.io/renfry/web:1.2");
    let got = once(&s, &[]);
    assert!(
        got.contains(
            &"drift k8s.deployment[\"web\"].spec.template.spec.containers[0].image: \
              \"gcr.io/renfry/web:1.0\" -> \"evil:latest\" (reconciled with the input change)"
                .to_string()
        ),
        "{got:#?}"
    );
    assert_eq!(got.last().unwrap(), "stack workload is undeformed");
    assert!(s.read(WORLD).contains("web:1.2"));
}

/// The published digest of a held approval.
fn pending_digest(s: &Scratch) -> String {
    let doc: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/workload/approval-pending.json")).unwrap();
    doc["digest"].as_str().unwrap().to_string()
}

/// A token from the example signer (`dform-approve`), as an `approval/1`
/// fact.
fn approval_fact(s: &Scratch, digest: &str) -> String {
    let out = Command::new(common::exe("dform-approve"))
        .args(["sign", "approver.key", "--digest", digest])
        .args(["--stack", "workload", "--approver", "alice"])
        .args(["--format", "fact"])
        .current_dir(&s.dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_prod_rollout_is_held_until_its_plan_is_approved() {
    let s = setup("ctl-hold");
    let out = Command::new(common::exe("dform-approve"))
        .args(["keygen", "approver.key"])
        .current_dir(&s.dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    s.write(
        "approvers.jwks.json",
        &String::from_utf8(out.stdout).unwrap(),
    );
    let prod = ["--set", "env=prod"];
    // The hold covers a create as well; the rest applies, and the tick
    // after holds it again, publishing that plan's digest.
    let got = once(&s, &prod);
    let digest = pending_digest(&s);
    assert_eq!(
        got,
        [
            "event start".to_string(),
            "tick 1: plan: 3 deformations (3 create)".to_string(),
            "tick 1: proceed: held, needs approval (a prod rollout): k8s.deployment[\"web\"]"
                .to_string(),
            got[3].clone(),
            "tick 2: plan: 1 deformation (1 create)".to_string(),
            "tick 2: proceed: held, needs approval (a prod rollout): k8s.deployment[\"web\"]"
                .to_string(),
            format!("tick 2: approval needed: plan digest {digest} (approval-pending.json)"),
            "stack workload is deformed: k8s.deployment[\"web\"] held".to_string(),
        ]
    );
    assert!(!s.read(WORLD).contains("k8s.deployment"));
    // A token for the digest, through the module of facts, releases it.
    let fact = approval_fact(&s, &digest);
    approvals(&s, &fact);
    assert_eq!(
        once(&s, &prod),
        [
            "input data.approvals changed (file data/approvals.df)".to_string(),
            "event input data.approvals".to_string(),
            "tick 1: plan: 1 deformation (1 create)".to_string(),
            format!("tick 1: approved by alice: plan digest {digest}"),
            "stack workload is undeformed".to_string(),
        ]
    );
    assert!(s.read(WORLD).contains("k8s.deployment"));
    assert!(
        !s.path("dform.state/workload/approval-pending.json")
            .exists()
    );
    // The next release is another plan: held again, the old token is for
    // another digest.
    release(&s, "gcr.io/renfry/web:1.1");
    let got = once(&s, &prod);
    assert_eq!(
        got[3],
        "tick 1: proceed: held, needs approval (a prod rollout): k8s.deployment[\"web\"]"
    );
    let digest = pending_digest(&s);
    // A token in the drop directory beside the state releases it too.
    let fact = approval_fact(&s, &digest);
    let token = fact
        .trim()
        .trim_start_matches("approval(\"")
        .trim_end_matches("\")");
    s.write("dform.state/workload/approvals/alice.token", token);
    let got = once(&s, &prod);
    assert_eq!(
        got[..2],
        [
            "event approval (dform.state/workload/approvals changed)".to_string(),
            "tick 1: plan: 1 deformation (1 update)".to_string(),
        ]
    );
    assert_eq!(got.last().unwrap(), "stack workload is undeformed");
    assert!(s.read(WORLD).contains("web:1.1"));
}

/// The release is a module of facts, read like any program file; `from
/// facts(..)` is gone (R-39) and says what replaces it.
#[test]
fn plan_reads_a_module_of_facts() {
    let s = setup("ctl-plan");
    let r = s.run(&["plan", "stacks/workload.df"]).success();
    assert!(
        r.stdout
            .contains("spec.template.spec.containers[name=web].image = \"gcr.io/renfry/web:1.0\""),
        "{}",
        r.stdout
    );
    s.write(
        "bad.df",
        "\n\ninput r from facts(\"r.facts\")\ndecl r(a)\nq(x) where r(x)\n",
    );
    let r = s.run(&["plan", "bad.df"]).failure();
    assert!(
        r.stderr
            .contains("`facts(..)` is gone (R-39): a `.df` file of facts is a module"),
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
        s.write("work/web.csv", &format!("image\n{image}\n"));
        let w = s.path("work");
        git(&w, &["add", "web.csv"]);
        git(&w, &["commit", "-q", "-m", image]);
        git(&w, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    };
    commit("gcr.io/renfry/web:2.0");
    s.write(
        "stacks/workload.df",
        &WORKLOAD
            // Replace the `use data.releases` import (and its comment) with
            // an inline relation input, placed with the other `input` and
            // `decl` lines (R-11a) so the file still has its header in
            // order: an `input` after `decl approve`/`decl approval` would
            // otherwise be a header statement below the body (R-27).
            .replace(
                "# The release to run, `releases.release(image)`; a change to the file is\n\
                 # an input event, as to any program file.\n\
                 use data.releases\n",
                "",
            )
            .replace(
                "input env: enum(\"dev\", \"prod\") = \"dev\"\n",
                "input env: enum(\"dev\", \"prod\") = \"dev\"\n\
                 input release from csv(git(\"releases.git\", \"main\", \"web.csv\"))\n\
                 decl release(image: string)\n",
            )
            .replace("where releases.release(image)", "where release(image)"),
    );
    let got = once(&s, &[]);
    assert_eq!(got.last().unwrap(), "stack workload is undeformed");
    assert!(s.read(WORLD).contains("web:2.0"));
    commit("gcr.io/renfry/web:2.1");
    let got = once(&s, &[]);
    assert_eq!(
        got[0],
        "input release changed (git releases.git main:web.csv)"
    );
    assert!(s.read(WORLD).contains("web:2.1"));
}

#[test]
fn the_polling_loop_runs_an_event_per_change() {
    use std::io::BufRead;
    let s = setup("ctl-loop");
    let mut child = common::dform()
        .args([
            "controller",
            "run",
            "--poll",
            "20",
            "--max-events",
            "2",
            "stacks/workload.df",
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
            "stack workload is undeformed",
            "input data.releases changed (file data/releases.df)",
            "event input data.releases",
            "tick 1: plan: 1 deformation (1 update)",
            "stack workload is undeformed",
        ]
    );
    assert!(s.read(WORLD).contains("web:3.0"));
}

#[test]
fn the_controller_runs_a_stack_of_the_project() {
    let s = setup("ctl-stack");
    let r = s.run(&["controller", "run", "other", "--once"]).failure();
    assert!(
        r.stderr.contains("no stack other in the project")
            && r.stderr.contains("workload  stacks/workload.df"),
        "{}",
        r.stderr
    );
}

/// Controller mode is experimental (R-41): `--help`, `stack --help` and
/// the completions list `controller` and `stack handover` only with
/// `DFORM_EXPERIMENTAL=1`; they run either way, each run with the warning.
#[test]
fn controller_mode_is_listed_only_when_experimental() {
    let s = setup("ctl-hidden");
    let help = |args: &[&str], on: bool| {
        let mut c = common::dform();
        c.args(args)
            .current_dir(&s.dir)
            .env_remove("DFORM_EXPERIMENTAL");
        if on {
            c.env("DFORM_EXPERIMENTAL", "1");
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    assert!(!help(&["--help"], false).contains("controller"));
    assert!(help(&["--help"], true).contains("controller"));
    assert!(!help(&["stack", "--help"], false).contains("handover"));
    assert!(help(&["stack", "--help"], true).contains("handover"));
    assert!(!help(&["completions", "zsh"], false).contains("controller"));
    assert!(!help(&["__complete", "stack"], false).contains("handover"));
    let r = s
        .run(&["controller", "run", "workload", "--once"])
        .success();
    assert!(
        r.stderr
            .contains("warning: controller mode is experimental"),
        "{}",
        r.stderr
    );
}
