//! The plan file (`plan --out`, `apply PLAN`): Terraform's stale-plan rule
//! stated for Z-sets. `apply PLAN` refreshes, re-evaluates and refuses
//! unless the delta it computes is the file's.

mod common;
use common::{Scratch, repo};

const WORLD: &str = r#"{"resources": {"compute.vm::app": {"typ": "compute.vm", "name": "app",
  "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}}"#;

/// Two ticks: the database is created in tick 1; the vm's update waits on
/// its endpoint and runs in tick 2.
const TWO_TICKS: &str = "edition 2026\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\n";

fn two_ticks(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write_owned_world("w.json", WORLD);
    s.write("p.df", TWO_TICKS);
    s.run(&[
        "--file",
        "p.df",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
    ])
    .success();
    s
}

#[test]
fn the_file_records_inputs_delta_nulls_and_ticks() {
    let s = two_ticks("planfile-record");
    let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    assert_eq!(f["version"], 3);
    assert_eq!(f["inputs"]["files"][0]["path"], "p.df");
    assert_eq!(f["inputs"]["world"], "w.json");
    assert!(f["world_digest"].is_string());
    let d = f["deformations"].as_array().unwrap();
    assert_eq!(d.len(), 2, "{f}");
    let vm = d.iter().find(|e| e["name"] == "app").unwrap();
    assert_eq!(vm["tick"], 2);
    assert_eq!(vm["on"][0], "db.postgres/main#endpoint");
    assert_eq!(
        vm["changes"][0]["after"],
        serde_json::json!({"null": "db.postgres/main#endpoint", "class": "open"})
    );
    assert_eq!(f["nulls"]["unresolved"][0], "db.postgres/main#endpoint");
    assert_eq!(f["ticks"][1]["addresses"][0], "compute.vm.app");
}

/// The file's delta reproduced: apply runs it, with no other flags.
#[test]
fn apply_plan_applies_the_files_delta() {
    let s = two_ticks("planfile-ok");
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let r = s
        .run(&["--file", "p.df", "--world", "w.json", "plan"])
        .success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

/// The world moved after the plan: the saved before-state is stale, and
/// nothing is applied.
#[test]
fn drift_after_the_plan_is_refused() {
    let s = two_ticks("planfile-drift");
    s.write("w.json", &WORLD.replace("old.db.fake", "older.db.fake"));
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "compute.vm.app db_host: the plan saw \"old.db.fake\", the world now has \"older.db.fake\""
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("stale plan"), "{}", r.stderr);
    assert!(!s.read("w.json").contains("db.postgres"), "applied anyway");
}

/// The chaos mock changes the database after tick 1: tick 2's refresh sees
/// a deformation the file does not have, and apply stops there.
#[test]
fn a_world_mutated_between_ticks_is_refused_at_the_boundary() {
    let s = two_ticks("planfile-mutate");
    let r = s
        .run(&[
            "apply",
            "plan.json",
            "--chaos",
            "mutate=db.postgres/main:size=2",
        ])
        .failure();
    assert!(
        r.stderr.contains("at tick 2 does not reproduce its delta"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "update db.postgres.main: deformed again at tick 2; the plan file ran it in tick 1"
        ),
        "{}",
        r.stderr
    );
    // Tick 2 never ran: the vm still has the old host.
    assert!(s.read("w.json").contains("old.db.fake"));
}

#[test]
fn a_changed_program_is_refused() {
    let s = two_ticks("planfile-program");
    s.write("p.df", &TWO_TICKS.replace("size = 1", "size = 3"));
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains("--file p.df: changed since the plan"),
        "{}",
        r.stderr
    );
}

/// A new deformation not in the file is refused too: here a resource the
/// plan did not have, because --set chose another environment.
#[test]
fn other_inputs_are_refused() {
    let s = Scratch::new("planfile-inputs");
    let prog = repo().join("examples/demo/stacks/dform.df");
    let prog = prog.to_str().unwrap();
    s.run(&[
        "--file",
        prog,
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
    ])
    .success();
    let r = s
        .run(&["--set", "env=prod", "apply", "plan.json"])
        .failure();
    assert!(
        r.stderr
            .contains("--set: the plan file has [], now [env=prod]"),
        "{}",
        r.stderr
    );
}

/// The plan file is something dform persists: a labeled secret is stored
/// redacted, like the plan prints it.
#[test]
fn the_file_never_carries_a_labeled_secret() {
    let s = Scratch::new("planfile-secret");
    s.write(
        "p.df",
        "edition 2026\nresource leaky.vault v { password = \"VAULT-SECRET-DO-NOT-PRINT\" }\n",
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.run(&[
        "--file",
        "p.df",
        "--provider",
        schema.to_str().unwrap(),
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
    ])
    .success();
    let f = s.read("plan.json");
    assert!(!f.contains("VAULT-SECRET"), "{f}");
    assert!(f.contains(r#""sensitive": null"#), "{f}");
    s.run(&["apply", "plan.json"]).success();
}

/// Across a boundary: tick 2's nodepools come from a pending group the file
/// records, and the kubernetes objects are the file's held deformations.
#[test]
fn a_two_phase_plan_file_applies_across_the_boundary() {
    let s = Scratch::new("planfile-gke");
    let prog = repo().join("examples/gke/stacks/gke_two_phase.df");
    s.run(&[
        "--file",
        prog.to_str().unwrap(),
        "--provider",
        "gke",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
    ])
    .success();
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.contains("tick 2:\n"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// create_before_destroy: the file records the replace's dependents, so
/// their update to the new identity at tick 2, and the deposed object's
/// delete, are the file's delta, not drift.
#[test]
fn a_create_before_destroy_plan_file_applies_in_two_ticks() {
    let s = Scratch::new("planfile-cbd");
    let net = "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nresource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\"), tier = \"web\" }\n";
    s.write("p.df", net);
    let args = ["--file", "p.df", "--world", "w.json"];
    s.run(&[&args[..], &["apply"]].concat()).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(net.vpc, \"main\", \"create_before_destroy\")\n",
            net.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    s.run(&[&args[..], &["plan", "--out", "plan.json"]].concat())
        .success();
    let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    assert_eq!(f["deformations"][0]["action"], "replace_create_first");
    assert_eq!(f["deformations"][0]["dependents"][0], "net.subnet.a");
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}
