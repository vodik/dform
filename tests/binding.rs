//! What binds a variable in a body (R-10): `x = t` with `x` unbound (a
//! pattern on the left), `x in t`, an aggregate, a relation atom's free
//! variables. Every other operand needs its variables bound by another
//! literal that does not depend on it, in any order; `=` with both sides
//! bound is an error that says to write `==`.

use crate::common;
use common::error;
use dform_core::engine;
use dform_core::parser::parse_file;
use dform_core::spell;

/// A program file's (`parse_program` reads core text, which the check
/// leaves alone).
fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = parse_file("t.df", &format!("\n{src}")).unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(spell::atom)
        .collect()
}

/// The order of the literals is irrelevant to what binds.
#[test]
fn a_literal_may_read_what_a_later_one_binds() {
    let src = r#"q(1)
q(5)
small(x, y) where x < 3, y = x + 1, q(x)
"#;
    assert_eq!(facts(src, "small"), ["small(1, 2)"]);
}

#[test]
fn an_unbound_operand_is_named_at_its_operator() {
    // `y` is bound only through what reads it.
    let e = error("q(1)\np(x) where q(x), y < x, y = z, z = y + 1\n");
    assert!(
        e.contains("`y` is unbound at this `<`; bind it with `=`, `in`, or a relation first"),
        "{e}"
    );
    let e = error("q(\"a\")\np(z) where q(x), z = str.upper(w), w = str.lower(z)\n");
    assert!(
        e.contains("`w` is unbound in this call of `str.upper`"),
        "{e}"
    );
    // A name nothing binds is unknown, a string meant unquoted.
    let e = error("q(\"a\")\np(x) where q(x), not has w.p\n");
    assert!(e.contains("unknown name `w`"), "{e}");
}

/// `==` compares: it never binds.
#[test]
fn double_equals_never_binds() {
    let e =
        error("image(\"a\")\np(i) where image(i), parsed == json.decode(i), not has parsed.pre\n");
    assert!(
        e.contains("`parsed` is unbound at this `==`; `=` binds, `==` compares"),
        "{e}"
    );
}

#[test]
fn equals_with_both_sides_bound_is_an_error() {
    let e = error("q(1, 1)\np(x) where q(x, y), x = y\n");
    assert!(e.contains("both sides are bound; write `==`"), "{e}");
    let e = error("q(1)\np(x) where q(x), x = 1\n");
    assert!(e.contains("both sides are bound; write `==`"), "{e}");
    // A pattern compares a name it does not bind (R-58).
    let src = r#"pair([1, 2])
q(1)
p(b) where pair(l), q(a), (a, b) = l
"#;
    assert_eq!(facts(src, "p"), ["p(2)"]);
}

/// Two literals that each bind what the other reads bind neither.
#[test]
fn a_cycle_binds_nothing() {
    let e = error("q(1)\np(x) where q(x), a = b + 1, b = a - 1\n");
    assert!(e.contains("is unbound at this"), "{e}");
}

/// The binders: `=`, `in`, an aggregate, an atom; and what a `not`
/// binds stays under it.
#[test]
fn the_binders() {
    let src = r#"q([1, 2, 3])
r(2)
s(n) where n = count(x), q(l), x in l
t(x) where q(l), x in l, not r(x)
u(x) where q(l), x in l, not { r(y), y > x }
"#;
    assert_eq!(facts(src, "s"), ["s(3)"]);
    assert_eq!(facts(src, "t"), ["t(1)", "t(3)"]);
    assert_eq!(facts(src, "u"), ["u(2)", "u(3)"]);
    let e = error("q(1)\np(x) where q(x), not z < x\n");
    assert!(e.contains("unknown name `z`"), "{e}");
}

/// A comprehension's body binds its own names; what its item reads must
/// be bound in it or around it.
#[test]
fn a_comprehension_binds_its_own() {
    let src = r#"q(1)
q(2)
all(l) where l = [x * 2 | q(x)]
"#;
    assert_eq!(facts(src, "all"), ["all([2, 4])"]);
    let e = error("q(1)\nall(l) where l = [x | q(x), y > x]\n");
    assert!(e.contains("unknown name `y`"), "{e}");
}

/// An interpolation reads its holes.
#[test]
fn an_interpolation_reads() {
    let e = error("q(1)\np(s) where q(x), s = \"n-${y}\", y = s\n");
    assert!(e.contains("`y` is unbound in this interpolation"), "{e}");
}

/// `not x = 3` is built as a comparison (R-211 step 9): `x` stays the
/// negation's own, never bound outside it, so a read of `x` after it is
/// the ordinary binding-order error, naming `x`.
#[test]
fn not_equals_is_a_comparison_so_x_stays_unbound_outside_it() {
    let e = error("p(y) where not x = 3, y = x\n");
    assert!(e.contains("`x` is unbound"), "{e}");
    let src = r#"q(3)
q(4)
r(x) where q(x), not x = 3
"#;
    assert_eq!(facts(src, "r"), ["r(4)"]);
}

/// A chained comparison whose middle term is read as two different
/// quantities (ambiguous beside `=`, a duration beside `<`) is a goal
/// per pair, not one comparison read whole: its `=` binds its pattern
/// side, `a = 30m < 1h` binding `a`. A chain with no such quantity reads
/// every term instead, so `b` must already be bound.
#[test]
fn a_chained_equals_with_a_quantity_binds_its_side() {
    let e = error("p(a) where a = b < 3\n");
    assert!(e.contains("`a` is unbound at this `==`"), "{e}");
    let ok = parse_file("t.df", "\np(a) where a = 30m < 1h\n");
    assert!(ok.is_ok(), "{:#}", ok.unwrap_err());
}

/// A computed key in a binding position, `e[i] = P` (R-211 step 9,
/// H-9): the key `i` is read to pick the element, as well as the
/// pattern `P` being bound to it; an unbound key is the error.
#[test]
fn a_computed_key_in_a_binding_position_is_read_too() {
    let e = error("obj({\"a\" = 1})\np(v) where obj(o), o[k] = v\n");
    assert!(e.contains('k'), "{e}");
    let src = r#"obj({"a" = 1})
key("a")
p(v) where obj(o), key(k), o[k] = v
"#;
    assert_eq!(facts(src, "p"), ["p(1)"]);
}

/// A negation written before what binds its names is decided after it.
#[test]
fn a_negation_waits_for_its_binders() {
    let src = r#"q([1, 2, 3])
r(2)
t(x) where not r(x), q(l), x in l
u(x) where not { r(y), y > x }, q(l), x in l
v(i) where not has json.decode(i).pre, version(i)
version("{\"major\": 1}")
"#;
    assert_eq!(facts(src, "t"), ["t(1)", "t(3)"]);
    assert_eq!(facts(src, "u"), ["u(2)", "u(3)"]);
    assert_eq!(facts(src, "v"), [r#"v("{\"major\": 1}")"#]);
}

/// A negation inside another reads what the clause around both binds:
/// its helper takes `x` from `p(x)`, not any `x` of `r` (R-211).
#[test]
fn a_negation_inside_another_shares_the_clauses_variables() {
    let src = r#"p(1)
p(2)
q(10)
r(1, 10)
ok(x) where p(x), not { q(y), not { r(x, y) } }
"#;
    assert_eq!(facts(src, "ok"), ["ok(1)"]);
}
