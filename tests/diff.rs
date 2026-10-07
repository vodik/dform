//! `dform diff --since REF` (R-15): the applies since REF, each
//! deformation with why it was planned, by the program as it was at that
//! apply (read at the commit the audit log recorded), and which inputs and
//! stated rows changed since the apply before.

mod common;
use common::{Scratch, repo};
use serde_json::Value;

const NET: &str = r#"

input zone from csv.decode(io.read("data/zones.csv"))

decl zone(name: string, n: int)

use fake

resource net.vpc main { cidr = "10.0.0.0/16" }

resource net.subnet "private-${z}" {
  vpc = main
  cidr = inet.subnet(main.cidr, 8, n)
  zone = z
} where zone(z, n)
"#;

/// `git ARGS` in the scratch project; its output. Hermetic: no global or
/// system configuration, and a fixed date, so a commit's hash is the same
/// on every run.
fn git(s: &Scratch, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=dform",
            "-c",
            "user.email=dform@example.com",
        ])
        .args(args)
        .current_dir(&s.dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The seq of each `apply_start` in the log.
fn applies(s: &Scratch) -> Vec<u64> {
    let r = s.run(&["log", "--json", "net"]).success();
    let es: Vec<Value> = serde_json::from_str(&r.stdout).unwrap();
    es.iter()
        .filter(|e| e["kind"] == "apply_start")
        .map(|e| e["seq"].as_u64().unwrap())
        .collect()
}

/// A project in git, applied twice: the second commit adds a row to the
/// table and moves the VPC's range. `diff` names the row, explains each
/// apply by the program at its own commit, and says which row changed.
#[test]
fn diff_names_the_row_an_apply_added_and_explains_each_apply_by_its_commit() {
    let s = Scratch::project("diff-git");
    s.write(".gitignore", "dform.state/\n");
    s.write("data/zones.csv", "name,n\nus-test-1a,1\nus-test-1b,2\n");
    s.write("stacks/net.df", NET);
    git(&s, &["init", "-q", "."]);
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-qm", "one"]);
    s.run(&["apply", "net"]).success();
    s.write(
        "data/zones.csv",
        "name,n\nus-test-1a,1\nus-test-1b,2\nus-test-1c,3\n",
    );
    s.write("stacks/net.df", &NET.replace("10.0.0.0/16", "10.9.0.0/16"));
    git(&s, &["commit", "-qam", "two"]);
    let second = git(&s, &["rev-parse", "HEAD"]);
    s.run(&["apply", "net"]).success();
    let starts = applies(&s);
    assert_eq!(starts.len(), 2);

    let r = s
        .run(&["diff", "--since", &starts[1].to_string(), "-vv", "net"])
        .success();
    let out = &r.stdout;
    assert!(out.starts_with(&format!("apply {} ", starts[1])), "{out}");
    assert!(
        out.contains(&format!(" at {}: ok\n", &second[..12])),
        "{out}"
    );
    assert!(
        out.contains(
            "+ net.subnet private-us-test-1c\n  by stacks/net.df:11  resource net.subnet \
             \"private-${z}\" { .. } where zone(z, n)\n  because stacks/net.df:9  \
             net.vpc main.cidr = \"10.9.0.0/16\"\n  because data/zones.csv:4  \
             zone(\"us-test-1c\", 3)\n"
        ),
        "{out}"
    );
    // A replace is explained by the attribute that changed.
    assert!(
        out.contains(
            "-/+ net.vpc main\n  because stacks/net.df:9  net.vpc main.cidr = \
             \"10.9.0.0/16\"\n"
        ),
        "{out}"
    );
    // By default each change says the first line of why; `-q` the
    // changes alone, their addresses as the plan file has them (R-111).
    let since = starts[1].to_string();
    let line = s.run(&["diff", "--since", &since, "net"]).success();
    assert!(
        line.stdout.contains(
            "+ net.subnet private-us-test-1c\n  by stacks/net.df:11  resource net.subnet \
             \"private-${z}\" { .. } where zone(z, n)\n"
        ) && !line.stdout.contains("because data/zones.csv:4"),
        "{}",
        line.stdout
    );
    let quiet = s.run(&["diff", "--since", &since, "-q", "net"]).success();
    assert!(
        quiet.stdout.contains(
            ": ok\n-/+ net.vpc[\"main\"]\n-/+ net.subnet[\"private-us-test-1a\"]\n-/+ \
             net.subnet[\"private-us-test-1b\"]\n+ net.subnet[\"private-us-test-1c\"]\nchanged"
        ),
        "{}",
        quiet.stdout
    );
    // What changed since the apply before: the row.
    let (_, changed) = out
        .split_once(&format!("changed since apply {} ", starts[0]))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        changed.ends_with(":\n  + data/zones.csv:4  zone(\"us-test-1c\", 3)\n"),
        "{out}"
    );

    // The same window by the commit the log recorded, and as JSON. The
    // whole hash: a prefix of only decimal digits (one 8-digit prefix in 43)
    // prefixes) is read as a sequence number, which is how this test
    // flaked while commit dates, and so hashes, varied.
    let r = s
        .run(&["diff", "--since", &second, "--json", "net"])
        .success();
    let j: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["deployment"], "net");
    assert_eq!(j["applies"].as_array().unwrap().len(), 1);
    let create = j["applies"][0]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["address"] == "net.subnet[\"private-us-test-1c\"]")
        .unwrap();
    assert_eq!(create["action"], "create");
    assert!(
        create["why"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["at"] == "data/zones.csv:4" && b["text"] == "zone(\"us-test-1c\", 3)"),
        "{create}"
    );
    assert_eq!(
        j["changed"]["added"][0],
        serde_json::json!({"at": "data/zones.csv:4", "text": "zone(\"us-test-1c\", 3)"})
    );
    assert_eq!(j["changed"]["since"]["seq"], starts[0]);

    // The first apply, explained by the program at its own commit: the
    // range it had then, not the one it has now.
    let r = s.run(&["diff", "--since", "1", "-vv", "net"]).success();
    let first = r.stdout.split("\napply ").next().unwrap();
    assert!(
        first.contains(
            "+ net.subnet private-us-test-1a\n  by stacks/net.df:11  resource net.subnet \
             \"private-${z}\" { .. } where zone(z, n)\n  because stacks/net.df:9  \
             net.vpc main.cidr = \"10.0.0.0/16\"\n  because data/zones.csv:2  \
             zone(\"us-test-1a\", 1)\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.ends_with("no apply before 1 to compare with\n"),
        "{}",
        r.stdout
    );
}

