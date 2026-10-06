//! Inputs and outputs are one grammar, value or relation (R-55): a
//! relation is declared once, by `decl`; `input p from TERM` gives a
//! stack's relation rows, `input p` declares a relation a module's user
//! gives in its `use` or `instance` block, by rows or `p from TERM`; and
//! `output p` exports a relation.

mod common;
use common::Scratch;

fn plan(s: &Scratch, file: &str) -> common::Run {
    s.run(&["dev", "--world", "w.json", "plan", "--why=none", file])
}

/// Several sources of one relation are one relation, with the facts the
/// program states.
#[test]
fn sources_and_stated_rows_are_one_relation() {
    let s = Scratch::project("rel-io-union");
    s.write("a.csv", "x\na\n");
    s.write("b.yaml", "rows:\n  - x: b\n");
    s.write(
        "p.df",
        "\ninput r from csv(\"a.csv\")\ninput r from yaml(\"b.yaml\").rows\n\
         decl r(x: string)\nr(\"c\")\nuse fake\n",
    );
    let r = s.run(&["query", "r(x)", "p.df"]).success();
    for x in ["\"a\"", "\"b\"", "\"c\""] {
        assert!(r.stdout.contains(x), "{x}: {}", r.stdout);
    }
    assert_eq!(r.stdout.lines().count(), 4, "{}", r.stdout);
}

const NET: &str = r#"
component subnets {
  input cidr: inet
  input zone
  decl zone(name: string, index: int)
  resource net.subnet "s-${z}" {
    cidr = inet.subnet(cidr, 8, i)
    zone = z
  } where zone(z, i)
}
"#;

/// A component's relation input is its instance block's: rows written
/// there, a rule over the user's relations, or a table `p from csv(..)`,
/// its columns the component's `decl`; a copy with a clause has rows only
/// while it holds.
#[test]
fn a_components_relation_input_is_given_by_rows_and_by_from() {
    let s = Scratch::project("rel-io-component");
    s.write("z.csv", "name,index\nc,2\n");
    s.write(
        "p.df",
        &format!(
            "\nkey env: enum(\"dev\", \"prod\") = \"dev\"\n{NET}\
             az(\"a\", 0)\naz(\"b\", 1)\n\
             instance subnets blue {{\n  cidr = \"10.0.0.0/16\"\n  zone(n, i) where az(n, i)\n  \
             zone from csv(\"z.csv\")\n}}\n\
             instance subnets green {{\n  cidr = \"10.1.0.0/16\"\n  zone(\"x\", 9)\n}} \
             where env == \"prod\"\n\
             use fake\n"
        ),
    );
    let r = plan(&s, "p.df").success();
    for (addr, cidr) in [
        ("blue.s-a", "10.0.0.0/24"),
        ("blue.s-b", "10.0.1.0/24"),
        ("blue.s-c", "10.0.2.0/24"),
    ] {
        assert!(
            r.stdout.contains(&format!(
                "  + net.subnet[\"{addr}\"]\n    cidr = \"{cidr}\"\n"
            )),
            "{addr}: {}",
            r.stdout
        );
    }
    assert!(!r.stdout.contains("green::"), "{}", r.stdout);
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "plan",
            "--why=none",
            "p.df",
            "env=prod",
        ])
        .success();
    assert!(
        r.stdout
            .contains("  + net.subnet[\"green.s-x\"]\n    cidr = \"10.1.9.0/24\"\n"),
        "{}",
        r.stdout
    );

    // A relation the component does not take is an error naming those it
    // does.
    s.write(
        "p.df",
        &s.read("p.df").replace("zone(\"x\", 9)", "zones(\"x\", 9)"),
    );
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr.contains("subnets takes no relation zones"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("the relations it takes: zone"),
        "{}",
        r.stderr
    );
}

