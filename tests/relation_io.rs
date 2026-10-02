//! Inputs and outputs are one grammar, value or relation (R-55): a
//! relation is declared once, by `decl`; `input p from TERM` gives a
//! stack's relation rows, `input p` declares a relation a module's user
//! gives in its `use` or `instance` block, by rows or `p from TERM`; and
//! `output p` exports a relation.

mod common;
use common::Scratch;

fn plan(s: &Scratch, file: &str) -> common::Run {
    s.run(&["dev", "--world", "w.json", "plan", file])
}

/// Several sources of one relation are one relation, with the facts the
/// program states.
#[test]
fn sources_and_stated_rows_are_one_relation() {
    let s = Scratch::project("rel-io-union");
    s.write("a.facts", "edition 2026\n\nr(\"a\")\n");
    s.write("b.facts", "edition 2026\n\nr(\"b\")\n");
    s.write(
        "p.df",
        "edition 2026\ninput r from facts(\"a.facts\")\ninput r from facts(\"b.facts\")\n\
         decl r(x)\nr(\"c\")\nprovider fake\n",
    );
    let r = s.run(&["query", "r(x)", "p.df"]).success();
    for x in ["\"a\"", "\"b\"", "\"c\""] {
        assert!(r.stdout.contains(x), "{x}: {}", r.stdout);
    }
    assert!(r.stdout.contains("(3 rows)"), "{}", r.stdout);
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
            "edition 2026\nkey env: enum(\"dev\", \"prod\") = \"dev\"\n{NET}\
             az(\"a\", 0)\naz(\"b\", 1)\n\
             instance subnets blue {{\n  cidr = \"10.0.0.0/16\"\n  zone(n, i) where az(n, i)\n  \
             zone from csv(\"z.csv\")\n}}\n\
             instance subnets green {{\n  cidr = \"10.1.0.0/16\"\n  zone(\"x\", 9)\n}} \
             where env == \"prod\"\n\
             provider fake\n"
        ),
    );
    let r = plan(&s, "p.df").success();
    for (addr, cidr) in [
        ("blue::s-a", "10.0.0.0/24"),
        ("blue::s-b", "10.0.1.0/24"),
        ("blue::s-c", "10.0.2.0/24"),
    ] {
        assert!(
            r.stdout
                .contains(&format!("+ net.subnet[\"{addr}\"]\n  cidr = \"{cidr}\"\n")),
            "{addr}: {}",
            r.stdout
        );
    }
    assert!(!r.stdout.contains("green::"), "{}", r.stdout);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df", "env=prod"])
        .success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"green::s-x\"]\n  cidr = \"10.1.9.0/24\"\n"),
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
        "edition 2026\ninput zone\ndecl zone(name: string, index: int)\n\
         let count = list.len([ z | zone(z, _) ])\n",
    );
    s.write(
        "p.df",
        "edition 2026\nuse zones {\n  zone(\"a\", 0)\n  zone(z, 1) where z = \"b\"\n}\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  n = zones.count\n}\nprovider fake\n",
    );
    let r = plan(&s, "p.df").success();
    assert!(r.stdout.contains("  n = 2\n"), "{}", r.stdout);

    s.write(
        "p.df",
        "edition 2026\ninput zone\ndecl zone(n: string)\nprovider fake\n",
    );
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr
            .contains("`input zone` with no `from` is a module's relation"),
        "{}",
        r.stderr
    );
    s.write(
        "zones.df",
        "edition 2026\ninput zone from csv(\"z.csv\")\ndecl zone(name: string)\n",
    );
    s.write("p.df", "edition 2026\nuse zones\nprovider fake\n");
    let r = plan(&s, "p.df").failure();
    assert!(
        r.stderr
            .contains("a module's relation is given by its user: declare `input zone`"),
        "{}",
        r.stderr
    );
}