/// Outside a repository the program now explains every apply, and the
/// diff says so where the program's digest moved since; a secret input
/// prints as its label.
#[test]
fn diff_outside_a_repository_explains_by_the_program_now_and_redacts_secrets() {
    let s = Scratch::new("diff-world");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let mock = ["--provider", schema.to_str().unwrap(), "--world", "w.json"];
    let run = |pw: &str, args: &[&str]| {
        let mut all = vec!["--set".to_string(), format!("pw={pw}")];
        all.extend(common::on("p.df", &mock, args));
        s.run(&all)
    };
    let prog = "\ninput pw: secret(string)\nresource leaky.vault v {\n  password = pw\n}\n";
    s.write("p.df", prog);
    run("FIRST-SECRET-123", &["apply"]).success();
    s.write(
        "p.df",
        &format!("{prog}resource leaky.oops o {{\n  password = \"plain\"\n}}\n"),
    );
    run("SECOND-SECRET-456", &["apply"]).success();
    let r = run("SECOND-SECRET-456", &["diff", "--since", "1", "-vv"]).success();
    assert!(
        r.stdout.contains(
            "  note: not in a repository, and the program changed since this apply: explained by \
             the program now\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  by p.df:4  resource leaky.vault v { password = pw }\n  because --set pw=(sensitive input.pw)\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("+ leaky.oops o\n"), "{}", r.stdout);
    let j = run("SECOND-SECRET-456", &["diff", "--since", "1", "--json"]).success();
    for out in [&r.stdout, &r.stderr, &j.stdout, &j.stderr] {
        assert!(!out.contains("SECRET-"), "{out}");
    }
    let bad = run("SECOND-SECRET-456", &["diff", "--since", "yesterday"]).failure();
    assert!(
        bad.stderr
            .contains("diff --since yesterday: expected a sequence number"),
        "{}",
        bad.stderr
    );
}

/// Outside a repository the plan entry records each document the run read
/// with its digest: a CSV changed since an apply is named, though the
/// program files are the same.
#[test]
fn diff_outside_a_repository_names_a_changed_document() {
    let s = Scratch::project("diff-doc");
    s.write("data/zones.csv", "name,n\nus-test-1a,1\n");
    s.write("stacks/net.df", NET);
    s.run(&["apply", "net"]).success();
    let r = s.run(&["log", "--json", "net"]).success();
    let es: Vec<Value> = serde_json::from_str(&r.stdout).unwrap();
    let plan = es.iter().find(|e| e["kind"] == "plan").unwrap();
    assert_eq!(
        plan["documents"][0]["path"]
            .as_str()
            .map(|p| p.ends_with("data/zones.csv")),
        Some(true),
        "{plan}"
    );
    s.write("data/zones.csv", "name,n\nus-test-1a,1\nus-test-1b,2\n");
    s.run(&["apply", "net"]).success();
    let r = s.run(&["diff", "--since", "1", "net"]).success();
    let first = r.stdout.split("\napply ").next().unwrap();
    assert!(
        first.contains("  note: not in a repository, and ")
            && first.contains(
                "data/zones.csv changed since this apply: explained by the documents now\n"
            ),
        "{}",
        r.stdout
    );
    // The second apply read the CSV as it is now: no note.
    let second = r.stdout.split("\napply ").nth(1).unwrap();
    assert!(!second.contains("note:"), "{}", r.stdout);
}

/// An apply from a dirty tree records the tracked files it had modified,
/// and `diff` says so directly.
#[test]
fn a_dirty_apply_is_recorded_and_named() {
    let s = Scratch::project("diff-dirty");
    s.write(".gitignore", "dform.state/\n");
    s.write("data/zones.csv", "name,n\nus-test-1a,1\n");
    s.write("stacks/net.df", NET);
    git(&s, &["init", "-q", "."]);
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-qm", "one"]);
    s.write("stacks/net.df", &NET.replace("10.0.0.0/16", "10.9.0.0/16"));
    s.run(&["apply", "net"]).success();
    let r = s.run(&["log", "--json", "net"]).success();
    let es: Vec<Value> = serde_json::from_str(&r.stdout).unwrap();
    let start = es.iter().find(|e| e["kind"] == "apply_start").unwrap();
    assert_eq!(start["dirty"], true, "{start}");
    assert_eq!(
        start["modified"],
        serde_json::json!(["stacks/net.df"]),
        "{start}"
    );
    // The program changes again: the apply is explained at its commit, and
    // the note names the dirty file.
    s.write("stacks/net.df", NET);
    let r = s.run(&["diff", "--since", "1", "net"]).success();
    assert!(
        r.stdout.contains(
            "  note: the tree was dirty at this apply (stacks/net.df modified): explained as \
             committed at "
        ),
        "{}",
        r.stdout
    );
}