/// A used module's relation input is the `use` block's; `input p` in a
/// stack, or `input p from ..` in a module, is an error.
#[test]
fn a_used_modules_relation_input_is_its_blocks() {
    let s = Scratch::project("rel-io-use");
    s.write(
        "zones.df",
        "\ninput zone\ndecl zone(name: string, index: int)\n\
         let count = list.len([ z | zone(z, _) ])\n",
    );
    s.write(
        "p.df",
        "\nuse zones {\n  zone(\"a\", 0)\n  zone(z, 1) where z = \"b\"\n}\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  n = zones.count\n}\nuse fake\n",
    );
    let r = plan(&s, "p.df").success();
    assert!(r.stdout.contains("  n = 2\n"), "{}", r.stdout);

    s.write("p.df", "\ninput zone\ndecl zone(n: string)\nuse fake\n");
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr
            .contains("`input zone` with no `from` is a module's relation"),
        "{}",
        r.stderr
    );
    s.write(
        "zones.df",
        "\ninput zone from csv(\"z.csv\")\ndecl zone(name: string)\n",
    );
    s.write("p.df", "\nuse zones\nuse fake\n");
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr
            .contains("a module's relation is given by its user: declare `input zone`"),
        "{}",
        r.stderr
    );
}

const VNET: &str = r#"
component vnet {
  input cidr: inet
  resource net.vpc vpc { cidr }
  resource net.subnet "s-${z}" {
    vpc
    cidr = inet.subnet(cidr, 8, i)
    zone = z
  } where az(z, i)
  #| The copy's subnets, by resource: their addresses leave it.
  decl subnet(s: net.subnet, zone: string)
  subnet(s, z) where s in net.subnet, z = s.zone
  output subnet
  output info { cidr = cidr, kind: string = "vpc" }
}
az("a", 0)
az("b", 1)
instance vnet blue { cidr = "10.0.0.0/16" }
instance vnet green { cidr = "10.1.0.0/16" }
"#;

/// `output p` exports a copy's relation (R-55): read as `blue.p(..)`,
/// one fact per row, a resource column as the copy's address; every
/// copy's as `vnet[t].p(..)`. An object output is its fields.
#[test]
fn a_relation_output_is_read_from_a_copy_and_from_every_copy() {
    let s = Scratch::project("rel-io-output");
    s.write(
        "p.df",
        &format!(
            "\n{VNET}\
             resource compute.vm \"vm-${{z}}\" {{\n  size = 1\n  subnet = s\n}} \
             where blue.subnet(s, z), s in net.subnet\n\
             seen(t, s) where vnet[t].subnet(s, _)\n\
             resource net.vpc tally {{\n  cidr = \"10.9.0.0/16\"\n  n\n  info = green.info\n}} \
             where n = list.len([ s | seen(_, s) ])\n\
             use fake\n"
        ),
    );
    let r = plan(&s, "p.df").success();
    for z in ["a", "b"] {
        assert!(
            r.stdout.contains(&format!(
                "+ compute.vm[\"vm-{z}\"]\n  size = 1\n  subnet = ?net.subnet[\"blue.s-{z}\"]\n"
            )),
            "{z}: {}",
            r.stdout
        );
    }
    assert!(
        r.stdout.contains(
            "  cidr = \"10.9.0.0/16\"\n  info.cidr = \"10.1.0.0/16\"\n  info.kind = \"vpc\"\n  n = 4\n"
        ),
        "{}",
        r.stdout
    );
    // A relation the component does not export is its own.
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("blue.subnet(s, z)", "blue.az(z, _), s = z"),
    );
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr.contains("vnet exports no relation az"),
        "{}",
        r.stderr
    );
}

/// A stack's `output p` publishes its rows; another stack reads them as
/// `zones[env=e].p(..)`, one fact per row (R-55).
#[test]
fn a_stacks_relation_output_is_read_across_stacks() {
    let s = Scratch::project("rel-io-stacks");
    s.write(
        "stacks/zones.df",
        "\nkey env: string = \"dev\"\nuse fake\n\
         zone(\"${env}-a\", 0)\nzone(\"${env}-b\", 1)\noutput zone\n",
    );
    s.write(
        "stacks/app.df",
        "\nuse fake\nuse stacks.zones\n\
         resource compute.vm \"vm-${z}\" {\n  size = n\n} where zones[env=\"prod\"].zone(z, n)\n",
    );
    s.run(&["apply", "zones", "env=prod"]).success();
    let r = s.run(&["plan", "--why=none", "app"]).success();
    assert!(
        r.stdout
            .contains("+ compute.vm[\"vm-prod-a\"]\n  size = 0\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ compute.vm[\"vm-prod-b\"]\n  size = 1\n"),
        "{}",
        r.stdout
    );
}
