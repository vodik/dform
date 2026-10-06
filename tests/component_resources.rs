//! A component is a type the program defines, and `resource` makes one of
//! it as it makes one of a provider's type (R-113): `resource vpc blue {
//! .. }` is the copy `blue`, its inputs the block's, its outputs read as
//! attributes (`blue.id`), `x in vpc` its copies. A component of another
//! file is named by its path, as any item is.

mod common;
use common::Scratch;

/// A component, `vpc`, and two resources of it.
const NET: &str = r#"
use fake
component vpc {
  input vpc_net: string
  output id = vpc.cidr
  resource net.vpc vpc { cidr = vpc_net }
}
resource vpc blue { vpc_net = "10.1.0.0/16" }
resource vpc green { vpc_net = "10.2.0.0/16" }
"#;

/// The plan prints a resource of a component as a copy: its own
/// resources under it, each by its path.
#[test]
fn a_resource_of_a_component_is_a_copy() {
    let s = Scratch::project("component-resource");
    s.write("main.df", NET);
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout.contains("  + vpc blue\n    + net.vpc blue.vpc ")
            && r.stdout.contains("  + vpc green\n    + net.vpc green.vpc "),
        "{}",
        r.stdout
    );
}

/// Its outputs read as a resource's attributes do (`blue.id`), and `x in
/// vpc` ranges over the resources of the component.
#[test]
fn its_outputs_read_as_attributes_and_in_ranges_over_it() {
    let s = Scratch::project("component-resource-read");
    s.write(
        "main.df",
        &format!(
            "{NET}bid(x) where x = blue.id\nids(i, c) where i in vpc, c = i.id\n\
             named(1) where blue in vpc\nnamed(2) where green in vpc, not blue in vpc\n"
        ),
    );
    let r = s.run(&["query", "ids(i, c)", "main.df"]).success();
    assert!(
        r.stdout.contains("\"blue\"   net.vpc blue.vpc.cidr\n")
            && r.stdout.contains("\"green\"  net.vpc green.vpc.cidr\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["query", "named(i)", "main.df"]).success();
    assert!(
        r.stdout.contains("1") && !r.stdout.contains("2"),
        "{}",
        r.stdout
    );
    let r = s.run(&["query", "bid(x)", "main.df"]).success();
    assert!(r.stdout.contains("net.vpc blue.vpc.cidr"), "{}", r.stdout);
}

/// A component of another file is its path from the root, with no `use`
/// (the file is loaded for it), or its path through a `use`.
#[test]
fn a_component_of_another_file_is_named_by_its_path() {
    let s = Scratch::project("component-resource-path");
    s.write(
        "modules/network.df",
        "component vpc {\n  input vpc_net: string\n  resource net.vpc vpc { cidr = vpc_net }\n}\n",
    );
    s.write(
        "main.df",
        "\nuse fake\nresource modules.network.vpc main { vpc_net = \"10.1.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout
            .contains("  + modules.network.vpc main\n    + net.vpc main.vpc "),
        "{}",
        r.stdout
    );
    s.write(
        "used.df",
        "\nuse fake\nuse modules.network\nresource network.vpc main { vpc_net = \"10.1.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "used.df"]).success();
    assert!(
        r.stdout
            .contains("  + modules.network.vpc main\n    + net.vpc main.vpc "),
        "{}",
        r.stdout
    );
}

/// What a provider's type's resource does not take: rows; and what a
/// component's does not: a rank, or a name from the clause.
#[test]
fn what_each_kind_of_resource_refuses() {
    let s = Scratch::project("component-resource-errors");
    s.write(
        "rows.df",
        "\nuse fake\nresource net.vpc v {\n  cidr = \"10.0.0.0/16\"\n  p(1)\n}\n",
    );
    let r = s.run(&["plan", "rows.df"]).failure();
    assert!(
        r.stderr
            .contains("net.vpc is no component, so its resource takes no rows"),
        "{}",
        r.stderr
    );
    s.write(
        "rank.df",
        &NET.replace("resource vpc green {", "resource vpc green @default {"),
    );
    let r = s.run(&["plan", "rank.df"]).failure();
    assert!(
        r.stderr
            .contains("a resource of the component vpc takes no rank"),
        "{}",
        r.stderr
    );
    s.write(
        "hole.df",
        &format!(
            "{NET}zone(\"a\")\nresource vpc \"z-${{z}}\" {{ vpc_net = \"10.3.0.0/16\" }} where zone(z)\n"
        ),
    );
    let r = s.run(&["plan", "hole.df"]).failure();
    assert!(
        r.stderr
            .contains("a resource of the component vpc is named statically"),
        "{}",
        r.stderr
    );
}

/// Every resource the program makes is checked against its providers'
/// schemas, a component's and a used module's too: a type none declares
/// is a plan error naming it, before anything is planned.
#[test]
fn a_components_resource_type_is_checked_against_the_providers() {
    let s = Scratch::project("component-resource-types");
    s.write(
        "main.df",
        "\nuse fake\ncomponent box {\n  resource nosuch.thing t { size = 1 }\n}\nresource box b {}\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("provider fake does not declare nosuch.thing"),
        "{}",
        r.stderr
    );
}
