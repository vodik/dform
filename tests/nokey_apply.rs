//! A run that does not hold the deployment's master (R-164): a second
//! operator, or CI, with no passphrase. It plans in full: a secret it
//! derives is a stand-in, each leaf proven unchanged by the derivation
//! digest state recorded, each other change marked `needs the key`. Its
//! apply makes every change that sends no stand-in and stops before the
//! rest (exit 5), listing them; a plan file it writes holds no digest of a
//! secret and is applied the same way.

mod common;
use common::{Run, Scratch, dform, repo, yes};

const PASS: (&str, &str) = ("DFORM_TEST_PASSPHRASE", "correct horse battery staple");

/// `dform ARGS` in `s` with the environment `env`, and no passphrase
/// unless `env` gives one.
fn run(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env_remove("RANDOM_MASTER")
        .env_remove("DFORM_TEST_PASSPHRASE");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// examples/crud-api, its master sealed under the passphrase, applied by
/// the operator who has it.
fn applied(name: &str) -> Scratch {
    let s = Scratch::new(name);
    common::copy_dir(&repo().join("examples/crud-api"), &s.dir);
    let _ = std::fs::remove_dir_all(s.path("dform.state"));
    let toml = s.read("dform.toml") + "\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n";
    s.write("dform.toml", &toml);
    run(&s, &[PASS], &["apply"]).success();
    s
}

fn world(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/crud_api/remote.json")["resources"].clone()
}

fn password(s: &Scratch) -> String {
    world(s)["google.sql_user::crud_user"]["attrs"]["password"]
        .as_str()
        .unwrap()
        .to_string()
}

const STACK: &str = "stacks/crud_api.df";

/// The Secret labelled: a change that sends no secret.
fn label(s: &Scratch) {
    let text = s.read(STACK).replace(
        r#"metadata = { name: "crud-api-db", namespace: shop.metadata.name }"#,
        r#"metadata = { name: "crud-api-db", namespace: shop.metadata.name, labels: { team: "shop" } }"#,
    );
    s.write(STACK, &text);
}

/// The namespace labelled: a change of a resource that holds no secret.
fn namespace(s: &Scratch) {
    let text = s.read(STACK).replace(
        r#"metadata.labels = { "pod-security.kubernetes.io/enforce": "restricted" }"#,
        r#"metadata.labels = { "pod-security.kubernetes.io/enforce": "restricted", team: "shop" }"#,
    );
    s.write(STACK, &text);
}

/// The password rotated (a new key): a change only the master derives.
fn rotate(s: &Scratch) {
    let text = s.read(STACK).replace(
        r#"random.password("crud-api-db")"#,
        r#"random.password("crud-api-db-2")"#,
    );
    s.write(STACK, &text);
}

#[test]
fn a_label_change_plans_and_applies_without_the_master() {
    let s = applied("nokey-label");
    let pw = password(&s);
    label(&s);
    let r = run(&s, &[], &["plan"]).success();
    assert!(
        r.stderr
            .contains("planned without its master (DFORM_TEST_PASSPHRASE is not set)"),
        "{}",
        r.stderr
    );
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("~ k8s.secret db_conn") && r.stdout.contains("secrets unchanged"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("needs the key"), "{}", r.stdout);
    let r = run(&s, &[], &["apply"]).success();
    assert!(!r.stdout.contains(&pw) && !r.stderr.contains(&pw));
    let w = world(&s);
    assert_eq!(
        w["k8s.secret::db_conn"]["attrs"]["metadata"]["labels"]["team"], "shop",
        "{w}"
    );
    // The Secret kept the password the master derives, not a stand-in.
    assert_eq!(
        w["k8s.secret::db_conn"]["attrs"]["stringData"]["PGPASSWORD"], pw,
        "{w}"
    );
    assert_eq!(password(&s), pw);
    for env in [&[][..], &[PASS][..]] {
        let r = run(&s, env, &["plan"]).success();
        assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
    }
}

#[test]
fn a_rotated_secret_stops_the_apply_without_the_master() {
    let s = applied("nokey-rotate");
    let pw = password(&s);
    label(&s);
    rotate(&s);
    let r = run(&s, &[], &["plan"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.contains("~ google.sql_user crud_user")
                && l.ends_with("secret changed, needs the key")),
        "{}",
        r.stdout
    );
    let r = run(&s, &[], &["apply"]).stopped();
    assert!(
        r.stderr.contains("apply crud_api: stopped; ")
            && r.stderr
                .contains("google.sql_user crud_user: password only the master derives")
            && r.stderr
                .contains("k8s.secret db_conn: stringData.PGPASSWORD only the master derives"),
        "{}",
        r.stderr
    );
    // Nothing that needs the master was sent: the old password stays
    // everywhere, and no stand-in reached the world.
    let w = world(&s);
    assert_eq!(password(&s), pw);
    assert_eq!(
        w["k8s.secret::db_conn"]["attrs"]["stringData"]["PGPASSWORD"],
        pw
    );
    // The operator who has the master makes them.
    let r = run(&s, &[PASS], &["apply"]).success();
    assert!(!r.stdout.contains(&pw));
    let new = password(&s);
    assert_ne!(new, pw);
    assert_eq!(
        world(&s)["k8s.secret::db_conn"]["attrs"]["stringData"]["PGPASSWORD"],
        new
    );
    let r = run(&s, &[], &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
}

/// A plan file written without the master holds no digest of a secret (a
/// derived leaf by its derivation digest, a stand-in's), and is applied
/// the same way: what it can make, then a stop before what needs the
/// master.
#[test]
fn a_plan_file_without_the_master_stops_the_same_way() {
    let s = applied("nokey-plan-file");
    let pw = password(&s);
    namespace(&s);
    rotate(&s);
    run(&s, &[], &["plan", "--out", "plan.json"]).success();
    let file = s.json("plan.json");
    assert_eq!(file["unkeyed"], true, "{file}");
    let text = s.read("plan.json");
    assert!(
        !text.contains("\"digest\": \"") || !text.contains("hmac"),
        "{text}"
    );
    assert!(text.contains("\"derived\""), "{text}");
    run(&s, &[], &["apply", "plan.json"]).stopped();
    assert_eq!(password(&s), pw);
    assert_eq!(
        world(&s)["k8s.namespace::shop"]["attrs"]["metadata"]["labels"]["team"],
        "shop",
        "what needs no master was made"
    );
    // A plan file the master keyed is not applied without it.
    run(&s, &[PASS], &["plan", "--out", "keyed.json"]).success();
    let r = run(&s, &[], &["apply", "keyed.json"]).failure();
    assert!(
        r.stderr
            .contains("its digests are keyed with crud_api's master"),
        "{}",
        r.stderr
    );
}

/// Nothing a run without the master writes holds a digest of a secret
/// that is not keyed, nor a stand-in: state, the audit log and the plan
/// file hold keyed digests and derivation digests only.
#[test]
fn state_holds_no_unkeyed_digest() {
    let s = applied("nokey-digests");
    label(&s);
    run(&s, &[], &["apply"]).success();
    let st = s.json("dform.state/crud_api/state.json");
    for (k, e) in st["resources"].as_object().unwrap() {
        for (p, d) in e["written"].as_object().into_iter().flatten() {
            assert!(
                d.as_str().is_some_and(|d| d.starts_with("hmac-sha256:")),
                "{k} {p}: {d}"
            );
        }
    }
    let derived = &st["resources"]["k8s.secret::db_conn"]["derived"];
    assert!(
        derived["stringData.PGPASSWORD"].is_string(),
        "a derived leaf has its derivation digest: {st}"
    );
}

/// A write-only attribute that derives a secret (a k3s token in an
/// instance's user data, R-106): the world never answers it, so a run
/// without the master proves it unchanged by its derivation digest, and
/// plans no replace; an update of that object sends it whole, so it
/// needs the master, while a new object that holds none is made.
#[test]
fn a_write_only_secret_is_proven_unchanged_without_the_master() {
    let s = Scratch::project("nokey-write-only");
    s.write(
        "dform.toml",
        &(s.read("dform.toml") + "\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n"),
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \"sensitive\"])\n"),
    );
    let program = |weight: u32, extra: &str| {
        format!(
            r##"use fake
let token = random.password("k3s")
resource compute.vm a {{
  name = "a"
  weight = {weight}
  user_data = "#cloud-config token: ${{token}}"
}}
{extra}"##
        )
    };
    s.write("stacks/p.df", &program(1, ""));
    run(&s, &[PASS], &["apply", "p"]).success();
    let r = run(&s, &[], &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    // An object with no secret is made; an update of the one that holds
    // the token is not.
    s.write(
        "stacks/p.df",
        &program(2, "resource compute.vm b {\n  name = \"b\"\n}\n"),
    );
    let r = run(&s, &[], &["plan", "p"]).success();
    assert!(
        r.stdout
            .contains("secrets unchanged, a write-only one needs the key"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("replace"), "{}", r.stdout);
    let r = run(&s, &[], &["apply", "p"]).stopped();
    assert!(
        r.stderr
            .contains("compute.vm a: user_data only the master derives"),
        "{}",
        r.stderr
    );
    let w = s.json("dform.state/p/remote.json");
    assert!(w["resources"]["compute.vm::b"].is_object(), "{w}");
    assert_eq!(w["resources"]["compute.vm::a"]["attrs"]["weight"], 1, "{w}");
    run(&s, &[PASS], &["apply", "p"]).success();
    let r = run(&s, &[], &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}
