//! `dform secrets` (R-161): `list` names every secret of a deployment by
//! key, with its kind, generation, age, the cells that read it and how a
//! new value lands there, never a value; `rotate KEY` moves one key's
//! generation on, recorded in state and the audit log, and the next plan
//! changes exactly that value with the reason. A given secret is rotated
//! where it lives. Policy reads `secrets/4` (its age, through the clock)
//! and `rotated/3`.

mod common;
use common::{Run, Scratch, dform, repo, yes};

/// `dform ARGS` in `s`, as alice, at `now`.
fn run(s: &Scratch, now: &str, args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env("RANDOM_MASTER", "secrets-cmd-master")
        .env("DFORM_ACTOR", "alice")
        .env("DFORM_TEST_NOW", now);
    Run::from(c.output().unwrap())
}

const NOW: &str = "2026-10-07T09:00:00Z";

/// Two derived secrets: a k3s token in two servers' user data (a change
/// replaces a server; `prod` may not be destroyed) and a database
/// password in a Secret; and a password the operator gives.
fn project(name: &str, extra: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"force_new\", \"sensitive\"])\n\
               type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n\
               type_attr(db.secret, \"admin\", \"string\", [\"sensitive\"])\n"),
    );
    s.write(
        "stacks/p.df",
        &format!(
            r##"input admin: secret(string) = "given-by-the-operator"
use fake
let token = random.password("k3s")
resource compute.vm lab {{
  name = "lab"
  user_data = "#cloud-config token: ${{token}}"
}}
resource compute.vm prod {{
  name = "prod"
  user_data = "#cloud-config token: ${{token}}"
}}
lifecycle(prod, "prevent_destroy")
resource db.secret app {{
  password = random.password("db")
  admin = admin
}}
{extra}"##
        ),
    );
    s
}

fn world(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/p/remote.json")["resources"].clone()
}

#[test]
fn a_rotation_changes_one_secret_with_its_reason() {
    let s = project("secrets-rotate", "");
    run(&s, NOW, &["apply", "p"]).success();
    let before = world(&s);

    // The listing: by key, never a value.
    let r = run(&s, "2026-10-10T09:00:00Z", &["secrets", "list", "p"]).success();
    let db = before["db.secret::app"]["attrs"]["password"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!r.stdout.contains(&db), "{}", r.stdout);
    let line = |key: &str| {
        r.stdout
            .lines()
            .find(|l| l.starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("{key}: {}", r.stdout))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    // A key never rotated is as old as the master's first apply, in
    // the log by the clock.
    let without_age = |l: String| {
        let mut w: Vec<&str> = l.split(' ').collect();
        assert!(w[3].ends_with('d') || w[3].ends_with('h'), "{l}");
        w.remove(3);
        w.join(" ")
    };
    assert_eq!(
        without_age(line("db")),
        "db random 1 db.secret app.password update",
        "{}",
        r.stdout
    );
    assert_eq!(
        without_age(line("k3s")),
        "k3s random 1 compute.vm lab.user_data, compute.vm prod.user_data refused by \
         prevent_destroy",
        "{}",
        r.stdout
    );
    assert_eq!(
        line("admin"),
        "admin given db.secret app.admin update",
        "{}",
        r.stdout
    );

    // Rotating the k3s token says first that it replaces the servers.
    let r = run(
        &s,
        "2026-10-11T09:00:00Z",
        &["secrets", "rotate", "p", "k3s"],
    )
    .success();
    assert_eq!(
        r.stdout,
        "rotating k3s of p (random): generation 1 -> 2\n  compute.vm lab.user_data  forces \
         replace\n  compute.vm prod.user_data  refused by prevent_destroy\nrotated k3s of p: \
         generation 2, by alice; the next plan changes it\n"
    );
    // Moved back by forgetting the record: the database password alone.
    let mut st = s.json("dform.state/p/state.json");
    st["secrets"].as_object_mut().unwrap().remove("k3s");
    s.write("dform.state/p/state.json", &st.to_string());

    let r = run(
        &s,
        "2026-10-12T09:00:00Z",
        &["secrets", "rotate", "p", "db"],
    )
    .success();
    assert!(
        r.stdout
            .contains("rotated db of p: generation 2, by alice; the next plan changes it"),
        "{}",
        r.stdout
    );
    let st = s.json("dform.state/p/state.json");
    assert_eq!(
        st["secrets"]["db"],
        serde_json::json!({
            "generation": 2,
            "rotated_at": "2026-10-12T09:00:00Z",
            "by": "alice",
            "pending": true,
        }),
        "{st}"
    );
    let log = s.read("dform.state/p/state.audit.jsonl");
    let rotated: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "rotated")
        .collect();
    assert_eq!(rotated.len(), 2, "{log}");
    assert_eq!(rotated[1]["key"], "db");
    assert_eq!(rotated[1]["generation"], 2);
    assert_eq!(rotated[1]["who"], "alice");

    // The next plan: that value, with the reason, and nothing else.
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "      password = (sensitive) → (sensitive, generation 2, rotated 2026-10-12 by \
             alice)\n"
        ),
        "{}",
        r.stdout
    );
    let r = run(&s, NOW, &["plan", "-v", "p"]).success();
    assert!(
        r.stdout.contains(
            "(sensitive random.password(\"db\"), generation 2, rotated 2026-10-12 by alice)"
        ),
        "{}",
        r.stdout
    );
    let r = run(&s, "2026-10-13T09:00:00Z", &["secrets", "list", "p"]).success();
    assert!(
        r.stdout.lines().any(
            |l| l.split_whitespace().take(4).collect::<Vec<_>>() == ["db", "random", "2", "1d"]
        ),
        "{}",
        r.stdout
    );

    run(&s, NOW, &["apply", "p"]).success();
    let after = world(&s);
    assert_ne!(after["db.secret::app"]["attrs"]["password"], db);
    assert_eq!(
        after["compute.vm::lab"]["attrs"]["user_data"],
        before["compute.vm::lab"]["attrs"]["user_data"]
    );
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    assert_eq!(
        s.json("dform.state/p/state.json")["secrets"]["db"]["pending"],
        serde_json::Value::Null
    );
}

