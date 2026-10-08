//! Approvals (README "Approvals"): `requires_approval(r, Reason)` in the
//! policy pass makes the plan hold the change for an approval and print its
//! digest, and `apply PLAN --approval FILE` verifies a signed statement
//! over that digest offline before any Apply call. The tokens come from
//! the example signer, `dform-approve`, on examples/approvals/stacks/approvals.df.

mod common;
use common::Scratch;
use std::process::Command;

const PROGRAM: &str = include_str!("../examples/approvals/stacks/approvals.df");
const MANIFEST: &str = include_str!("../examples/approvals/dform.toml");
const PROD: [&str; 1] = ["env=prod"];
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
    let s = Scratch::project(name);
    s.write("dform.toml", MANIFEST);
    s.write("stacks/approvals.df", PROGRAM);
    let jwks = signer(&s, &["keygen", "approver.key"]);
    s.write("approvers.jwks.json", &jwks);
    s.run(&[&["apply", "stacks/approvals.df"][..], &PROD].concat())
        .success();
    let r = s
        .run(
            &[
                &["plan", "--out", "plan.json", "stacks/approvals.df"][..],
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
        "approvals",
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
    s.read("dform.state/approvals/env=prod/remote.json")
}

#[test]
fn a_prod_replace_plans_with_needs_approval() {
    let (s, digest) = setup("approvals-plan");
    let args = [&["plan", "stacks/approvals.df"][..], &PROD, &NEW_CIDR].concat();
    let r = s.run(&args).success();
    assert!(
        r.stdout.contains("  ± net.vpc main  ")
            && r.stdout.contains("\nheld for approval\n  net.vpc main  ")
            && r.stdout
                .contains("  a replace in prod    stacks/approvals.df:21\n"),
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
        serde_json::json!([{ "deformation": "net.vpc[\"main\"]", "reason": "a replace in prod" }])
    );
    // Staging: the policy asks for nothing, and the plan says nothing.
    let staging = [&["plan", "stacks/approvals.df"][..], &NEW_CIDR].concat();
    let r = s.run(&staging).success();
    assert!(!r.stdout.contains("held: approval"), "{}", r.stdout);
    assert!(!r.stdout.contains("plan digest"), "{}", r.stdout);
}

#[test]
fn apply_refuses_until_a_valid_token_for_the_plans_digest() {
    let (s, digest) = setup("approvals-apply");
    // No token.
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "apply refused: net.vpc[\"main\"] (a replace in prod) needs an approval, and no \
             --approval was given"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains(&digest), "{}", r.stderr);
    // A plain apply has no digest to approve.
    let r = s
        .run(&[&["apply", "stacks/approvals.df"][..], &PROD, &NEW_CIDR].concat())
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
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
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
            "approvals",
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
            "approval by alice: it is for stack approvals[env=staging], and this is \
             approvals[env=prod]"
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
            "approvals",
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
            "approval by carol: approver_allowed(\"carol\", D) does not hold for net.vpc[\"main\"]"
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

/// A git table's commit is an extern answer the plan file records (R-39):
/// apply reads what plan read, though the ref moved since.
#[test]
fn a_git_table_is_read_at_the_planned_commit() {
    let s = Scratch::project("approvals-git");
    let git = |dir: &std::path::Path, args: &[&str]| {
        common::git(dir, args);
    };
    git(&s.dir, &["init", "-q", "--bare", "ops.git"]);
    git(&s.dir, &["clone", "-q", "ops.git", "work"]);
    let commit = |text: &str| {
        s.write("work/owners.csv", &format!("name\n{text}\n"));
        let w = s.path("work");
        git(&w, &["add", "owners.csv"]);
        git(&w, &["commit", "-q", "-m", text]);
        git(&w, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    };
    commit("a");
    s.write(
        "p.df",
        "\n\ninput owner from csv.decode(io.read(\"git+file:ops.git/owners.csv?ref=main\"))\n\n\
         decl owner(name: string)\n\nresource net.vpc main {\ncidr = \"10.0.0.0/16\"\n\
         tags = { owners: [ n | owner(n) ] }\n}\nuse fake\n",
    );
    s.run(&["plan", "--out", "plan.json", "p.df"]).success();
    let file = s.read("plan.json");
    // The rows, at the commit they were read at (R-153).
    assert!(
        file.contains("table.csv.owner") && file.contains("ops.git@"),
        "{file}"
    );
    commit("a\nb");
    s.run(&["apply", "plan.json"]).success();
    let world = s.read("dform.state/p/remote.json");
    assert!(
        world.contains("\"a\"") && !world.contains("\"b\""),
        "{world}"
    );
}
