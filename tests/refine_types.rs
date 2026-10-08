//! Refinement types (E DR-13, F DR-13 revised): a `where` over the
//! attribute's own value that fits the checkable table is a rank-blind
//! constraint in the attribute's cell; checked at compile time against a
//! literal, at collapse against the winning value, deferred while that
//! value carries a null, and deferred to the provider on a sensitive path.
//! Everything else lowers to a deny.

mod common;
use common::{Run, Scratch, repo};

fn plan(s: &Scratch, src: &str) -> Run {
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
}

/// An input's refinement, its check (R-54), over the `set` layers (R-38).
const SETTINGS: &str = "\ninput db {\n  backup_days: int = 3 check 1 <= backup_days <= 35\n}\nuse fake\non(1)\nset db.backup_days = 14 where on(1)\n";

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
            "{SETTINGS}days(40)\n\
             set db.backup_days = d @override where days(d)\n"
        ),
    )
    .failure();
    assert!(
        r.stdout
            .contains("conflicts\n  ! input db.backup_days: 40 violates range(1, 35)\n"),
        "{}",
        r.stdout
    );
    // Each witness by where it was written (R-111), and said once.
    assert!(
        r.stdout.contains(
            "\n      override {\"backup_days\":40}  p.df:9\n      refinement \"range(1, 35)\"  p.df:3\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("refinement violated"), "{}", r.stderr);
    // A losing rank's value is not checked: the winning one is.
    plan(
        &s,
        &format!(
            "{SETTINGS}days(40)\n\
             set db.backup_days = d @default where days(d)\n"
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
        "\ninput db {\n  backup_days: int = 3 check 1 <= backup_days <= 35\n}\non(1)\nset db = { backup_days: 40 } where on(1)\nuse fake\n",
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:6:1: 40 violates the refinement range(1, 35) of input .db.backup_days")
            && r.stderr.contains("refined here: range(1, 35)")
            && r.stderr.contains(" 3 │   backup_days: int = 3 check"),
        "{}",
        r.stderr
    );
    // crates/dform-mock/schemas/fake.df: type_refine(net.subnet, cidr, prefix_len_le(24)).
    let r = plan(
        &s,
        "\nresource net.subnet a { cidr = \"10.0.0.0/26\" }\nuse fake\n",
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
        "\nresource net.subnet a { cidr = \"10.0.1.0/24\" }\nuse fake\n",
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
            "
type compute.vm {{
  pool.min: int
  pool.max: int check pool.min <= pool.max
}}
resource compute.vm prod {{ pool = {{ min: {min}, max: 3 }} }}
\nuse fake\n"
        )
    };
    plan(&s, &src(2)).success();
    let r = plan(&s, &src(5)).failure();
    assert!(
        r.stdout.contains(
            "\nconflicts\n  ! compute.vm prod.pool.max: 3 does not satisfy pool.min <= pool.max  p.df:4\n"
        ),
        "{}",
        r.stdout
    );
}

fn gke(s: &Scratch, extra: &[&str]) -> Run {
    let prog = repo().join("examples/refine/stacks/refine_gke.df");
    s.run(&common::on(
        prog.to_str().unwrap(),
        &[
            "--provider",
            "gke",
            "--provider",
            "k8s",
            "--world",
            "w.json",
        ],
        extra,
    ))
}

/// E §4.1 case 4: the refinement on the cluster's zones is deferred while
/// they are an open null, and checked at the boundary that resolves them:
/// placed in two zones (`--set zones=2`; the example's default is three),
/// the cluster came back in two, the refinement wants three, and apply
/// stops after tick 1 like any deny between ticks.
#[test]
fn a_refinement_on_a_null_is_deferred_and_fires_after_the_boundary() {
    let s = Scratch::new("refine-gke");
    let r = gke(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("  undetermined  len_ge(3) of google.container_cluster pngu.zones  stacks/refine_gke.df:126"),
        "{}",
        r.stdout
    );
    let r = gke(&s, &["apply", "--set", "zones=2"]).failure();
    assert!(r.stdout.contains("tick 1  "), "{}", r.stdout);
    // One plan printed: no later tick was planned.
    assert_eq!(r.stdout.matches("plan: ").count(), 1, "{}", r.stdout);
    assert!(
        r.stderr.contains(
            "constraint violations after tick 1:\n- ! google.container_cluster pngu.zones: \
                       [\"us-east1-b\", \"us-east1-c\"] violates len_ge(3)\n"
        ) && r.stderr.contains("refinement \"len_ge(3)\"  ")
            && r.stderr
                .contains("examples/refine/stacks/refine_gke.df:126\n")
            && !r.stderr.contains("ctx=")
            && r.stderr
                .contains("; stopped after tick 1; ticks 1 to 1 were applied"),
        "{}",
        r.stderr
    );
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let made: Vec<&String> = w["resources"].as_object().unwrap().keys().collect();
    assert_eq!(
        made,
        [
            "google.compute_address::static_ip",
            "google.compute_subnetwork::gke_subnet",
            "google.container_cluster::pngu",
        ]
    );
}

const VAULT: &str = "\ntype_provider(vault.secret, \"fakecloud\")\ntype_attr(vault.secret, \"id\", \"string\", [\"computed\", \"id\"])\ntype_attr(vault.secret, \"value\", \"string\", [\"computed\", \"sensitive\"])\ntype_mint(vault.secret, \"value\", \"MINT\")\ntype_provider(app.db, \"fakecloud\")\ntype_attr(app.db, \"id\", \"string\", [\"computed\", \"id\"])\ntype_attr(app.db, \"password\", \"string\", [\"sensitive\"])\ntype_refine(app.db, \"password\", len_ge(16))\n";

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
            "\nresource vault.secret pw {}\nresource app.db main { password = ref(vault.secret, \"pw\", \"value\") }\nuse fake\n",
        );
        let _ = std::fs::remove_file(s.path("w.json"));
        let _ = std::fs::remove_dir_all(s.path("dform.state"));
        s.run(&[
            "dev",
            "--provider",
            "./schema.df",
            "--world",
            "w.json",
            "apply",
            "p.df",
        ])
    };
    let r = apply("hunter2").failure();
    assert!(
        r.stderr.contains(
            "! apply app.db main: refused, nothing changed\n    assertion failed: app.db \
             main.password fails its refinement len_ge(16)"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("hunter2") && !r.stdout.contains("hunter2"));
    let r = apply("correct-horse-battery-staple").success();
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
}

