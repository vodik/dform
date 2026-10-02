//! `dform dev effects`: what each scope reads, writes and offers
//! (DESIGN.org R-11c), text and `--json`.

mod common;
mod inspection_common;
use inspection_common::{dform, golden};

/// The play example: `baseline` writes `(*, tags)`, `(iam.policy,
/// statements)` and `settings.audit.sinks`; its reads and the other
/// scopes' are pinned as a golden.
#[test]
fn the_demo_pack_writes_the_grants_it_declares() {
    let out = dform("examples/demo/stacks/dform.df", &["effects"]);
    let baseline = out
        .split("\n\n")
        .find(|s| s.starts_with("baseline:"))
        .unwrap_or(&out);
    assert!(baseline.contains("(*, tags)"), "{out}");
    assert!(baseline.contains("(iam.policy, statements)"), "{out}");
    assert!(baseline.contains("settings.audit.sinks"), "{out}");
    // Deterministic: a second run prints the same thing.
    assert_eq!(out, dform("examples/demo/stacks/dform.df", &["effects"]));
    golden("effects_demo", &out);
}

/// A module instance's own writes are its resources' attribute cells; a
/// variable-typed write (no evaluation, so it cannot be known) prints
/// `*` for the type or the path rather than guess.
#[test]
fn a_module_instance_writes_its_resource_cells() {
    let out = dform("examples/demo/stacks/dform.df", &["effects"]);
    assert!(
        out.contains("(net.vpc, cidr)") && out.contains("(net.vpc, tags)"),
        "{out}"
    );
    assert!(
        out.contains("vpc: addr") && out.contains("private_subnet: relation/1"),
        "{out}"
    );
}

/// Cross-instance wiring (`use database { backup_days = cfg.db.backup_days
/// }`, then `database.iam_need`) is written at the call site (the stack
/// writes the module's input cell) and read there too (the stack reads
/// the module's output).
#[test]
fn cross_instance_wiring_is_the_callers_effect() {
    let out = dform("examples/demo/stacks/dform.df", &["effects"]);
    assert!(
        out.contains("input database.backup_days"),
        "stack should write database's input cell: {out}"
    );
    assert!(
        out.contains("output database.iam_need"),
        "stack should read database's output: {out}"
    );
}

/// `--json` is the same information, one document, parseable.
#[test]
fn json_is_the_same_information() {
    let out = dform("examples/demo/stacks/dform.df", &["effects", "--json"]);
    let doc: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let baseline = &doc["baseline"];
    let writes: Vec<&str> = baseline["writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(writes.contains(&"(iam.policy, statements)"), "{out}");
}
