//! A table from git (README "Tables"): the ref is resolved to a commit and
//! the plan file pins it, so `apply PLAN` applies what plan saw after the
//! branch moves; state keeps the commit last applied, and plan says when
//! the ref moved; the controller takes a moved ref as an input event.

mod common;
mod tables_common;
use common::Scratch;
use tables_common::{push, repo, scratch};

const PROGRAM: &str = r#"edition 2026

input relation node(name: string) from csv(git("ops.git", "main", "nodes.csv"))

resource compute.vm "{n}" {
  for node(n)
  size = 1
}
"#;

fn setup(name: &str) -> (Scratch, String) {
    let s = scratch(name);
    repo(&s, "ops.git");
    let first = push(&s, "nodes.csv", "name\na\n", "main");
    s.write("p.df", PROGRAM);
    (s, first)
}

fn short(c: &str) -> &str {
    &c[..7]
}

#[test]
fn a_plan_file_pins_the_commit_the_branch_named() {
    let (s, first) = setup("pin");
    s.run(&["apply", "p.df"]).success();
    let second = push(&s, "nodes.csv", "name\na\nb\n", "main");
    let r = s.run(&["plan", "--out", "plan.json", "p.df"]).success();
    // The ref moved since the last apply: plan says so, first.
    assert_eq!(
        r.stdout.lines().next().unwrap(),
        format!("node: ops.git main {} -> {}", short(&first), short(&second))
    );
    assert_eq!(r.summary(), "plan: 1 deformation (1 create)");
    let plan = s.read("plan.json");
    assert!(plan.contains(&second), "{plan}");
    assert!(
        plan.contains(&format!("ops.git@{}:nodes.csv:3", short(&second))),
        "{plan}"
    );

    // The branch moves again before apply: apply reads the plan's commit.
    let third = push(&s, "nodes.csv", "name\na\nb\nc\n", "main");
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.contains("+ compute.vm[\"b\"]"), "{}", r.stdout);
    assert!(!r.stdout.contains("compute.vm[\"c\"]"), "{}", r.stdout);
    assert!(r.stdout.contains("apply: complete"), "{}", r.stdout);

    // State has the commit apply read; the next plan reads the ref again.
    let r = s.run(&["plan", "p.df"]).success();
    assert_eq!(
        r.stdout.lines().next().unwrap(),
        format!("node: ops.git main {} -> {}", short(&second), short(&third))
    );
    assert!(r.stdout.contains("+ compute.vm[\"c\"]"), "{}", r.stdout);
    let r = s.run(&["why", r#"node("c")"#, "p.df"]).success();
    assert!(
        r.stdout
            .contains(&format!("fact, ops.git@{}:nodes.csv:4", short(&third))),
        "{}",
        r.stdout
    );
}

#[test]
fn a_ref_that_names_no_commit_is_an_error() {
    let (s, _) = setup("noref");
    s.write("p.df", &PROGRAM.replace("\"main\"", "\"env/prod\""));
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("git repository ops.git: ref env/prod does not name a commit"),
        "{}",
        r.stderr
    );
}

/// The controller's log, without its `HH:MM:SS ` stamps and the line
/// naming the files.
fn once(s: &Scratch) -> Vec<String> {
    let r = s.run(&["controller", "run", "--once", "p.df"]).success();
    r.stdout
        .lines()
        .map(|l| l[9..].to_string())
        .filter(|l| !l.starts_with("controller "))
        .collect()
}

#[test]
fn the_controller_takes_a_moved_ref_as_an_input_event() {
    let (s, _) = setup("controller");
    assert_eq!(
        once(&s),
        [
            "event start",
            "tick 1: plan: 1 deformation (1 create)",
            "stack p is undeformed",
        ]
    );
    assert_eq!(once(&s), ["event resync", "stack p is undeformed"]);
    let (first, second) = (
        tables_common::git(&s.path("work"), &["rev-parse", "HEAD"]),
        push(&s, "nodes.csv", "name\na\nb\n", "main"),
    );
    assert_eq!(
        once(&s),
        [
            "input node changed (git ops.git main:nodes.csv)",
            "event input node",
            &format!("node: ops.git main {} -> {}", short(&first), short(&second)),
            "tick 1: plan: 1 deformation (1 create)",
            "stack p is undeformed",
        ]
    );
}

/// A file table is watched too.
#[test]
fn the_controller_takes_a_changed_file_as_an_input_event() {
    let s = scratch("controller-file");
    s.write(
        "p.df",
        &PROGRAM.replace(r#"git("ops.git", "main", "nodes.csv")"#, r#""nodes.csv""#),
    );
    s.write("nodes.csv", "name\na\n");
    assert_eq!(once(&s)[0], "event start");
    s.write("nodes.csv", "name\na\nb\n");
    assert_eq!(
        once(&s)[..2],
        ["input node changed (file nodes.csv)", "event input node"]
    );
}
