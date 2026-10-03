//! `x in T` over a named enum type (R-70): one row per value, in the
//! order the type declares them, lowered as a range is; `why` names the
//! type; an input of an enum type is a cell, not a type. The errors have a
//! case in tests/syntax/err/membership.df.

mod common;
use common::Scratch;
use dform::parser::parse_file;
use dform::partition::fmt_atom;

fn facts(src: &str, pred: &str) -> Vec<String> {
    let p = parse_file("t.df", &format!("\n{src}")).unwrap_or_else(|e| panic!("{e:#}"));
    let (r, _) = dform::engine::eval(&p, &[]).unwrap_or_else(|e| panic!("{e:#}"));
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(fmt_atom)
        .collect()
}

/// Each value, and in declaration order where order shows: a list built
/// over the type is the type's order, not the values' sorted one.
#[test]
fn x_in_an_enum_type_is_each_value_in_order() {
    let src = "type environment = enum(\"staging\", \"prod\", \"dev\")\n\
               bucket(e) where e in environment\n\
               envs(l) where l = [ e | e in environment ]\n\
               other(e) where bucket(e), not e in environment\n";
    assert_eq!(
        facts(src, "bucket"),
        ["bucket(\"dev\")", "bucket(\"prod\")", "bucket(\"staging\")"]
    );
    assert_eq!(
        facts(src, "envs"),
        ["envs([\"staging\", \"prod\", \"dev\"])"]
    );
    assert!(facts(src, "other").is_empty());
}

/// A pattern takes each value with its position, as over a list.
#[test]
fn a_pattern_over_an_enum_type_takes_the_position() {
    assert_eq!(
        facts(
            "type tier = enum(\"small\", \"large\")\nrank(t, i) where (i, t) in tier\n",
            "rank"
        ),
        ["rank(\"large\", 1)", "rank(\"small\", 0)"]
    );
}

/// One resource per value; `why` prints the statement, the value, and the
/// type it came from as the leaf.
#[test]
fn why_shows_the_type_as_the_leaf() {
    let s = Scratch::project("enum-why");
    s.write(
        "p.df",
        "\nprovider fake\n\ntype environment = enum(\"staging\", \"prod\")\n\n\
         resource compute.vm \"web-${e}\" {\n  size = \"small\"\n} where e in environment\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 2 deformations (2 create)",
        "{}",
        r.stdout
    );
    let r = s
        .run(&["why", "compute.vm[\"web-prod\"]", "p.df"])
        .success();
    assert!(
        r.stdout.contains("where e in environment\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("with e = \"prod\"\n"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("└─ type environment = enum(\"staging\", \"prod\")   p.df:4"),
        "{}",
        r.stdout
    );
}

/// An input of an enum type holds one value: `x in env` says to range
/// over its type.
#[test]
fn an_enum_input_is_a_cell() {
    let e = parse_file(
        "t.df",
        "\ninput env: environment = \"dev\"\n\
         type environment = enum(\"dev\", \"prod\")\np(x) where x in env\n",
    )
    .map(|_| ())
    .unwrap_err();
    let e = format!("{e:#}");
    assert!(
        e.contains("`env` is an input, one value of environment"),
        "{e}"
    );
    assert!(e.contains("range over its type: `x in environment`"), "{e}");
}

/// `dform test` takes an enum input's values from the same place
/// `x in T` does, in the type's order.
#[test]
fn the_test_space_is_the_types_values() {
    let s = Scratch::project("enum-test");
    s.write(
        "p.df",
        "\ninput env: environment = \"staging\"\n\nprovider fake\n\n\
         type environment = enum(\"staging\", \"prod\")\n\
         deny \"env ${e} is not prod\" where e in environment, env == e, e != \"prod\"\n",
    );
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "test p: 2 combinations of env\n\
             env      result\n\
             staging  denied\n\
             prod     ok\n\
             denied  dform plan p --set env=staging\n  \
             - env staging is not prod\n"
        ),
        "{}",
        r.stdout
    );
}
