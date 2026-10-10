//! `range(T)` for every ordered type (R-180): `a..b` leaves its end out,
//! `a..=b` takes it; `x in r` with `x` bound tests `start <= x <= end`
//! for every ordered type, and with `x` unbound enumerates a discrete
//! range (an int's, an ip's) and is an error for a dense one, naming
//! the fix; `r.start`, `r.end`, and a discrete range's `r.len`; the
//! canonical print `a..=b`, which a `range(T)` position parses back.

mod common;
use common::{error, facts};
use dform_core::{engine, parser::parse_program};

/// The error evaluating `src` gives.
fn eval_error(src: &str) -> String {
    let program = parse_program(src).unwrap_or_else(|e| panic!("{e}"));
    format!("{:#}", engine::eval(&program, &[]).unwrap_err())
}

#[test]
fn an_int_range_enumerates_and_tests() {
    assert_eq!(
        facts("p(i) where i in 0..3\n", "p"),
        ["p(0)", "p(1)", "p(2)"]
    );
    assert_eq!(
        facts("q(0)\nq(3)\nq(4)\np(x) where q(x), x in 1..=3\n", "p"),
        ["p(3)"]
    );
    assert_eq!(
        facts("q(2)\nq(3)\np(x) where q(x), x in 1..3\n", "p"),
        ["p(2)"]
    );
}

/// A range is a value: it is printed canonically, its end in it for a
/// discrete type, and its parts are fields.
#[test]
fn a_range_is_a_value_with_fields() {
    assert_eq!(facts("r(0..3)\n", "r"), ["r(0..=2)"]);
    assert_eq!(
        facts(
            "r(0..3)\np(a, b, n) where r(x), a = x.start, b = x.end, n = x.len\n",
            "p"
        ),
        ["p(0, 2, 3)"]
    );
    assert_eq!(
        facts("let k = 4\nr(1..=k)\n", "r"),
        ["r(1..=4)"],
        "a computed end"
    );
    assert_eq!(
        facts(
            "r(1Gi..=500Gi)\np(a, b) where r(x), a = x.start, b = x.end\n",
            "p"
        ),
        ["p(1Gi, 500Gi)"]
    );
}

#[test]
fn an_ip_range_enumerates_and_tests() {
    assert_eq!(
        facts("p(a) where a in \"10.0.0.1\"..=\"10.0.0.3\"\n", "p"),
        ["p(10.0.0.1)", "p(10.0.0.2)", "p(10.0.0.3)"]
    );
    let src = "let r: range(ip) = \"10.0.0.10..=10.0.0.20\"\n\
               n(r.len)\n\
               inside() where \"10.0.0.20\" in r\n\
               outside() where not \"10.0.0.21\" in r\n\
               each(a) where a in r, a > \"10.0.0.18\"\n";
    assert_eq!(facts(src, "n"), ["n(11)"]);
    assert_eq!(facts(src, "inside"), ["inside()"]);
    assert_eq!(facts(src, "outside"), ["outside()"]);
    assert_eq!(facts(src, "each"), ["each(10.0.0.19)", "each(10.0.0.20)"]);
}

#[test]
fn a_bytes_range_tests_a_bound_value() {
    assert_eq!(
        facts(
            "q(512Mi)\nq(2Gi)\nq(600Gi)\np(x) where q(x), x in 1Gi..=500Gi\n",
            "p"
        ),
        ["p(2Gi)"]
    );
    // `100m..=1`: the one dimension that reads both ends, cpu's.
    let src = "let a: cpu = 50m\nlet b: cpu = 500m\n\
               low() where a in 100m..=1\nmid() where b in 100m..=1\n";
    assert!(facts(src, "low").is_empty());
    assert_eq!(facts(src, "mid"), ["mid()"]);
}

#[test]
fn a_version_range_tests_a_bound_version() {
    let src = "decl q(v: semver)\nq(\"1.1.0\")\nq(\"1.5.0\")\nq(\"2.0.0\")\n\
               p(v) where q(v), v in \"1.2.0\"..\"2.0.0\"\n";
    assert_eq!(facts(src, "p"), ["p(1.5.0)"]);
}

