//! A component is a resource type the program defines (R-65, R-67): its
//! copies are instances, named and printed as resources are.

mod common;
use common::Scratch;

/// A component, `vpc`, copied twice, one copy gated by a clause.
const NET: &str = r#"
input env: string = "dev"
use fake
component vpc {
  input vpc_net: string
  output id = vpc.cidr
  resource net.vpc vpc { cidr = vpc_net }
  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) }
}
resource vpc blue { vpc_net = "10.1.0.0/16" }
resource vpc green { vpc_net = "10.2.0.0/16" } where env != "dev"
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
            "--tree",
            "net.vpc[\"green.vpc\"].cidr",
            "main.df",
            "--set",
            "env=prod",
        ])
        .success();
    assert!(
        r.stdout.contains("├─ resource vpc green\n")
            && r.stdout
                .contains("└─ vpc_net(\"10.2.0.0/16\")   (in green)\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("::"), "{}", r.stdout);
    // A copy's resource names the copy's frame (R-67).
    assert!(
        r.stdout
            .contains("resource net.vpc vpc { cidr = vpc_net }   (resource vpc green)\n"),
        "{}",
        r.stdout
    );
}

/// A name whose dot comes from a value at run time is one quoted segment
/// of its address, never a scope (R-112), and so is a `/`: the address is
/// the path, and a name is never refused for what it holds.
#[test]
fn a_name_with_a_dot_from_a_value_is_one_quoted_segment() {
    let s = project(
        "instances-dot",
        "part(\"a.b\")\npart(\"c/d\")\n\
         resource net.vpc \"${p}\" { cidr = \"10.9.0.0/16\" } where part(p)\n",
    );
    let r = s.run(&["plan", "main.df"]).success();
    for name in [r#""a.b""#, r#""c/d""#] {
        assert!(
            r.stdout.contains(&format!("+ net.vpc {name}  ")),
            "{name}: {}",
            r.stdout
        );
    }
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
        r.stdout.contains("\"blue\"   net.vpc blue.vpc.cidr\n")
            && r.stdout.contains("\"green\"  net.vpc green.vpc.cidr\n"),
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
        r.stdout
            .starts_with("plan: 4 changes (4 create) over 1 tick\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  + vpc blue\n    + net.vpc blue.vpc        main.df:7\n        \
             cidr = \"10.1.0.0/16\"  main.df:10\n    + net.subnet blue.a       main.df:8\n        \
             cidr = \"10.1.0.0/16\"  main.df:10\n        vpc_id = blue.vpc\n"
        ) && r
            .stdout
            .contains("  + vpc green\n    + net.vpc green.vpc  "),
        "{}",
        r.stdout
    );
    // A removed copy's deletes are its own: state remembers it.
    s.run(&["apply", "main.df", "--set", "env=prod", "--yes"])
        .success();
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout
            .contains("  - vpc green\n    - net.subnet green.a  main.df:8\n"),
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
            "\ndenied\n  lifecycle prevent_destroy on vpc[\"blue\"]: the plan would delete \
             net.subnet[\"blue.a\"]\n"
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
            "resource vpc blue { vpc_net = \"10.1.0.0/16\" }",
            "resource vpc blue { vpc_net = \"10.1.0.0/16\" } where env == \"none\"",
        ),
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stdout
            .contains("\n  lifecycle prevent_destroy: the plan would delete vpc[\"blue\"]"),
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
        r.stdout.contains(
            "\n  a new copy blue at net.vpc[\"blue.vpc\"].cidr  vpc blue    main.df:12\n"
        ),
        "{}",
        r.stdout
    );
}

/// A copy inside a copy nests again, each resource by its full path; a
/// value given two copies out says where (R-111).
#[test]
fn a_nested_copy_nests_again() {
    let s = Scratch::project("instances-nested");
    s.write(
        "main.df",
        r#"
use fake
component vpc {
  input vpc_net: string
  resource net.vpc vpc { cidr = vpc_net }
  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) }
}
component edge {
  input base: string
  resource vpc left { vpc_net = base }
}
resource edge east { base = "10.1.0.0/16" }
"#,
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout.contains(
            "  + edge east\n    + vpc east.left\n      + net.vpc east.left.vpc   main.df:5\n          \
             cidr = \"10.1.0.0/16\"  main.df:12\n      + net.subnet east.left.a  main.df:6\n          \
             cidr = \"10.1.0.0/16\"  main.df:12\n          vpc_id = east.left.vpc\n"
        ),
        "{}",
        r.stdout
    );
    // `why` takes the path the plan prints, and follows the value through
    // each copy's input to where it is given (R-122).
    let r = s.run(&["why", "east.left.vpc", "main.df"]).success();
    assert!(
        r.stdout.starts_with(
            "net.vpc east.left.vpc  main.df:5\n  cidr = \"10.1.0.0/16\"\n    \
             = vpc_net                  main.df:5\n    \
             = base                     main.df:10\n    \
             = \"10.1.0.0/16\"            main.df:12\n"
        ),
        "{}",
        r.stdout
    );
}
