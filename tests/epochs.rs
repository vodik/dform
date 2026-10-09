//! Master epochs (R-165): `dform secrets cycle` seals a new master as the
//! next epoch beside the current one, and no value changes; each secret
//! stays on the epoch it was derived on until `secrets rotate` moves it to
//! the current one, and the apply that moves an epoch's last secret
//! retires it (its sealed master deleted, audited). Digests of secrets are
//! keyed with the first epoch's master, carried over, so a write-only
//! value is still proven unchanged.

mod common;
use common::{Run, Scratch, dform, repo, yes};

const PASS: (&str, &str) = ("DFORM_TEST_PASSPHRASE", "correct horse battery staple");

/// `dform ARGS` in `s`, as alice, with the environment `env` (no
/// passphrase unless it gives one).
fn run(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env_remove("RANDOM_MASTER")
        .env_remove("DFORM_TEST_PASSPHRASE")
        .env("DFORM_ACTOR", "alice");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// Two passwords in a Secret, a token in a server's write-only user data
/// (a change replaces it), and with `memo` a secret memo; the master
/// sealed under the passphrase, applied.
fn applied(name: &str, memo: bool) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &(s.read("dform.toml") + "\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n"),
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \"sensitive\"])\n\
               type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n\
               type_attr(db.secret, \"admin\", \"string\", [\"sensitive\"])\n\
               type_attr(db.secret, \"cookie\", \"string\", [\"sensitive\"])\n"),
    );
    let cookie = match memo {
        true => "  cookie = memo.first(\"cookie\", random.base64(\"cookie\", 32))\n",
        false => "",
    };
    s.write(
        "stacks/p.df",
        &format!(
            r##"use fake
resource compute.vm server {{
  name = "server"
  user_data = "#cloud-config token: ${{random.password("k3s")}}"
}}
resource db.secret app {{
  password = random.password("db")
  admin = random.password("admin")
{cookie}}}
"##
        ),
    );
    run(&s, &[PASS], &["apply", "p"]).success();
    s
}

fn world(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/stacks.p/remote.json")["resources"].clone()
}

fn record(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/stacks.p/state.master")
}