/// Generation needs a next member: a dense range with its variable
/// unbound is a compile error with the fix at the site.
#[test]
fn a_dense_range_enumerates_none() {
    let e = error("p(n) where n in 1Gi..=500Gi\n");
    assert!(
        e.contains("`n` is unbound at this `in`: a range of bytes has no next member"),
        "{e}"
    );
    assert!(
        e.contains("enumerate ints and scale them: `i in 1..=500, n = i * 1Gi`"),
        "{e}"
    );
    let e = error("p(v) where v in \"1.2.0\"..\"2.0.0\"\n");
    assert!(
        e.contains("`v` is unbound at this `in`: a range of semver has no next member"),
        "{e}"
    );
    assert!(
        e.contains("bind `v` first, and `v in \"1.2.0\"..\"2.0.0\"` tests it"),
        "{e}"
    );
    // A computed dense range says so when it is evaluated.
    let e = eval_error("lo(1Gi)\np(n) where lo(a), n in a..=500Gi\n");
    assert!(e.contains("has no next member"), "{e}");
    assert!(e.contains("`n in 1..=500, x = n * 1Gi`"), "{e}");
}

/// A range's ends are of one ordered type.
#[test]
fn a_range_has_ends_of_one_ordered_type() {
    let e = error("r(1Gi..=\"x\")\n");
    assert!(e.contains("`1Gi..=\"x\"` is no range"), "{e}");
    let e = error("r(true..false)\n");
    assert!(e.contains("which have no order"), "{e}");
    let e = error("let r: range(string) = \"a..b\"\n");
    assert!(e.contains("`range(string)` is no range type"), "{e}");
}

/// `iprange` is `range(ip)`: the old name is an error naming the new one,
/// and the old text form is an error naming the canonical one.
#[test]
fn iprange_is_range_of_ip() {
    let e = error("let r: iprange = \"10.0.0.1..=10.0.0.9\"\n");
    assert!(e.contains("unknown type iprange"), "{e}");
    assert!(e.contains("a range of addresses is `range(ip)`"), "{e}");
    let e = error("let r: range(ip) = \"10.0.0.1-10.0.0.9\"\n");
    assert!(
        e.contains("a range of addresses is written `10.0.0.1..=10.0.0.9`"),
        "{e}"
    );
}

/// `check storage in 1Gi..=500Gi`: an input's refinement over a range.
#[test]
fn an_input_is_checked_against_a_range() {
    let s = common::Scratch::project("range-check");
    s.write(
        "p.df",
        "\ninput storage: bytes = 10Gi check storage in 1Gi..=500Gi\nresource net.vpc main {\n  storage = s\n} where storage(s)\nuse fake\n",
    );
    let plan = |extra: &[&str]| {
        let mut a = vec!["plan", "--why=none"];
        a.extend(extra);
        s.run(&common::on("p.df", &["--world", "w.json"], &a))
    };
    let r = plan(&[]).success();
    assert!(r.stdout.contains("storage = \"10Gi\""), "{}", r.stdout);
    let r = plan(&["--set", "storage=600Gi"]).failure();
    assert!(
        r.stderr
            .contains("error  --set storage=600Gi is outside the check on storage\n"),
        "{}",
        r.stderr
    );
}

/// `input pool: range(ip)`: a range input given by `--set` and by a
/// document, its text read as the range a literal is.
#[test]
fn an_input_is_a_range() {
    let s = common::Scratch::project("range-input");
    s.write("pool.yaml", "pool: 10.0.0.2..=10.0.0.9\n");
    s.write(
        "p.df",
        "\ninput pool: range(ip)\nset from yaml.decode(io.read(\"pool.yaml\"))\n\
         resource net.vpc main {\n  pool = p\n  size = p.len\n} where pool(p)\nuse fake\n",
    );
    let plan = |extra: &[&str]| {
        let mut a = vec!["plan", "--why=none"];
        a.extend(extra);
        s.run(&common::on("p.df", &["--world", "w.json"], &a))
    };
    let r = plan(&[]).success();
    assert!(
        r.stdout.contains("pool = \"10.0.0.2..=10.0.0.9\""),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("size = 8"), "{}", r.stdout);
    let r = plan(&["--set", "pool=10.0.0.2..=10.0.0.3"]).success();
    assert!(r.stdout.contains("size = 2"), "{}", r.stdout);
    let r = plan(&["--set", "pool=10.0.0.2-10.0.0.3"]).failure();
    assert!(
        r.stderr.contains(
            "input pool is range(ip): \"10.0.0.2-10.0.0.3\" is not a range: a range of \
             addresses is written `10.0.0.2..=10.0.0.3`"
        ),
        "{}",
        r.stderr
    );
}
