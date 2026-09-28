//! Approvals (README "Approvals"): `requires_approval(D, Reason)` in the
//! policy pass makes the plan print a "needs approval" section and its
//! digest, and `apply PLAN --approval FILE` verifies a signed statement
//! over that digest offline before any Apply call. The tokens come from
//! the example signer, `dform-approve`, on examples/approvals/stacks/approvals.df.

mod common;
use common::Scratch;
use std::process::Command;

const PROGRAM: &str = include_str!("../examples/approvals/stacks/approvals.df");
const PROD: [&str; 2] = ["--set", "env=prod"];
const NEW_CIDR: [&str; 2] = ["--set", "cidr=10.1.0.0/16"];

/// `dform-approve ARGS` in the scratch directory: its stdout.
fn signer(s: &Scratch, args: &[&str]) -> String {
    let out = Command::new(common::exe("dform-approve"))
        .args(args)
        .current_dir(&s.dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "dform-approve {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The example program applied in prod, a key whose JWKS is its trust
/// root, and a plan file of a replace (`plan.json`); returns its digest.
fn setup(name: &str) -> (Scratch, String) {
    let s = Scratch::new(name);
    s.write("stacks/approvals.df", PROGRAM);
    let jwks = signer(&s, &["keygen", "approver.key"]);
    s.write("approvers.jwks.json", &jwks);
    s.run(&[&["--file", "stacks/approvals.df", "apply"][..], &PROD].concat())
        .success();
    let r = s
        .run(
            &[
                &[
                    "--file",
                    "stacks/approvals.df",
                    "plan",
                    "--out",
                    "plan.json",
                ][..],
                &PROD,
                &NEW_CIDR,
            ]
            .concat(),
        )
        .success();
    let file: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    let digest = file["digest"].as_str().unwrap().to_string();
    assert!(
        r.stdout.contains(&format!("plan digest: {digest}\n")),
        "{}",
        r.stdout
    );
    (s, digest)
}

/// A token signed with `approver.key`, written to `name`.
fn token(s: &Scratch, name: &str, digest: &str, extra: &[&str]) {
    let mut args = vec![
        "sign",
        "approver.key",
        "--digest",
        digest,
        "--stack",
        "approvals.demo",
        "--key",
        "env=prod",
    ];
    if !extra.contains(&"--approver") {
        args.extend(["--approver", "alice"]);
    }
    args.extend(extra);
    let t = signer(s, &args);
    s.write(name, &t);
}

fn world(s: &Scratch) -> String {
    s.read("stacks/.dform/approvals.demo/env=prod/remote.json")
}

#[test]
fn a_prod_replace_plans_with_needs_approval() {
    let (s, digest) = setup("approvals-plan");
    let args = [
        &["--file", "stacks/approvals.df", "plan"][..],
        &PROD,
        &NEW_CIDR,
    ]
    .concat();
    let r = s.run(&args).success();
    assert!(
        r.stdout
            .contains("needs approval:\n  net.vpc.main  (a replace in prod)\n"),
        "{}",
        r.stdout
    );
    // The digest is the plan file's: the same plan, the same digest.
    assert!(
        r.stdout.contains(&format!("plan digest: {digest}\n")),
        "{}",
        r.stdout
    );
    let r = s.run(&[&args[..], &["--json"]].concat()).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["digest"], digest.as_str());
    assert_eq!(
        j["needs_approval"],
        serde_json::json!([{ "deformation": "net.vpc.main", "reason": "a replace in prod" }])
    );
    // Staging: the policy asks for nothing, and the plan says nothing.
    let staging = [&["--file", "stacks/approvals.df", "plan"][..], &NEW_CIDR].concat();
    let r = s.run(&staging).success();
    assert!(!r.stdout.contains("needs approval"), "{}", r.stdout);
    assert!(!r.stdout.contains("plan digest"), "{}", r.stdout);
}

#[test]
fn apply_refuses_until_a_valid_token_for_the_plans_digest() {
    let (s, digest) = setup("approvals-apply");
    // No token.
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "apply refused: net.vpc.main (a replace in prod) needs an approval, and no \
             --approval was given"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains(&digest), "{}", r.stderr);
    // A plain apply has no digest to approve.
    let r = s
        .run(
            &[
                &["--file", "stacks/approvals.df", "apply"][..],
                &PROD,
                &NEW_CIDR,
            ]
            .concat(),
        )
        .failure();
    assert!(r.stderr.contains("plan --out PLAN"), "{}", r.stderr);
    // A token for another plan.
    let other = format!("sha256:{}", "0".repeat(64));
    token(&s, "other.json", &other, &[]);
    let r = s
        .run(&["apply", "plan.json", "--approval", "other.json"])
        .failure();
    assert!(
        r.stderr.contains(&format!(
            "apply refused: approval by alice: it approves plan digest {other}, and this \
             plan's is {digest}"
        )),
        "{}",
        r.stderr
    );
    // An expired token.
    token(
        &s,
        "old.json",
        &digest,
        &["--expires", "2020-01-01T00:00:00Z"],
    );
    let r = s
        .run(&["apply", "plan.json", "--approval", "old.json"])
        .failure();
    assert!(
        r.stderr
            .contains("apply refused: approval by alice: it expired at 2020-01-01T00:00:00Z"),
        "{}",
        r.stderr
    );
    // Nothing was applied.
    assert!(world(&s).contains("10.0.0.0/16"), "{}", world(&s));
    // A valid one.
    token(&s, "ok.json", &digest, &[]);
    let r = s
        .run(&["apply", "plan.json", "--approval", "ok.json"])
        .success();
    assert!(
        r.stdout
            .contains(&format!("approved by alice: plan digest {digest}")),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("apply: complete"), "{}", r.stdout);
    assert!(world(&s).contains("10.1.0.0/16"), "{}", world(&s));
}

