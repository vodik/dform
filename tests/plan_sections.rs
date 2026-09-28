//! The plan printer's sections (E §7.4, F DR-2 revised, DR-9 revised):
//! set-aware diffs, "may derive after tick N", shadowed and conflicts, the
//! summary line and the apply order.

mod common;
use common::{Scratch, repo};

fn gke(s: &Scratch, extra: &[&str], cmd: &str) -> common::Run {
    let prog = repo().join("examples/adversarial/gke_two_phase.df");
    let mut args = vec!["--file", prog.to_str().unwrap()];
    for e in extra {
        args.extend(["--file", e]);
    }
    args.extend(["--provider", "gke", "--world", "w.json", cmd]);
    s.run(&args)
}

#[test]
fn gke_plan_has_the_summary_hints_and_apply_order() {
    let s = Scratch::new("sections-gke");
    let r = gke(&s, &[], "plan").success();
    let first = r.stdout.lines().next().unwrap();
    assert_eq!(
        first,
        "plan: 3 deformations (3 create), 4 pending, 1 undetermined"
    );
    for want in [
        "definite:\n+ google_compute_subnetwork.gke_subnet\n",
        "pending on ?gke_cluster/pngu#ca_certificate ?gke_cluster/pngu#endpoint (resolves after tick 1):\n",
        "? gke_nodepool.? x unknown, on ?gke_cluster/pngu#zones, resolves after tick 1",
        "? deny \"cluster must be in at least two zones\" on ?gke_cluster/pngu#zones, decided after tick 1",
        "apply order: tick 1 [google_compute_subnetwork.gke_subnet gke_cluster.pngu google_compute_address.static_ip] tick 2 [k8s.deployment.api k8s.namespace.pngu k8s.secret.db_credentials gke_nodepool.?]\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("to create"), "{}", r.stdout);
}

/// F DR-2 revised, last clause: a deny whose body positively reads a
/// predicate with a stuck instance is not undetermined, but may derive
/// after the boundary; the plan says so.
#[test]
fn a_deny_reading_a_stuck_predicate_may_derive_after_the_tick() {
    let s = Scratch::new("sections-may-derive");
    s.write(
        "extra.df",
        r#"deny("no nodepool in zone z", {pool: N}) :-
  want(gke_nodepool, N),
  arg(gke_nodepool, N, zone, "us-east1-z").
"#,
    );
    let r = gke(&s, &["extra.df"], "plan").success();
    assert!(
        r.stdout.contains(
            "? deny \"no nodepool in zone z\" on ?gke_cluster/pngu#zones, may derive after tick 1"
        ),
        "{}",
        r.stdout
    );
    assert!(r.summary().ends_with(", 2 undetermined"), "{}", r.stdout);
}

const AWS: &str = "examples/aws_demo.df";

fn aws(s: &Scratch, cmd: &str) -> common::Run {
    let prog = repo().join(AWS);
    s.run(&[
        "--file",
        prog.to_str().unwrap(),
        "--provider",
        "aws-mock",
        "--world",
        "w.json",
        cmd,
    ])
}

/// A keyless set diffs by element: one rule opened by hand is one element
/// removed, not every later index shifting.
#[test]
fn a_keyless_set_diffs_by_element() {
    let s = Scratch::new("sections-set");
    aws(&s, "apply").success();
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]["aws_security_group::web"]["attrs"]["ingress"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            serde_json::json!({"from_port": 22, "to_port": 22, "protocol": "tcp", "cidr_blocks": ["0.0.0.0/0"]}),
        );
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = aws(&s, "plan").success();
    assert!(
        r.stdout.contains(
            "~ aws_security_group.web\n  - ingress[]\n      cidr_blocks[0] was \"0.0.0.0/0\"\n      from_port was 22\n      protocol was \"tcp\"\n      to_port was 22\napply order"
        ),
        "{}",
        r.stdout
    );
}

