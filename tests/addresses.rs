//! A resource's address is its path (R-112): the module or copy path, the
//! local name last (`k3s.admin`, a nested copy's `edge.left.vpc`), as a
//! `let`'s is. A local name holding a dot is one quoted segment
//! (`k3s."k8s-lab.vodik.xyz"`, R-77's quoting). `/` in an address a
//! program writes is an error naming the dot form. State, the world and
//! the plan file carry the path.

mod common;
use common::Scratch;

/// k3s.df holds two resources, one named by a DNS name; the stack uses
/// it and reads one through the module's path.
fn k3s(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "k3s.df",
        "\n\nlet host = \"k8s-lab.vodik.xyz\"\n\n\
         resource net.vpc admin {\n  cidr = \"10.1.0.0/16\"\n}\n\n\
         resource net.vpc \"${host}\" {\n  cidr = \"10.2.0.0/16\"\n}\n",
    );
    s.write(
        "stacks/main.df",
        "\n\nuse fake\n\nuse k3s\n\n\
         resource net.subnet s {\n  cidr = k3s.admin.cidr\n}\n",
    );
    s
}

/// A module's resource is `k3s.admin`, read `k3s.admin.cidr`; one whose
/// name holds a dot is `k3s."k8s-lab.vodik.xyz"`.
#[test]
fn a_modules_resource_is_its_path() {
    let s = k3s("addresses-module");
    let r = s.run(&["plan", "--why=none", "main"]).success();
    for want in [
        "+ net.vpc[\"k3s.admin\"]\n",
        "+ net.vpc[\"k3s.\\\"k8s-lab.vodik.xyz\\\"\"]\n",
        "+ net.subnet[\"s\"]\n  cidr = \"10.1.0.0/16\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
}

/// The world, state's keys and the plan file carry the path; a quoted
/// segment round trips, and `why` takes the path as the plan prints it.
#[test]
fn a_quoted_segment_round_trips_through_the_plan_file_and_the_world() {
    let s = k3s("addresses-quoted");
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "main",
    ])
    .success();
    let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
    let names: Vec<&str> = f["deformations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(
        names.contains(&"k3s.admin") && names.contains(&r#"k3s."k8s-lab.vodik.xyz""#),
        "{names:?}"
    );
    s.run(&["apply", "plan.json"]).success();
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    for key in ["net.vpc::k3s.admin", r#"net.vpc::k3s."k8s-lab.vodik.xyz""#] {
        assert!(w["resources"][key].is_object(), "{key}: {w}");
    }
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "main"])
        .success();
    assert_eq!(r.summary(), "stack main is up to date", "{}", r.stdout);
    for path in [
        r#"k3s."k8s-lab.vodik.xyz""#,
        r#"net.vpc k3s."k8s-lab.vodik.xyz""#,
    ] {
        let r = s
            .run(&["dev", "--world", "w.json", "why", path, "main"])
            .success();
        assert!(
            r.stdout.starts_with("net.vpc k3s.\"k8s-lab.vodik.xyz\"\n"),
            "{path}: {}",
            r.stdout
        );
    }
}

/// A nested copy's resource is every scope in front: `edge.left.vpc`, and
/// `T["edge.left.vpc"]` names it from the top.
#[test]
fn a_nested_copys_resource_is_every_scope_in_front() {
    let s = Scratch::new("addresses-nested");
    s.write(
        "p.df",
        "component spoke {\n  resource net.vpc vpc {\n    cidr = \"10.1.0.0/16\"\n  }\n}\n\
         component pair {\n  resource spoke left {}\n}\nresource pair edge {}\nuse fake\n\
         resource net.subnet s {\n  cidr = net.vpc[\"edge.left.vpc\"].cidr\n}\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    for want in [
        "+ net.vpc[\"edge.left.vpc\"]\n",
        "+ net.subnet[\"s\"]\n  cidr = \"10.1.0.0/16\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
}

/// `/` in an address a program writes is an error naming the path, and
/// so is R-72's `::` before it; a `/` inside a quoted segment is a name's.
#[test]
fn a_slash_in_an_address_is_an_error_naming_the_path() {
    let s = Scratch::new("addresses-slash");
    s.write(
        "p.df",
        "use fake\nresource net.vpc \"a/b\" {\n  cidr = \"10.1.0.0/16\"\n}\n\
         p(c) where c = net.vpc[\"blue/vpc\"].cidr\n\
         q(c) where c = net.vpc[\"\\\"a/b\\\"\"].cidr\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("\"blue/vpc\": an address is a path, its scope separated by `.`, not `/`")
            && r.stderr.contains("write \"blue.vpc\" (R-112)"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("a/b"), "{}", r.stderr);
}
