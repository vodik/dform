//! Refinement types (E DR-13, F DR-13 revised): a `where` over the
//! attribute's own value that fits the checkable table is a rank-blind
//! constraint in the attribute's cell; checked at compile time against a
//! literal, at collapse against the winning value, deferred while that
//! value carries a null, and deferred to the provider on a sensitive path.
//! Everything else lowers to a deny.

mod common;
use common::{Run, Scratch, repo};
use std::process::Command;

fn plan(s: &Scratch, src: &str) -> Run {
    s.write("p.df", src);
    s.run(&["--file", "p.df", "--world", "w.json", "plan"])
}

const SETTINGS: &str = "edition 2026.
type settings {
  db.backup_days: int where 1 <= db.backup_days <= 35
}.
settings prod @default { db = { backup_days: 3 } }.
settings prod { db = { backup_days: 14 } }.
";

/// A constraint is never out-ranked: an `@override` whose value violates
/// it is a deny naming the refinement's place and both witnesses, and the
/// plan lists it with the conflicts.
#[test]
fn an_override_that_violates_a_refinement_is_a_deny() {
    let s = Scratch::new("refine-override");
    plan(&s, SETTINGS).success();
    let r = plan(
        &s,
        &format!(
            "{SETTINGS}days(40).\n\
             arg(settings, prod, \"db.backup_days\", D) @override :- days(D).\n"
        ),
    )
    .failure();
    assert!(
        r.stdout
            .contains("conflicts:\n! settings.prod db.backup_days: 40 violates range(1, 35)\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("    override 40  from arg(")
            && r.stdout.contains(
                "    refinement \"range(1, 35)\"  from type_refine(\"settings\", \"db.backup_days\", \"range(1, 35)\") (at p.df:3:3)"
            ),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr.contains(
            "- refinement violated ctx={\"addr\":\"prod\",\"at\":\"p.df:3:3\",\"constraint\":\"range(1, 35)\",\"path\":\"db.backup_days\",\"reason\":\"40 violates range(1, 35)\""
        ),
        "{}",
        r.stderr
    );
    // A losing rank's value is not checked: the winning one is.
    plan(
        &s,
        &format!(
            "{SETTINGS}days(40).\n\
             arg(settings, prod, \"db.backup_days\", D) @default :- days(D).\n"
        ),
    )
    .success();
}

/// A literal that violates a checkable refinement is a compile error with
/// both places; a provider schema's refinement is named by its fact.
#[test]
fn a_literal_that_violates_a_refinement_is_a_compile_error() {
    let s = Scratch::new("refine-literal");
    let r = plan(
        &s,
        "edition 2026.
type settings {
  db.backup_days: int where 1 <= db.backup_days <= 35
}.
settings prod { db = { backup_days: 40 } }.
",
    )
    .failure();
    assert!(
        r.stderr.contains(
            "p.df:5:17: 40 violates the refinement range(1, 35) of settings .db.backup_days"
        ) && r.stderr.contains("refined here: range(1, 35)")
            && r.stderr.contains(" 3 │   db.backup_days: int where"),
        "{}",
        r.stderr
    );
    // providers/fake/schema.df: type_refine(net.subnet, cidr, prefix_len_le(24)).
    let r = plan(
        &s,
        "edition 2026.\nresource net.subnet a { cidr = \"10.0.0.0/26\" }.\n",
    )
    .failure();
    assert!(
        r.stderr.contains(
            "\"10.0.0.0/26\" violates the refinement prefix_len_le(24) of net.subnet .cidr"
        ) && r.stderr.contains(
            "the provider schema states type_refine(net.subnet, cidr, prefix_len_le(24))"
        ),
        "{}",
        r.stderr
    );
    plan(
        &s,
        "edition 2026.\nresource net.subnet a { cidr = \"10.0.1.0/24\" }.\n",
    )
    .success();
}

/// A refinement that does not fit the table (another attribute) is a deny
/// rule with the refinement's place, not a cell constraint.
#[test]
fn a_cross_attribute_refinement_lowers_to_a_deny() {
    let s = Scratch::new("refine-cross");
    let src = |min: i64| {
        format!(
            "edition 2026.
type settings {{
  pool.min: int
  pool.max: int where pool.min <= pool.max
}}.
settings prod {{ pool = {{ min: {min}, max: 3 }} }}.
"
        )
    };
    plan(&s, &src(2)).success();
    let r = plan(&s, &src(5)).failure();
    assert!(
        r.stderr.contains(
            "- refinement violated ctx={\"addr\":\"prod\",\"at\":\"p.df:4:3\",\"constraint\":\"\\\"pool.min\\\" <= \\\"pool.max\\\"\",\"path\":\"pool.max\",\"reason\":\"3 does not satisfy \\\"pool.min\\\" <= \\\"pool.max\\\"\""
        ),
        "{}",
        r.stderr
    );
}

fn gke(s: &Scratch, extra: &[&str]) -> Run {
    let prog = repo().join("examples/refine_gke.df");
    let args = [
        "--file",
        prog.to_str().unwrap(),
        "--provider",
        "gke",
        "--root",
        s.dir.to_str().unwrap(),
        "--world",
        "w.json",
    ];
    s.run(&[&args[..], extra].concat())
}

/// E §4.1 case 4: the refinement on the cluster's zones is deferred while
/// they are an open null, and checked at the boundary that resolves them:
/// the cluster came back in two zones, the refinement wants three, and
/// apply stops after tick 1 like any deny between ticks.
#[test]
fn a_refinement_on_a_null_is_deferred_and_fires_after_the_boundary() {
    let s = Scratch::new("refine-gke");
    let r = gke(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "? refinement on ?gke_cluster/pngu#zones deferred: len_ge(3) of gke_cluster.pngu .zones, decided after tick 1\n"
        ),
        "{}",
        r.stdout
    );
    let r = gke(&s, &["apply"]).failure();
    assert!(r.stdout.contains("tick 1:"), "{}", r.stdout);
    assert!(!r.stdout.contains("tick 2:"), "{}", r.stdout);
    assert!(
        r.stderr
            .contains("constraint violations after tick 1:\n- refinement violated ctx={\"addr\":\"pngu\",\"at\":\"")
            && r.stderr.contains("examples/refine_gke.df:136:3\",\"constraint\":\"len_ge(3)\",\"path\":\"zones\"")
            && r.stderr
                .contains("apply stopped after tick 1: blocked by constraints"),
        "{}",
        r.stderr
    );
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let made: Vec<&String> = w["resources"].as_object().unwrap().keys().collect();
    assert_eq!(
        made,
        [
            "gke_cluster::pngu",
            "google_compute_address::static_ip",
            "google_compute_subnetwork::gke_subnet",
        ]
    );
}

const VAULT: &str = "edition 2026.
type_provider(vault.secret, fakecloud).
type_attr(vault.secret, id, string, [computed, id]).
type_attr(vault.secret, value, string, [computed, sensitive]).
type_mint(vault.secret, value, \"MINT\").
type_provider(app.db, fakecloud).
type_attr(app.db, id, string, [computed, id]).
type_attr(app.db, password, string, [sensitive]).
type_refine(app.db, password, len_ge(16)).
";

/// F DR-13 revised: the engine never checks a secret. A refinement on a
/// sensitive path is an Apply assertion; the mock materializes the secret,
/// checks it, and fails the action when it does not hold.
#[test]
fn a_refinement_on_a_secret_is_an_apply_assertion() {
    let s = Scratch::new("refine-secret");
    let apply = |mint: &str| {
        s.write("schema.df", &VAULT.replace("MINT", mint));
        s.write(
            "p.df",
            "edition 2026.
resource vault.secret pw {}.
resource app.db main { password = ref(vault.secret, pw, .value) }.
",
        );
        let _ = std::fs::remove_file(s.path("w.json"));
        let _ = std::fs::remove_dir_all(s.path(".dform"));
        s.run(&[
            "--file",
            "p.df",
            "--provider",
            "./schema.df",
            "--world",
            "w.json",
            "apply",
        ])
    };
    let r = apply("hunter2").failure();
    assert!(
        r.stderr.contains(
            "apply app.db/main: assertion failed: app.db/main .password fails its refinement len_ge(16)"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("hunter2") && !r.stdout.contains("hunter2"));
    let r = apply("correct-horse-battery-staple").success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// E0306: a provider whose Schema does not declare `checks_refinements`
/// (the Kubernetes provider) cannot take a refinement on a sensitive path.
#[test]
fn e0306_a_refinement_on_a_sensitive_path_the_provider_cannot_check() {
    let s = Scratch::new("refine-e0306");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "p.df",
        "edition 2026.
provider k8s { source = \"./providers/k8s\" }.
type k8s.secret {
  data.password: string where len(data.password) >= 16
}.
resource k8s.secret db { metadata.name = \"db\", data = { password: \"x\" } }.
",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_dform"))
        .args(["--file", "p.df", "plan"])
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .output()
        .unwrap();
    let r = Run::from(out).failure();
    assert!(
        r.stderr.contains(
            "p.df:4:3: E0306: a refinement on sensitive path k8s.secret .data.password cannot be checked by the engine, and provider kubernetes does not check refinements"
        ) && r.stderr.contains("1 error"),
        "{}",
        r.stderr
    );
}