/// E0306: a provider whose Schema does not declare `checks_refinements`
/// (the Kubernetes provider) cannot take a refinement on a sensitive path.
#[test]
fn e0306_a_refinement_on_a_sensitive_path_the_provider_cannot_check() {
    let s = Scratch::new("refine-e0306");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "p.df",
        "\nuse k8s { source = \"./providers/k8s\" }\ntype k8s.secret {\n  data.password: string check data.password.len >= 16\n}\nresource k8s.secret db { metadata.name = \"db\", data = { password: \"x\" } }\n",
    );
    let out = common::dform()
        .args(["plan", "p.df"])
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .output()
        .unwrap();
    let r = Run::from(out).failure();
    assert!(
        r.stderr.contains(
            "p.df:4:3: E0306: a refinement on sensitive path k8s.secret .data.password cannot be checked by the engine, and provider k8s does not check refinements"
        ) && r.stderr.contains("1 error"),
        "{}",
        r.stderr
    );
}

/// In a `where`, the attribute's name is its value, in the checkable form
/// (`name.len <= 3`, a cell refinement) and in a deny (`code.len != 2`),
/// whose message prints the name as written; another attribute of the
/// block is its value too, through the `prefix_len` builtin.
#[test]
fn a_refinement_names_its_attribute_by_name() {
    let s = Scratch::new("refine-names");
    let src = |name: &str, wide: &str| {
        format!(
            "\ntype app.thing {{
  name: string check name.len <= 3
  code: string check code.len != 2
  net: inet check net.bits >= wide.bits
  wide: inet
}}
resource app.thing a {{
  name = x
  code = x
  net = \"10.0.0.0/24\"
  wide = \"{wide}\"
}} where n(x)
n(\"{name}\")
\nuse fake\n"
        )
    };
    plan(&s, &src("abc", "10.0.0.0/16")).success();
    let r = plan(&s, &src("abcd", "10.0.0.0/16")).failure();
    assert!(
        r.stdout
            .contains("! app.thing a.name: \"abcd\" violates len_le(3)\n"),
        "{}",
        r.stdout
    );
    let r = plan(&s, &src("ab", "10.0.0.0/16")).failure();
    assert!(
        r.stdout
            .contains("! app.thing a.code: ab does not satisfy code.len != 2  p.df:4\n"),
        "{}",
        r.stdout
    );
    let r = plan(&s, &src("abc", "10.0.0.0/28")).failure();
    assert!(
        r.stdout.contains(
            "! app.thing a.net: 10.0.0.0/24 does not satisfy net.bits >= wide.bits  p.df:"
        ),
        "{}",
        r.stdout
    );
}

/// A refinement that calls a function the evaluator does not have, or a
/// `matches` whose pattern does not compile, is a compile error at the
/// refinement: it would otherwise deny every value.
#[test]
fn an_unknown_function_or_a_bad_pattern_is_a_compile_error() {
    let s = Scratch::new("refine-unknown");
    let r = plan(
        &s,
        "\ntype app.thing {\n  name: string check frobnicate(name) == 3\n}\nresource app.thing a { name = \"x\" }\nuse fake\n",
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:3:3: in a refinement: unknown function frobnicate"),
        "{}",
        r.stderr
    );
    let r = plan(
        &s,
        "\ntype app.thing {\n  name: string check matches(name, \"a(\")\n}\nuse fake\n",
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:3:3: in a refinement: regex(\"a(\"): regex parse error"),
        "{}",
        r.stderr
    );
    let r = plan(&s, "\ninput n: int = 1 check frob(n) == 1\nuse fake\n").failure();
    assert!(
        r.stderr
            .contains("p.df:2:1: in a refinement: unknown function frob"),
        "{}",
        r.stderr
    );
}
