//! The compiler's relations are its own (`ast::Name::Internal`): a
//! program that writes a name of the same text writes a relation of its
//! own, and never reaches the compiler's helpers, its facts or its
//! functions; a program's `__`-led variable is a variable like any other;
//! and a message spells what the compiler wrote as the program wrote it.

use crate::common;
use common::{Run, Scratch};

fn dev(name: &str, src: &str, args: &[&str]) -> Run {
    let s = Scratch::new(name);
    s.write("p.df", src);
    let mut all = vec!["dev", "--provider", "fake"];
    all.extend_from_slice(args);
    all.push("p.df");
    s.run(&all)
}

/// A program's `__neg_0` is not the helper a `not { }` lowers to.
#[test]
fn a_programs_neg_fact_does_not_empty_a_negation() {
    let src = "p(1)\np(2)\ns(2)\n__neg_0(1)\nq(x) where p(x), not { s(x) }\n";
    let r = dev("internal-neg", src, &["query", "q"]).success();
    assert_eq!(
        r.stdout.lines().collect::<Vec<_>>(),
        ["a", "1"],
        "{}",
        r.stdout
    );
}

/// A program's `__enum` fact adds no member to an enum type.
#[test]
fn a_programs_enum_fact_adds_no_member() {
    let src = "type E = enum(\"a\", \"b\")\n__enum(\"E\", [\"z\"])\n\
               ok(x) where x in [\"a\", \"z\"], x in E\n";
    let r = dev("internal-enum", src, &["query", "ok"]).success();
    assert_eq!(
        r.stdout.lines().collect::<Vec<_>>(),
        ["a", "\"a\""],
        "{}",
        r.stdout
    );
}

/// The lowering's `__known` is no predicate a body calls: a body's
/// `__known(x)` reads a relation the program does not define.
#[test]
fn the_lowerings_known_is_not_a_body_predicate() {
    let src = "p(1)\nk(x) where p(x), __known(x)\n";
    let r = dev("internal-known", src, &["query", "k"]).failure();
    assert!(
        r.stderr.contains("undefined predicate __known/1"),
        "{}",
        r.stderr
    );
}

/// A program's `__z` is a variable as any other: `why` binds it.
#[test]
fn a_programs_double_underscore_variable_is_bound() {
    let src = "t(1)\nt(2)\nw(__z) where t(__z), __z > 1\n";
    let r = dev("internal-var", src, &["why", "w(2)"]).success();
    assert!(r.stdout.contains("with __z = 2"), "{}", r.stdout);
}

/// A message spells what the lowering's functions came from as the
/// program wrote it: `x + 1`, never `add(x, 1)`.
#[test]
fn a_why_not_line_spells_the_programs_operator() {
    let src = "p(1)\nq(y) where p(x), y = x + 1\nr(x) where q(x)\n";
    let r = dev("internal-why-op", src, &["why", "r(5)"]).success();
    assert!(r.stdout.contains("y == x + 1: false"), "{}", r.stdout);
    assert!(!r.stdout.contains("add("), "{}", r.stdout);
}