#[test]
fn a_jwt_approves_as_well() {
    let (s, digest) = setup("approvals-jwt");
    token(&s, "ok.jwt", &digest, &["--format", "jwt"]);
    s.run(&["apply", "plan.json", "--approval", "ok.jwt"])
        .success();
    assert!(world(&s).contains("10.1.0.0/16"), "{}", world(&s));
}

#[test]
fn a_token_for_another_deployment_signer_or_approver_is_refused() {
    let (s, digest) = setup("approvals-who");
    let refused = |file: &str| {
        s.run(&["apply", "plan.json", "--approval", file])
            .failure()
            .stderr
    };
    // Staging's approval is not prod's.
    let t = signer(
        &s,
        &[
            "sign",
            "approver.key",
            "--digest",
            &digest,
            "--stack",
            "approvals.demo",
            "--key",
            "env=staging",
            "--approver",
            "alice",
        ],
    );
    s.write("staging.json", &t);
    let e = refused("staging.json");
    assert!(
        e.contains(
            "approval by alice: it is for stack approvals.demo[env=staging], and this is \
             approvals.demo[env=prod]"
        ),
        "{e}"
    );
    // A key the trust root does not list.
    signer(&s, &["keygen", "mallory.key"]);
    let t = signer(
        &s,
        &[
            "sign",
            "mallory.key",
            "--digest",
            &digest,
            "--stack",
            "approvals.demo",
            "--key",
            "env=prod",
            "--approver",
            "alice",
        ],
    );
    s.write("mallory.json", &t);
    let e = refused("mallory.json");
    assert!(
        e.contains("apply refused: approval: signature: keyid")
            && e.contains("not in the stack's trust root"),
        "{e}"
    );
    // A forged payload under a real signature.
    token(&s, "ok.json", &digest, &[]);
    let mut env: serde_json::Value = serde_json::from_str(&s.read("ok.json")).unwrap();
    let payload = env["payload"].as_str().unwrap().to_string();
    env["payload"] = serde_json::Value::String(format!("{}AA", &payload[..payload.len() - 2]));
    s.write("forged.json", &env.to_string());
    let e = refused("forged.json");
    assert!(e.contains("the signature does not verify"), "{e}");
    // approver_allowed admits alice and bob only.
    token(&s, "carol.json", &digest, &["--approver", "carol"]);
    let e = refused("carol.json");
    assert!(
        e.contains(
            "approval by carol: approver_allowed(\"carol\", D) does not hold for net.vpc.main"
        ),
        "{e}"
    );
    assert!(world(&s).contains("10.0.0.0/16"), "{}", world(&s));
}

#[test]
fn an_edited_plan_file_is_refused() {
    let (s, digest) = setup("approvals-edited");
    token(&s, "ok.json", &digest, &[]);
    let text = s.read("plan.json");
    s.write("plan.json", &text.replace("10.1.0.0/16", "10.9.0.0/16"));
    let r = s
        .run(&["apply", "plan.json", "--approval", "ok.json"])
        .failure();
    assert!(
        r.stderr.contains("plan file plan.json: its digest")
            && r.stderr.contains("it was edited after the plan"),
        "{}",
        r.stderr
    );
}

/// A git input relation's commit is pinned in the plan file: a ref that
/// moved since the plan is a stale plan.
#[test]
fn a_moved_git_commit_is_a_stale_plan() {
    let s = Scratch::new("approvals-git");
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&s.dir, &["init", "-q", "--bare", "ops.git"]);
    git(&s.dir, &["clone", "-q", "ops.git", "work"]);
    let commit = |text: &str| {
        s.write("work/tags.facts", &format!("edition 2026\n\n{text}\n"));
        let w = s.path("work");
        git(&w, &["add", "tags.facts"]);
        git(&w, &["commit", "-q", "-m", text]);
        git(&w, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    };
    commit("owner(\"a\")");
    s.write(
        "p.df",
        "edition 2026\n\ninput relation owner/1 from git(\"ops.git\", \"main\", \"tags.facts\")\n\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n}\n",
    );
    s.run(&["--file", "p.df", "plan", "--out", "plan.json"])
        .success();
    let file: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    assert_eq!(
        file["git_commits"][0]["source"], "git ops.git main:tags.facts",
        "{file}"
    );
    // Another commit whose facts the program does not read differently:
    // the delta is the same, the pinned commit is not.
    commit("owner(\"a\")\nowner(\"b\")");
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("input relation git ops.git main:tags.facts: the plan read commit"),
        "{}",
        r.stderr
    );
}
