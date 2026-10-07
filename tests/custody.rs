//! Who holds a deployment's master (R-163, R-164): the master never
//! changes without someone deciding it. A deployment with state whose key
//! file is gone refuses, and so does a `RANDOM_MASTER` other than the
//! master state was applied with; `--new-master` takes the new one, every
//! derived secret changing, and the audit log says so.

mod common;
use common::{Run, Scratch, dform, repo, yes};

/// `dform ARGS` in `s` with the environment `env` (`RANDOM_MASTER` unset
/// unless given).
fn run(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env_remove("RANDOM_MASTER");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// examples/crud-api, applied: its database password is
/// `random.password("crud-api-db")`.
fn applied(name: &str) -> Scratch {
    let s = Scratch::new(name);
    common::copy_dir(&repo().join("examples/crud-api"), &s.dir);
    let _ = std::fs::remove_dir_all(s.path("dform.state"));
    run(&s, &[], &["apply"]).success();
    s
}

/// The entries of kind `kind` in the deployment's audit log.
fn entries(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("dform.state/crud_api/state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == kind)
        .collect()
}

fn password(s: &Scratch) -> String {
    let w: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/crud_api/remote.json")).unwrap();
    w["resources"]["google.sql_user::crud_user"]["attrs"]["password"]
        .as_str()
        .unwrap_or_else(|| panic!("{w}"))
        .to_string()
}

/// A key file lost (a restore that missed one object) is never made again
/// silently: the plan refuses, naming the file and what to do; with
/// `--new-master` it plans every derived secret as changed, and the apply
/// records the new master in the audit log.
#[test]
fn a_missing_key_file_is_refused_not_made_again() {
    let s = applied("custody-missing-key");
    let state: serde_json::Value = s.json("dform.state/crud_api/state.json");
    let id = state["master"]
        .as_str()
        .unwrap_or_else(|| panic!("{state}"));
    let pw = password(&s);
    std::fs::remove_file(s.path("dform.state/crud_api/state.key")).unwrap();

    let r = run(&s, &[], &["plan"]).failure();
    assert!(
        r.stderr
            .contains("state.key is missing, and the deployment was applied with it")
            && r.stderr.contains("--new-master"),
        "{}",
        r.stderr
    );
    assert!(
        !s.path("dform.state/crud_api/state.key").exists(),
        "a refused plan made a key"
    );
    run(&s, &[], &["apply"]).failure();

    let r = run(&s, &[], &["plan", "--new-master"]).success();
    assert!(
        r.stderr.contains("new master (--new-master)"),
        "{}",
        r.stderr
    );
    assert!(
        r.summary().contains("update"),
        "every derived secret changes:\n{}",
        r.stdout
    );
    run(&s, &[], &["apply", "--new-master"]).success();
    assert_ne!(password(&s), pw, "a new master is a new password");
    let m = entries(&s, "master");
    let last = m.last().unwrap_or_else(|| panic!("no master entry"));
    assert_eq!(last["from"], id, "{last}");
    assert_ne!(last["to"], id, "{last}");
    // Then the new master is the deployment's: a plain plan takes it.
    let r = run(&s, &[], &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
}

/// A `RANDOM_MASTER` other than the master state was applied with is
/// refused, naming where this run's came from; `--new-master` takes it.
#[test]
fn another_random_master_is_refused() {
    let s = applied("custody-random-master");
    let pw = password(&s);
    let other = [("RANDOM_MASTER", "another-master")];
    let r = run(&s, &other, &["plan"]).failure();
    assert!(
        r.stderr.contains("RANDOM_MASTER") && r.stderr.contains("is not the one its state"),
        "{}",
        r.stderr
    );
    let r = run(&s, &other, &["plan", "--new-master"]).success();
    assert!(r.summary().contains("update"), "{}", r.stdout);
    run(&s, &other, &["apply", "--new-master"]).success();
    assert_ne!(password(&s), pw);
    let m = entries(&s, "master");
    assert_eq!(m.last().unwrap()["source"], "RANDOM_MASTER", "{m:?}");
    // Back to the key file: the master state was applied with is now
    // RANDOM_MASTER's, so the key file's is refused in turn.
    let r = run(&s, &[], &["plan"]).failure();
    assert!(r.stderr.contains("the key file"), "{}", r.stderr);
    // The same RANDOM_MASTER is taken as it is.
    let r = run(&s, &other, &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
}

const PASS: (&str, &str) = ("DFORM_TEST_PASSPHRASE", "correct horse battery staple");

/// examples/crud-api whose dform.toml keeps each master sealed under the
/// passphrase in `DFORM_TEST_PASSPHRASE`.
fn sealed(name: &str) -> Scratch {
    let s = Scratch::new(name);
    common::copy_dir(&repo().join("examples/crud-api"), &s.dir);
    let _ = std::fs::remove_dir_all(s.path("dform.state"));
    passphrase(&s);
    s
}

fn passphrase(s: &Scratch) {
    let toml = s.read("dform.toml") + "\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n";
    s.write("dform.toml", &toml);
}

/// With `[secrets] passphrase`, the backend holds the master sealed
/// (`state.master`: its id, the salt and the sealed bytes), never a key
/// file; the passphrase opens it, another is refused by name.
#[test]
fn a_passphrase_seals_the_master() {
    let s = sealed("custody-passphrase");
    run(&s, &[PASS], &["apply"]).success();
    assert!(!s.path("dform.state/crud_api/state.key").exists());
    let record = s.json("dform.state/crud_api/state.master");
    let state = s.json("dform.state/crud_api/state.json");
    assert_eq!(record["id"], state["master"], "{record}");
    let sealed = &record["passphrase"];
    assert_eq!(sealed["kdf"], "scrypt", "{record}");
    assert!(
        sealed["salt"].as_str().is_some_and(|s| s.len() == 32)
            && sealed["sealed"].as_str().is_some(),
        "{record}"
    );
    let r = run(&s, &[PASS], &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
    let wrong = [("DFORM_TEST_PASSPHRASE", "Tr0ub4dor&3")];
    let r = run(&s, &wrong, &["plan"]).failure();
    assert!(
        r.stderr
            .contains("the passphrase from env:DFORM_TEST_PASSPHRASE does not open"),
        "{}",
        r.stderr
    );
}

/// A deployment whose master is a key file keeps it until the first apply
/// that has the passphrase: that seals it (the same master, so no derived
/// secret changes), removes the file, and says so in the audit log.
#[test]
fn the_first_apply_with_the_passphrase_seals_the_key_file() {
    let s = applied("custody-migrate");
    let pw = password(&s);
    let id = s.json("dform.state/crud_api/state.json")["master"].clone();
    passphrase(&s);
    // A plan reads the key file as it is.
    let r = run(&s, &[PASS], &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
    assert!(s.path("dform.state/crud_api/state.key").exists());
    run(&s, &[PASS], &["apply"]).success();
    assert!(!s.path("dform.state/crud_api/state.key").exists());
    let record = s.json("dform.state/crud_api/state.master");
    assert_eq!(record["id"], id, "the same master: {record}");
    assert_eq!(password(&s), pw);
    let c = entries(&s, "custody");
    assert!(
        c.len() == 1 && c[0]["sealed"] == "state.key" && c[0]["id"] == id,
        "{c:?}"
    );
    let r = run(&s, &[PASS], &["plan"]).success();
    assert_eq!(r.summary(), "stack crud_api is up to date", "{}", r.stdout);
}
