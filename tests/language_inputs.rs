//! Typed stack inputs: a default is a `@default` contribution, `--set` and
//! `--input-file` normal ones; types, required inputs and refinements are
//! checked.

mod common;
use common::Scratch;

const P: &str = r#"edition 2026
input env: enum("dev", "staging", "prod") = "staging"
input replicas: int = 2 where 1 <= replicas, replicas <= 5
input nets: list(inet) = []
input owner: string
resource net.vpc main {
  for env(e), replicas(r), owner(o)
  env = e
  replicas = r
  owner = o
}
resource net.subnet s {
  for nets(ns), member(ns, i, n), s = format("s%s", i)
  cidr = n
}
"#;

fn scratch() -> Scratch {
    let s = Scratch::project("lang-inputs");
    s.write("p.df", P);
    s
}

fn plan(s: &Scratch, extra: &[&str]) -> common::Run {
    let mut a = vec!["plan"];
    a.extend(extra);
    s.run(&common::on("p.df", &["--world", "w.json"], &a))
}

#[test]
fn a_default_yields_to_set() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops"]).success();
    assert!(r.stdout.contains("env = \"staging\""), "{}", r.stdout);
    assert!(r.stdout.contains("replicas = 2"), "{}", r.stdout);
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=prod"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
}

#[test]
fn a_required_input_with_no_value_is_an_error_naming_it() {
    let s = scratch();
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:5:1: input owner is required and has no value"),
        "{}",
        r.stderr
    );
}

#[test]
fn set_is_checked_against_the_declaration() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=qa"]).failure();
    assert!(
        r.stderr
            .contains("--set env=qa: input env is enum(dev, staging, prod)"),
        "{}",
        r.stderr
    );
    let r = plan(&s, &["--set", "owner=ops", "--set", "region=x"]).failure();
    assert!(
        r.stderr
            .contains("--set region: the program declares no input region"),
        "{}",
        r.stderr
    );
    // An int where a string is declared is its text.
    let r = plan(&s, &["--set", "owner=42"]).success();
    assert!(r.stdout.contains("owner = \"42\""), "{}", r.stdout);
}

/// `1 <= replicas <= 5` fits the checkable table: a refinement of the
/// input's cell, violated by the winning value (E DR-13).
#[test]
fn a_refinement_on_an_input_is_a_deny() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "replicas=9"]).failure();
    assert!(
        r.stderr.contains(
            "- refinement violated ctx={\"addr\":\"\",\"at\":\"p.df:3:1\",\"constraint\":\"range(1, 5)\",\"path\":\"replicas\",\"reason\":\"9 violates range(1, 5)\",\"type\":\"input\",\"value\":9,"
        ),
        "{}",
        r.stderr
    );
}

/// An input file holds one fact per input; its values are program terms,
/// so lists and `inet(...)` work, and a value of the wrong type is a
/// violation naming the input.
#[test]
fn an_input_file_gives_inputs_as_facts() {
    let s = scratch();
    s.write(
        "prod.df",
        "edition 2026\nenv(\"prod\")\nowner(\"ops\")\nnets([inet(\"10.0.0.0/24\"), inet(\"10.0.1.0/24\")])\n",
    );
    let r = plan(&s, &["--input-file", "prod.df"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("+ net.subnet.s1\n  cidr = \"10.0.1.0/24\""),
        "{}",
        r.stdout
    );

    s.write("bad.df", "edition 2026\nowner(\"ops\")\nnets([\"x\"])\n");
    let r = plan(&s, &["--input-file", "bad.df"]).failure();
    assert!(
        r.stderr.contains("input nets: [\"x\"] is not list(inet)"),
        "{}",
        r.stderr
    );

    s.write("stray.df", "edition 2026\nowner(\"ops\")\nregion(\"x\")\n");
    let r = plan(&s, &["--input-file", "stray.df"]).failure();
    assert!(
        r.stderr
            .contains("stray.df:3:1: region/1 is not an input of the program"),
        "{}",
        r.stderr
    );
}

/// A value a rule computes is checked after evaluation.
#[test]
fn a_module_input_of_the_wrong_type_is_a_violation() {
    let s = Scratch::project("lang-inputs-module");
    s.write(
        "p.df",
        "edition 2026\nmodule m {\n  input n: int\n  resource net.vpc v {\n    for n(n_)\n    n = n_\n  }\n}\ninstance m a { n = \"three\" }\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .failure();
    assert!(
        r.stderr.contains("input n of m.a: three is not int"),
        "{}",
        r.stderr
    );
}

/// The plan file records each input file's digest: `apply PLAN` reads the
/// same file, and refuses when it changed.
#[test]
fn the_plan_file_records_input_files() {
    let s = scratch();
    s.write("prod.df", "edition 2026\nenv(\"prod\")\nowner(\"ops\")\n");
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "--input-file",
        "prod.df",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    assert!(s.read("plan.json").contains("\"input_files\""));
    s.run(&["apply", "plan.json"]).success();
    s.write("prod.df", "edition 2026\nenv(\"dev\")\nowner(\"ops\")\n");
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("--input-file prod.df: changed since the plan"),
        "{}",
        r.stderr
    );
}

/// An input file may hold a secret: the plan file records its digest keyed
/// with the stack's plan key (HMAC-SHA256, as a sensitive leaf), never an
/// unkeyed hash of its bytes that could be brute-forced.
#[test]
fn the_plan_file_digests_input_files_with_the_stack_key() {
    let text = "edition 2026\nenv(\"prod\")\nowner(\"hunter2\")\n";
    let fnv = {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    };
    let digest = |name: &str| -> String {
        let s = Scratch::project(name);
        s.write("p.df", P);
        s.write("prod.df", text);
        let args = [
            "--input-file",
            "prod.df",
            "plan",
            "--out",
            "plan.json",
            "p.df",
        ];
        s.run(&args).success();
        let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
        let files = f["inputs"]["input_files"].as_array().unwrap().clone();
        assert_eq!(files.len(), 1, "{f}");
        assert_eq!(files[0]["path"], "prod.df");
        let d = files[0]["digest"].as_str().unwrap_or_default().to_string();
        assert!(!s.read("plan.json").contains(&fnv), "{f}");
        s.run(&args).success();
        let again: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
        assert_eq!(
            again["inputs"]["input_files"][0]["digest"],
            d.as_str(),
            "same key, same digest"
        );
        d
    };
    let (a, b) = (digest("lang-inputs-key-a"), digest("lang-inputs-key-b"));
    assert_eq!(a.len(), 64, "{a}");
    assert_ne!(a, b, "another stack key, another digest");
}