#[test]
fn a_given_secret_is_rotated_where_it_lives() {
    let s = project("secrets-given", "");
    run(&s, NOW, &["apply", "p"]).success();
    let r = run(&s, NOW, &["secrets", "rotate", "p", "admin"]).failure();
    assert_eq!(r.code, Some(1));
    assert!(
        r.stderr.contains(
            "secrets rotate admin: admin is given in p, and lives in the input admin: --set, an \
             input file or a settings block: rotate it there, then plan"
        ),
        "{}",
        r.stderr
    );
    let r = run(&s, NOW, &["secrets", "rotate", "p", "nope"]).failure();
    assert!(
        r.stderr
            .contains("secrets rotate nope: p has no secret nope (its keys: db, k3s)"),
        "{}",
        r.stderr
    );
    assert!(s.json("dform.state/p/state.json")["secrets"].is_null());
}

/// `secrets/4` gives a policy each secret's age through the clock;
/// `rotated/3` the rotations a plan carries, for an approval.
#[test]
fn policy_reads_a_secrets_age_and_its_rotation() {
    let s = project(
        "secrets-policy",
        r#"use time
deny "${k} is older than 90 days" where {
  secrets(k, "random", _, at)
  now = time.now()
  now - at > 90d
}
deny "rotates ${k} to generation ${g} (by ${by}): needs an approval" where rotated(k, g, by)
"#,
    );
    run(&s, NOW, &["apply", "p"]).success();
    run(&s, NOW, &["plan", "p"]).success();
    let r = run(&s, "2030-01-01T00:00:00Z", &["plan", "p"]).failure();
    assert!(
        r.stderr.contains("db is older than 90 days")
            && r.stderr.contains("k3s is older than 90 days"),
        "{}",
        r.stderr
    );
    run(
        &s,
        "2029-12-30T00:00:00Z",
        &["secrets", "rotate", "p", "db"],
    )
    .success();
    // The plan that carries the rotation lists the denies over it.
    let r = run(&s, "2030-01-01T00:00:00Z", &["plan", "p"]).failure();
    assert!(
        !r.stdout.contains("db is older") && r.stdout.contains("k3s is older than 90 days"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("rotates db to generation 2 (by alice): needs an approval"),
        "{}",
        r.stdout
    );
}

/// `generation:` derives a key's earlier value, and `random.verify_key`
/// is its signing key's public half: a rotated Synapse key publishes the
/// one before it.
#[test]
fn an_earlier_generation_overlaps_a_rotation() {
    let s = project(
        "secrets-overlap",
        r#"
let g = random.generation("synapse")
resource db.user signing {
  password = random.signing_key("synapse")
}
resource db.user verify {
  name = random.verify_key("synapse")
}
resource db.user old {
  name = random.verify_key("synapse", generation: list.max([1, g - 1]))
}
"#,
    );
    s.write(
        "providers/fake/schema.df",
        &(s.read("providers/fake/schema.df")
            + "type_provider(db.user, \"fakecloud\")\n\
               type_attr(db.user, \"password\", \"string\", [\"sensitive\"])\n"),
    );
    run(&s, NOW, &["apply", "p"]).success();
    let w = world(&s);
    let verify = w["db.user::verify"]["attrs"]["name"].clone();
    assert_eq!(w["db.user::old"]["attrs"]["name"], verify);
    assert!(
        verify.as_str().unwrap().starts_with("ed25519 a_"),
        "{verify}"
    );
    run(&s, NOW, &["secrets", "rotate", "p", "synapse"]).success();
    run(&s, NOW, &["apply", "p"]).success();
    let w = world(&s);
    assert_eq!(w["db.user::old"]["attrs"]["name"], verify, "{w}");
    assert_ne!(w["db.user::verify"]["attrs"]["name"], verify, "{w}");
}