/// A list with merge keys diffs by element too: a new container is one
/// added element with its leaves.
#[test]
fn a_keyed_list_diffs_by_element() {
    let s = Scratch::new("sections-keyed");
    let one = r#"
resource k8s.deployment api {
  metadata.name = "api",
  spec.selector.matchLabels = {app: "api"},
  spec.template.spec.containers = [ {name: "app", image: "api:1"} ]
}.
"#;
    s.write("p.df", one);
    let args = ["--file", "p.df", "--provider", "k8s", "--world", "w.json"];
    s.run(&[&args[..], &["apply"]].concat()).success();
    s.write(
        "p.df",
        &one.replace(
            r#"{name: "app", image: "api:1"} ]"#,
            r#"{name: "app", image: "api:1"}, {name: "sidecar", image: "envoy:1"} ]"#,
        ),
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert!(
        r.stdout.contains(
            "~ k8s.deployment.api\n  + spec.template.spec.containers[name=sidecar]\n      image = \"envoy:1\"\n      name = \"sidecar\"\n"
        ),
        "{}",
        r.stdout
    );
}

/// DR-9 revised and E §2.8: a shadowed disagreement is a section, and a
/// conflict is a section naming the resource, the path and every witness;
/// the conflicted address is not a deformation, and the plan refuses.
#[test]
fn shadowed_and_conflicts_are_sections() {
    let s = Scratch::new("sections-conflict");
    s.write(
        "p.df",
        r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
arg(net.vpc, main, cidr, "10.1.0.0/16").
resource net.vpc two @default { cidr = "10.0.0.0/16" }.
arg(net.vpc, two, cidr, "10.9.0.0/16", default).
arg(net.vpc, two, cidr, "10.3.0.0/16").
"#,
    );
    let r = s
        .run(&["--file", "p.df", "--world", "w.json", "plan"])
        .failure();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 create), 1 conflict",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("+ net.vpc.main"), "{}", r.stdout);
    for want in [
        "shadowed:\n! net.vpc.two cidr at rank default: two contributions disagree at cidr\n",
        "conflicts:\n! net.vpc.main cidr: two contributions disagree\n    normal \"10.0.0.0/16\"  from arg(\"net.vpc\", \"main\", \"cidr\", \"10.0.0.0/16\", \"normal\")\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

/// A conflict at a sensitive path prints neither witness's value nor the
/// rule text that spells it.
#[test]
fn a_conflict_at_a_sensitive_path_is_redacted_in_the_plan() {
    let s = Scratch::new("sections-conflict-secret");
    s.write(
        "p.df",
        r#"
resource leaky.vault v { password = "VAULT-SECRET-A" }.
arg(leaky.vault, v, password, "VAULT-SECRET-B").
"#,
    );
    let schema = repo().join("providers/leaky/schema.df");
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "plan",
        ])
        .failure();
    assert!(
        r.stdout.contains("conflicts:\n! leaky.vault.v password"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("VAULT-SECRET"), "{}", r.stdout);
}

/// The executor's entries in text: a replace with its marker and a
/// prevent_destroy deny as a section, the plan still printed.
#[test]
fn a_denied_replace_is_a_section() {
    let s = Scratch::new("sections-denied");
    let net = "resource net.vpc main { cidr = \"10.0.0.0/16\" }.\n";
    s.write("p.df", net);
    let args = ["--file", "p.df", "--world", "w.json"];
    s.run(&[&args[..], &["apply"]].concat()).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(net.vpc, main, prevent_destroy).\n",
            net.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).failure();
    assert_eq!(
        r.stdout,
        "plan: 1 deformation (1 replace)\ndefinite:\n\
         -/+ net.vpc.main  (replace)\n  cidr: \"10.0.0.0/16\" -> \"10.1.0.0/16\"\n\
         denied:\n! lifecycle prevent_destroy: the plan would replace net.vpc.main\n\
         apply order: tick 1 [net.vpc.main]\n"
    );
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}
