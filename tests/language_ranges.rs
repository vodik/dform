//! Ranges (R-56): `lo..hi` and `lo..=hi` after `in` enumerate integers in
//! order, for "once per i"; a range anywhere else is an error, so it never
//! becomes a list by accident.

mod common;
use common::{Scratch, error, facts};

#[test]
fn a_half_open_range_stops_before_its_end() {
    assert_eq!(
        facts("n(3)\np(i) where n(k), i in 0..k\n", "p"),
        ["p(0)", "p(1)", "p(2)"]
    );
    assert!(facts("p(i) where i in 2..2\n", "p").is_empty());
}

#[test]
fn an_inclusive_range_takes_its_end() {
    assert_eq!(
        facts("n(3)\np(i) where n(k), i in 1..=k\n", "p"),
        ["p(1)", "p(2)", "p(3)"]
    );
    assert_eq!(facts("p(i) where i in 5..=5\n", "p"), ["p(5)"]);
    // `not i in ..` tests a bound i.
    assert_eq!(
        facts("q(0)\nq(4)\np(i) where q(i), not i in 1..=3\n", "p"),
        ["p(0)", "p(4)"]
    );
}

#[test]
fn a_range_end_must_be_bound() {
    let e = error("p(i) where i in 0..n\n");
    assert!(e.contains("unknown name `n`"), "{e}");
}

#[test]
fn a_range_end_is_an_integer() {
    let e = error("p(i) where i in \"a\"..3\n");
    assert!(
        e.contains("a range's ends are integers: `\"a\"..3` has \"a\""),
        "{e}"
    );
}

#[test]
fn a_range_is_not_a_value() {
    for src in [
        "p(x) where x = [0..3]\n",
        "resource net.vpc v { cidrs = 0..3 }\n",
        "p(x) where x = 0..=3\n",
    ] {
        let e = error(src);
        assert!(
            e.contains("a range is enumerated with `in`; `["),
            "{src}: {e}"
        );
        assert!(e.contains("is not a list"), "{src}: {e}");
    }
}

/// One resource per i, and `why` prints the statement with its range as
/// written and i's value.
#[test]
fn why_shows_the_range_as_written() {
    let s = Scratch::project("lang-ranges");
    s.write(
        "p.df",
        "\nuse fake\n\npool(\"web\", 2)\n\nresource compute.vm \"${p}-${i}\" {\n  size = \"small\"\n} where pool(p, n), i in 0..n\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 2 changes (2 create) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("+ compute.vm web-1"), "{}", r.stdout);
    let r = s
        .run(&["why", "--tree", "compute.vm web-1", "p.df"])
        .success();
    assert!(
        r.stdout.contains("where pool(p, n), i in 0..n\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("with p = \"web\", n = 2, i = 1\n"),
        "{}",
        r.stdout
    );
}
