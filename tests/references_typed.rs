//! A reference is a reference everywhere (R-185): a field read of a value
//! that is not an object is an error, never a literal that does not hold,
//! so a deny cannot pass without checking.

mod common;
use common::{Scratch, mock};

/// A program under the fake provider.
fn scratch(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", &format!("\nuse fake\nuse k8s\n\n{src}"));
    s
}

/// A field of a value the compiler cannot type (a document's) that is a
/// string at run time: an error at the deny naming the read and the value,
/// under `plan` and `dform test` alike, where it was a deny that held.
#[test]
fn a_field_of_a_string_is_an_error_at_run_time() {
    let s = scratch(
        "refs-typed-run-time",
        "let doc = json.decode(\"{\\\"tier\\\": \\\"gold\\\"}\")\n\n\
         deny \"tier is not gold\" where not doc.tier.name == \"gold\"\n",
    );
    let want = "`doc.tier.name`: `doc.tier` is the string \"gold\", which has no field `name`";
    let r = mock(&s, &["plan"]).failure();
    assert!(r.stderr.contains(want), "{}", r.stderr);
    assert!(!r.stdout.contains("tier is not gold"), "{}", r.stdout);
    let r = s.run(&["test", "p.df"]).failure();
    assert!(r.stdout.contains(want), "{}{}", r.stdout, r.stderr);
}

/// Two workloads, a Deployment that may run as root and a StatefulSet
/// that may not, and the reviewer's `workload(w)` over both types.
const WORKLOADS: &str = r#"
resource k8s.deployment web {
  metadata.name = "web"
  spec.selector.matchLabels = { app: "web" }
  spec.template.spec.containers = [{ name: "web", image: "nginx:1" }]
}
resource k8s.stateful_set db {
  metadata.name = "db"
  spec.selector.matchLabels = { app: "db" }
  spec.serviceName = "db"
  spec.template.spec.containers = [{ name: "db", image: "postgres:18" }]
  spec.template.spec.securityContext.runAsNonRoot = true
}

workload(w) where w in k8s.deployment
workload(w) where w in k8s.stateful_set
"#;

/// The reviewer's shape: a deny reading a pod field through `workload(w)`
/// fires for the Deployment and holds for the StatefulSet, where it read
/// nothing through the address and passed; `why` says `w` is the
/// resource.
#[test]
fn a_deny_reads_through_a_relation_of_references() {
    let s = scratch(
        "refs-typed-deny",
        &format!(
            "{WORKLOADS}\n\
             deny \"pod may run as root\" {{ workload: w }} where {{\n  \
               workload(w)\n  \
               not w.spec.template.spec.securityContext.runAsNonRoot == true\n\
             }}\n"
        ),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        common::refused_for(&r.stderr, "pod may run as root") == ["workload = \"web\""],
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("workload = \"db\""), "{}", r.stderr);
    let r = mock(&s, &["why", "deny \"pod may run as root\""]).success();
    assert!(
        r.stdout.contains("with w = k8s.deployment web"),
        "{}",
        r.stdout
    );
}

/// A `set` through a column of references writes each row's resource:
/// one statement for both workload types, where it was one per type.
#[test]
fn a_set_writes_through_a_relation_of_references() {
    let s = scratch(
        "refs-typed-set",
        &format!(
            "{WORKLOADS}\n\
             set w.spec.template.spec.containers[_].resources.requests = {{\n  \
               cpu: 100m,\n  \
               memory: 128Mi,\n\
             }} @default where workload(w)\n"
        ),
    );
    let r = mock(&s, &["plan"]).success();
    for c in ["web", "db"] {
        let line = format!(
            "spec.template.spec.containers[name={c}].resources.requests = \
             {{ cpu: \"100m\", memory: \"128Mi\" }}"
        );
        assert!(r.stdout.contains(&line), "{c}: {}", r.stdout);
    }
}

/// A rule per type gives the column the union of them, as hover and the
/// signatures print it.
#[test]
fn a_column_of_several_types_is_their_union() {
    let p = dform::parser::parse_file("t.df", &format!("\nuse k8s\n{WORKLOADS}"))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let l = dform::transform::lower(&p).unwrap_or_else(|e| panic!("{e:#}"));
    let sigs: Vec<String> = l.signatures.values().map(|s| s.to_string()).collect();
    assert!(
        sigs.contains(&"workload(w: ref(k8s.deployment | k8s.stateful_set))".to_string()),
        "{sigs:?}"
    );
}

/// A field read on a column the program types as a string is an error
/// naming the column, its type and the fix, where it read nothing.
#[test]
fn a_field_of_a_string_column_is_a_compile_error() {
    let s = scratch(
        "refs-typed-compile",
        "name(\"web\")\n\
         deny \"root\" { workload: w } where name(w), not w.spec.template.spec.securityContext.runAsNonRoot == true\n",
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`w.spec.template.spec.securityContext.runAsNonRoot`: `w` is string (`name`'s \
             column 1), which has no field `spec`"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("(`w in k8s.deployment`)"), "{}", r.stderr);
}

/// An address string in a column of references is an error at the row,
/// naming the column, its type and the resource to write.
#[test]
fn an_address_string_in_a_reference_column_is_an_error() {
    let s = scratch(
        "refs-typed-row",
        &format!("{WORKLOADS}\nworkload(\"web\")\n"),
    );
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`workload`'s column `w` is ref(k8s.deployment | k8s.stateful_set): a row gives it \
             a resource, not `\"web\"`"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("write the resource: its name in scope, or `T[\"a\"]`"),
        "{}",
        r.stderr
    );
}
