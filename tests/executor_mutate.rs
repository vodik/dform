//! The world mutates between phases: chaos `mutate=` lands after tick 1 and
//! tick 2's refresh sees it. A change under an address with a pending
//! deformation stops the run before tick 2 with the change printed (a deny
//! over `deformation/4`); a change anywhere else is drift, reported, and
//! the run goes on.

mod common;
use common::{Scratch, mock};

/// Tick 1 creates the database; the vm's update waits on its endpoint.
fn stack(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write_owned_world(
        "w.json",
        r#"{"resources": {"compute.vm::app": {"typ": "compute.vm", "name": "app",
            "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}}"#,
    );
    s.write(
        "p.df",
        "\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\nuse fake\n",
    );
    s
}

#[test]
fn a_mutation_under_a_pending_deformation_stops_before_tick_two() {
    let s = stack("mutate-pending");
    let r = mock(
        &s,
        &["apply", "--chaos", "mutate=compute.vm[\"app\"].size=2"],
    )
    .failure();
    assert!(
        r.stdout
            .contains("chaos: mutate compute.vm[\"app\"].size = 2 after tick 0"),
        "{}",
        r.stdout
    );
    // One plan printed: no later tick was planned.
    assert_eq!(r.stdout.matches("plan: ").count(), 1, "{}", r.stdout);
    assert!(
        r.stderr.contains(
            "the world changed under a pending change after tick 1:\n\
             ~ compute.vm app\n  size: <none> -> 2\n"
        ),
        "{}",
        r.stderr
    );
    // The stop is a deny the evaluator derives from the held deformation.
    assert!(
        r.stderr.contains(
            "constraint violations after tick 1:\n\
             - the world changed under a pending change: compute.vm[\"app\"]\n"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("; stopped after tick 1; ticks 1 to 1 were applied"),
        "{}",
        r.stderr
    );
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    assert_eq!(
        w["resources"]["compute.vm::app"]["attrs"]["db_host"],
        "old.db.fake"
    );
}

#[test]
fn a_mutation_elsewhere_is_drift_and_the_run_continues() {
    let s = stack("mutate-elsewhere");
    let r = mock(
        &s,
        &["apply", "--chaos", "mutate=db.postgres[\"main\"].size=9"],
    )
    .success();
    assert!(
        r.stdout.contains(
            "drift after tick 1:\n~ db.postgres main\n  size: 1 -> 9\n\
             plan: 2 changes (2 update) over 1 tick\n\ntick 2  2 changes\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  ~ db.postgres main  p.df:2\n      size: 9 → 1\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
    // The mutation lands once per run: the stack is now undeformed.
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}
