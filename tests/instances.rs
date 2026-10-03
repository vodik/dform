//! A component is a resource type the program defines (R-65, R-67): its
//! copies are instances, named and printed as resources are.

mod common;
use common::Scratch;

/// A component, `vpc`, copied twice, one copy gated by a clause.
const NET: &str = r#"
input env: string = "dev"
provider fake
component vpc {
  input vpc_net: string
  output id = vpc.cidr
  resource net.vpc vpc { cidr = vpc_net }
  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) }
}
instance vpc blue { vpc_net = "10.1.0.0/16" }
instance vpc green { vpc_net = "10.2.0.0/16" } where env != "dev"
"#;

fn project(name: &str, extra: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("main.df", &format!("{NET}{extra}"));
    s
}

/// `why` prints a copy's own relations by their names there, and a gated
/// copy's gate as the statement that makes it (R-73 item 3), never in the
/// core's spelling (`green::__instance("vpc")`, `green::vpc_net(..)`).
#[test]
fn why_prints_a_copys_relations_in_its_frame() {
    let s = project("instances-why", "");
    let r = s
        .run(&[
            "why",
            "net.vpc[\"green/vpc\"].cidr",
            "main.df",
            "--set",
            "env=prod",
        ])
        .success();
    assert!(
        r.stdout.contains("├─ instance vpc green\n")
            && r.stdout
                .contains("└─ vpc_net(\"10.2.0.0/16\")   (in green)\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("::"), "{}", r.stdout);
    // A copy's resource names the copy's frame (R-67).
    assert!(
        r.stdout
            .contains("resource net.vpc vpc { cidr = vpc_net }   (instance vpc green)\n"),
        "{}",
        r.stdout
    );
}

/// A name whose `/` comes from a value at run time is refused where the
/// plan assembles addresses (R-73 item 5): `/` separates a copy's scope.
#[test]
fn a_name_with_a_slash_from_a_value_is_an_error() {
    let s = project(
        "instances-slash",
        "part(\"a/b\")\nresource net.vpc \"${p}\" { cidr = \"10.9.0.0/16\" } where part(p)\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "net.vpc[\"a/b\"]: its name holds `/` from a value the program computed (\"a/b\")"
        ),
        "{}",
        r.stderr
    );
    // A copy's own resources are scoped, and pass.
    let s = project("instances-slash-ok", "");
    s.run(&["plan", "main.df"]).success();
}

/// `x in vpc` binds every copy of the component (R-67), and `x.id` reads
/// that copy's output.
#[test]
fn x_in_a_component_binds_every_copy() {
    let s = project("instances-in", "ids(i, c) where i in vpc, c = i.id\n");
    let r = s
        .run(&["query", "ids(i, c)", "main.df", "--set", "env=prod"])
        .success();
    assert!(
        r.stdout.contains("\"blue\"   net.vpc[\"blue/vpc\"].cidr\n")
            && r.stdout
                .contains("\"green\"  net.vpc[\"green/vpc\"].cidr\n"),
        "{}",
        r.stdout
    );
}

/// The plan prints a copy as a composite resource, its resources indented
/// under it and counted as themselves in the summary (R-67).
#[test]
fn the_plan_groups_a_copys_resources_under_it() {
    let s = project("instances-plan", "");
    let r = s.run(&["plan", "main.df", "--set", "env=prod"]).success();
    assert!(
        r.stdout.starts_with("plan: 4 deformations (4 create)\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "+ vpc[\"blue\"]\n  + net.vpc[\"blue/vpc\"]\n    cidr = \"10.1.0.0/16\"\n  \
             + net.subnet[\"blue/a\"]\n"
        ) && r
            .stdout
            .contains("+ vpc[\"green\"]\n  + net.vpc[\"green/vpc\"]\n"),
        "{}",
        r.stdout
    );
    // A removed copy's deletes are its own: state remembers it.
    s.run(&["apply", "main.df", "--set", "env=prod", "--yes"])
        .success();
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout
            .contains("- vpc[\"green\"]\n  - net.subnet[\"green/a\"]\n"),
        "{}",
        r.stdout
    );
}

/// `lifecycle(blue, "prevent_destroy")` covers every resource of the copy,
/// and the copy has a `deformation` row of its own, `delete` when all of
/// its resources are deleted (R-67).
#[test]
fn a_lifecycle_over_a_copy_covers_its_resources() {
    let s = project("instances-lifecycle", "");
    // The header first: `input` above the body.
    s.write(
        "main.df",
        &format!(
            "input subnets: bool = true\n{}lifecycle(blue, \"prevent_destroy\")\n",
            NET.replace(
                "  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) }\n",
                "  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) } where subnets\n",
            )
        ),
    );
    s.run(&["apply", "main.df", "--yes"]).success();
    // One resource of the copy goes: denied, by the copy's flag.
    let r = s
        .run(&["plan", "main.df", "--set", "subnets=false"])
        .failure();
    assert!(
        r.stdout.contains(
            "! lifecycle prevent_destroy on vpc[\"blue\"]: the plan would delete \
             net.subnet[\"blue/a\"]\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        !r.stdout.contains("would delete vpc[\"blue\"]"),
        "{}",
        r.stdout
    );
    // The copy goes: its own row is a delete.
    s.write(
        "main.df",
        &s.read("main.df").replace(
            "instance vpc blue { vpc_net = \"10.1.0.0/16\" }",
            "instance vpc blue { vpc_net = \"10.1.0.0/16\" } where env == \"none\"",
        ),
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stdout
            .contains("! lifecycle prevent_destroy: the plan would delete vpc[\"blue\"]\n"),
        "{}",
        r.stdout
    );
}

/// A policy reads a copy's row as a resource's: `deformation(k, x, _), x in
/// vpc` binds the copy, `x.id` its output.
#[test]
fn a_policy_reads_a_copys_deformation_row() {
    let s = project(
        "instances-policy",
        "deny \"a new copy ${x} at ${x.id}\" where deformation(\"create\", x, _), x in vpc\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stdout
            .contains("! a new copy blue at net.vpc[\"blue/vpc\"].cidr\n"),
        "{}",
        r.stdout
    );
}
