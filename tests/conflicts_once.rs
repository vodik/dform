//! A conflict is said once (R-111): in the plan's `conflicts` section,
//! with its witnesses by where they were written, and not again on
//! stderr; a run that refuses before it plans (`apply`) says it the same
//! way, never as the deny's raw context. The shape of the private
//! project's plan: two ticks, a `later` group, and the conflict inside a
//! component copied twice.

mod common;
use common::{Scratch, repo};

/// The two-phase GKE stack (a second tick, a `later` group) with a
/// component whose two copies each conflict; and the line of the
/// component's resource, the `set` the next.
fn project() -> (Scratch, usize) {
    let s = Scratch::new("conflicts-once");
    let stack = repo().join("examples/gke/stacks/gke_two_phase.df");
    let mut text = std::fs::read_to_string(stack).unwrap();
    let at = text.lines().count() + 4;
    text.push_str(
        r#"
component pool {
  on(1)
  resource google.compute_address a { project = "p", region = "r", name = "a" }
  set a.name = "b" where on(1)
}
resource pool one {}
resource pool two {}
"#,
    );
    s.write("p.df", &text);
    (s, at)
}

fn dform(s: &Scratch, cmd: &[&str]) -> common::Run {
    let mut args = vec![
        "dev",
        "--provider",
        "gke",
        "--provider",
        "k8s",
        "--world",
        "w.json",
    ];
    args.extend_from_slice(cmd);
    args.push("p.df");
    s.run(&args).failure()
}

#[test]
fn a_refused_plan_says_each_conflict_once() {
    let (s, at) = project();
    let r = dform(&s, &["plan"]);
    assert!(r.stdout.contains("\ntick 2 "), "{}", r.stdout);
    assert!(r.stdout.contains("\npolicy  "), "{}", r.stdout);
    let all = format!("{}{}", r.stdout, r.stderr);
    for want in [
        "conflicts",
        "  ! google.compute_address one.a.name: two contributions disagree",
        "  ! google.compute_address two.a.name: two contributions disagree",
        "apply: refused  2 conflicts",
    ] {
        let n = all.lines().filter(|l| *l == want).count();
        assert_eq!(n, 1, "{want}\n---\n{all}");
    }
    // Each witness by its value and where it was written.
    let witnesses: Vec<&str> = r
        .stdout
        .lines()
        .filter(|l| l.starts_with("      \""))
        .collect();
    let (a, b) = (
        format!("      \"a\"  p.df:{at}"),
        format!("      \"b\"  p.df:{}", at + 1),
    );
    assert_eq!(witnesses, [&a, &b, &a, &b], "{}", r.stdout);
    assert!(!all.contains("ctx="), "{all}");
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

#[test]
fn a_refused_apply_says_each_conflict_as_the_plan_does() {
    let (s, at) = project();
    let r = dform(&s, &["apply", "--yes"]);
    let want = "  ! google.compute_address one.a.name: two contributions disagree";
    let n = r.stderr.lines().filter(|l| *l == want).count();
    assert_eq!(n, 1, "{}", r.stderr);
    let b = format!("\n      \"b\"  p.df:{}\n", at + 1);
    assert!(r.stderr.contains(&b), "{}", r.stderr);
    assert!(!r.stderr.contains("ctx="), "{}", r.stderr);
}
