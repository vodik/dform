//! The plan file (`plan --out`, `apply PLAN`): Terraform's stale-plan rule
//! stated for Z-sets. `apply PLAN` refreshes, re-evaluates and refuses
//! unless the delta it computes is the file's.

mod common;
use common::{Scratch, repo};

const WORLD: &str = r#"{"resources": {"compute.vm::app": {"typ": "compute.vm", "name": "app",
  "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}}"#;

/// Two ticks: the database is created in tick 1; the vm's update waits on
/// its endpoint and runs in tick 2.
const TWO_TICKS: &str = "edition 2026\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\nprovider fake\n";

fn two_ticks(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write_owned_world("w.json", WORLD);
    s.write("p.df", TWO_TICKS);
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    s
}

#[test]
fn the_file_records_inputs_delta_nulls_and_ticks() {
    let s = two_ticks("planfile-record");
    let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    assert_eq!(f["version"], 4);
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
        serde_json::json!({"null": "db.postgres[\"main\"].endpoint", "class": "open"})
    );
    assert_eq!(f["nulls"]["unresolved"][0], "db.postgres/main#endpoint");
    assert_eq!(f["ticks"][1]["addresses"][0], "compute.vm[\"app\"]");
}

/// The file's delta reproduced: apply runs it, with no other flags.
#[test]
fn apply_plan_applies_the_files_delta() {
    let s = two_ticks("planfile-ok");
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
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
            "compute.vm[\"app\"].db_host: the plan saw \"old.db.fake\", the world now has \"older.db.fake\""
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
            "dev",
            "--chaos",
            "mutate=db.postgres[\"main\"].size=2",
            "apply",
            "plan.json",
        ])
        .failure();
    assert!(
        r.stderr.contains("at tick 2 does not reproduce its delta"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "update db.postgres[\"main\"]: deformed again at tick 2; the plan file ran it in tick 1"
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
        r.stderr.contains("program p.df: changed since the plan"),
        "{}",
        r.stderr
    );
}

/// An input given now that the plan did not have is refused: here
/// `--data`.
#[test]
fn other_inputs_are_refused() {
    let s = Scratch::new("planfile-inputs");
    let prog = repo().join("examples/demo/stacks/dform.df");
    let prog = prog.to_str().unwrap();
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        prog,
    ])
    .success();
    let r = s
        .run(&["--data", "zone=us-test-9z", "apply", "plan.json"])
        .failure();
    assert!(
        r.stderr
            .contains("--data: the plan file has [], now [zone=us-test-9z]"),
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
        "edition 2026\nresource leaky.vault v { password = \"VAULT-SECRET-DO-NOT-PRINT\" }\nprovider fake\n",
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.run(&[
        "dev",
        "--provider",
        schema.to_str().unwrap(),
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "p.df",
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
        "dev",
        "--provider",
        "gke",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        prog.to_str().unwrap(),
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
    let net = "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nresource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\"), tier = \"web\" }\nprovider fake\n";
    s.write("p.df", net);
    s.run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"create_before_destroy\")\n",
            net.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    s.run(&common::on(
        "p.df",
        &["--world", "w.json"],
        &["plan", "--out", "plan.json"],
    ))
    .success();
    let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    assert_eq!(f["deformations"][0]["action"], "replace_create_first");
    assert_eq!(f["deformations"][0]["dependents"][0], "net.subnet[\"a\"]");
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// A policy named for the database's endpoint: a pending group, its
/// member named at tick 2.
const GROUP: &str = r#"edition 2026

resource db.postgres orders { size = 1 }

resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
provider fake
"#;

/// The file records the group's head, its rule and the stuck instance's
/// bound variables; tick 2's policy is admitted only when a firing of that
/// rule with those bindings derives it. A file whose group names another
/// database is stale at tick 2, its type notwithstanding.
#[test]
fn a_tick_2_address_outside_the_recorded_group_is_stale() {
    let s = Scratch::new("planfile-group");
    s.write("p.df", GROUP);
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    let mut f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    let g = &f["pending_groups"][0];
    assert_eq!(g["pattern"], "iam.policy[?]", "{f}");
    assert_eq!(g["head"], "want(\"iam.policy\", _)", "{f}");
    assert!(g["rule"].as_str().unwrap().starts_with('r'), "{f}");
    let bound: Vec<&serde_json::Value> = g["bindings"].as_object().unwrap().values().collect();
    assert_eq!(bound, [&serde_json::json!("orders")], "{f}");
    // The group as recorded: tick 2's policy is its member.
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);

    let s = Scratch::new("planfile-group-other");
    s.write("p.df", GROUP);
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    for v in f["pending_groups"][0]["bindings"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        *v = serde_json::json!("payments");
    }
    // Unsigned: the digest is what an approval checks, not this.
    f.as_object_mut().unwrap().remove("digest");
    let mut now: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    now["pending_groups"] = f["pending_groups"].clone();
    now.as_object_mut().unwrap().remove("digest");
    s.write("plan.json", &now.to_string());
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "create iam.policy[\"connect-orders.db.fake\"]: not in the plan file, and no pending \
             group it records derives it"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("\"payments\""), "{}", r.stderr);
    let world = s.read("w.json");
    assert!(world.contains("db.postgres"), "{world}");
    assert!(!world.contains("iam.policy"), "{world}");
}