fn log(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("dform.state/stacks.p/state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == kind)
        .collect()
}

#[test]
fn a_cycle_changes_nothing_and_a_rotation_moves_one_secret() {
    let s = applied("epochs-cycle", false);
    let before = world(&s);
    let first = record(&s)["id"].as_str().unwrap().to_string();

    let r = run(&s, &[PASS], &["secrets", "cycle", "p"]).success();
    assert!(
        r.stdout.contains(
            "cycled the master of p: epoch 2 (id ") && r.stdout.contains(
            ") is current, for new secrets and each one rotated; 3 secrets stay on epoch 1 until \
             rotated: admin, db, k3s"
        ),
        "{}",
        r.stdout
    );
    let rec = record(&s);
    assert_eq!(rec["epoch"], 2, "{rec}");
    assert_eq!(rec["earlier"][0]["epoch"], 1, "{rec}");
    assert_eq!(rec["earlier"][0]["id"], first, "{rec}");
    assert_ne!(rec["id"], first);
    assert_eq!(
        s.json("dform.state/stacks.p/state.json")["master"],
        rec["id"],
        "state is applied with the new master"
    );
    let cycled = log(&s, "cycled");
    assert_eq!(cycled.len(), 1);
    assert_eq!(cycled[0]["from"], first);
    assert_eq!(cycled[0]["who"], "alice");

    // No value changes: with the passphrase, and without it (each epoch's
    // stand-ins by its id); the write-only token's digest is the first
    // epoch's, so the server is not replaced.
    for env in [&[PASS][..], &[][..]] {
        let r = run(&s, env, &["plan", "p"]).success();
        assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    }
    let r = run(&s, &[PASS], &["secrets", "list", "p"]).success();
    assert!(
        r.stdout
            .contains("epoch 2 is current; 3 secrets on an earlier epoch"),
        "{}",
        r.stdout
    );
    let row = |key: &str| -> Vec<String> {
        r.stdout
            .lines()
            .find(|l| l.starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("{key}: {}", r.stdout))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    assert_eq!(row("db")[..5], ["db", "random", "1", "1", "(earlier)"]);

    // One rotated: exactly that value changes, from the current epoch.
    run(&s, &[PASS], &["secrets", "rotate", "p", "db"]).success();
    let r = run(&s, &[PASS], &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("      password = (sensitive) → (sensitive, generation 2"),
        "{}",
        r.stdout
    );
    run(&s, &[PASS], &["apply", "p"]).success();
    let after = world(&s);
    assert_ne!(
        after["db.secret::app"]["attrs"]["password"],
        before["db.secret::app"]["attrs"]["password"]
    );
    for (t, a) in [
        ("db.secret::app", "admin"),
        ("compute.vm::server", "user_data"),
    ] {
        assert_eq!(after[t]["attrs"][a], before[t]["attrs"][a], "{t}.{a}");
    }
    // Epoch 1 stays: the others derive from it.
    assert_eq!(record(&s)["earlier"][0]["epoch"], 1);
    assert!(log(&s, "retired").is_empty());
}

#[test]
fn the_apply_that_moves_an_epochs_last_secret_retires_it() {
    let s = applied("epochs-retire", true);
    let cookie = world(&s)["db.secret::app"]["attrs"]["cookie"].clone();
    run(&s, &[PASS], &["secrets", "cycle", "p"]).success();
    // A memo sealed under epoch 1 still opens, and holds it.
    let r = run(&s, &[PASS], &["secrets", "list", "p"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.split_whitespace().take(5).collect::<Vec<_>>()
                == ["cookie", "memo", "1", "1", "(earlier)"]),
        "{}",
        r.stdout
    );
    for key in ["db", "admin", "k3s"] {
        run(&s, &[PASS], &["secrets", "rotate", "p", key]).success();
    }
    run(&s, &[PASS], &["apply", "p"]).success();
    assert_eq!(world(&s)["db.secret::app"]["attrs"]["cookie"], cookie);
    assert_eq!(
        record(&s)["earlier"][0]["epoch"],
        1,
        "the memo holds epoch 1"
    );
    assert!(log(&s, "retired").is_empty());
    run(&s, &[PASS], &["secrets", "rotate", "p", "cookie"]).success();
    // Rotated, not applied: the epoch stays until the apply moves them.
    assert_eq!(record(&s)["earlier"][0]["epoch"], 1);
    let r = run(&s, &[PASS], &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    run(&s, &[PASS], &["apply", "p"]).success();
    assert_ne!(world(&s)["db.secret::app"]["attrs"]["cookie"], cookie);
    let rec = record(&s);
    assert!(rec["earlier"].is_null(), "{rec}");
    let retired = log(&s, "retired");
    assert_eq!(retired.len(), 1, "{retired:?}");
    assert_eq!(retired[0]["epoch"], 1);
    let r = run(&s, &[PASS], &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    // A second cycle: epoch 3 beside 2.
    run(&s, &[PASS], &["secrets", "cycle", "p"]).success();
    let rec = record(&s);
    assert_eq!(
        (rec["epoch"].clone(), rec["earlier"][0]["epoch"].clone()),
        (3.into(), 2.into())
    );
    let r = run(&s, &[PASS], &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// A key file's deployment keeps one master: cycling needs the
/// passphrase, which keeps an epoch sealed beside the next.
#[test]
fn a_key_file_is_not_cycled() {
    let s = Scratch::project("epochs-keyfile");
    s.write(
        "stacks/p.df",
        "use fake\nresource db.user u {\n  name = random.id(\"u\")\n}\n",
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_provider(db.user, \"fakecloud\")\n"),
    );
    run(&s, &[], &["apply", "p"]).success();
    let r = run(&s, &[], &["secrets", "cycle", "p"]).failure();
    assert!(
        r.stderr
            .contains("so cycling needs `[secrets] passphrase` or `recipients` in dform.toml"),
        "{}",
        r.stderr
    );
}
