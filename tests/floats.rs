//! Floats (R-75): a decimal literal is a `float`, an integer literal an
//! `int`; arithmetic promotes an int to a float when the two mix, and
//! `int / int` stays an int; `int(x)` and `float(x)` convert; numbers
//! compare by value; a document's `1.5` is a float; `--set` reads by the
//! declared type; NaN and the infinities are errors where a float is read.

mod common;
use common::facts;

/// The evaluation error of the core-text program `src`.
fn eval_error(src: &str) -> String {
    let program = dform_core::parser::parse_program(src).unwrap();
    dform_core::engine::eval(&program, &[])
        .map(|_| ())
        .unwrap_err()
        .to_string()
}

/// NaN and the infinities are no float: `float("nan")`, a division by
/// zero, an overflow are a call with no value, as an int's division by
/// zero is.
#[test]
fn nan_and_infinity_are_no_value() {
    for (src, want) in [
        (
            "x(v) where v = float(\"nan\")\n",
            "float(\"nan\") is not defined",
        ),
        (
            "x(v) where v = float(\"inf\")\n",
            "float(\"inf\") is not defined",
        ),
        ("x(v) where v = 1.0 / 0\n", "div(1.0, 0) is not defined"),
        ("x(v) where v = 1 / 0\n", "div(1, 0) is not defined"),
    ] {
        let e = eval_error(src);
        assert!(e.contains(want), "{src}: {e}");
    }
}

#[test]
fn a_decimal_literal_is_a_float_and_mixed_arithmetic_promotes() {
    let src = r#"lit(x) where x = 0.5
two(x) where x = 2.0
sum(x) where x = 1 + 0.5
prod(x) where x = 3 * 0.25
idiv(x) where x = 7 / 2
fdiv(x) where x = 7 / 2.0
fmod(x) where x = 7.5 % 2
neg(x) where x = 0 - 1.25
"#;
    assert_eq!(facts(src, "lit"), ["lit(0.5)"]);
    assert_eq!(facts(src, "two"), ["two(2.0)"]);
    assert_eq!(facts(src, "sum"), ["sum(1.5)"]);
    assert_eq!(facts(src, "prod"), ["prod(0.75)"]);
    assert_eq!(facts(src, "idiv"), ["idiv(3)"]);
    assert_eq!(facts(src, "fdiv"), ["fdiv(3.5)"]);
    assert_eq!(facts(src, "fmod"), ["fmod(1.5)"]);
    assert_eq!(facts(src, "neg"), ["neg(-1.25)"]);
}

#[test]
fn int_and_float_convert_and_numbers_compare_by_value() {
    let src = r#"i(x) where x = int(2.75)
n(x) where x = int(0 - 2.75)
f(x) where x = float(2)
s(x) where x = float("1.25")
eq() where 1 == 1.0
ne() where 1 != 1.0
lt() where 1 < 1.5
gt() where 2.5 > 2
big() where 9007199254740993 > 9007199254740992.0
text(t, u) where t = "r=${0.5}", u = "${1.5}"
"#;
    assert_eq!(facts(src, "i"), ["i(2)"]);
    assert_eq!(facts(src, "n"), ["n(-2)"]);
    assert_eq!(facts(src, "f"), ["f(2.0)"]);
    assert_eq!(facts(src, "s"), ["s(1.25)"]);
    assert_eq!(facts(src, "eq"), ["eq()"]);
    assert!(facts(src, "ne").is_empty());
    assert_eq!(facts(src, "lt"), ["lt()"]);
    assert_eq!(facts(src, "gt"), ["gt()"]);
    // An int against a float is exact, past a float's 2^53.
    assert_eq!(facts(src, "big"), ["big()"]);
    assert_eq!(facts(src, "text"), [r#"text("r=0.5", "1.5")"#]);
}

#[test]
fn aggregates_over_floats() {
    let src = r#"r("a", 0.5)
r("b", 2)
r("c", 1.25)
total(x) where x = sum(v), r(_, v)
least(x) where x = min(v), r(_, v)
most(x) where x = max(v), r(_, v)
"#;
    assert_eq!(facts(src, "total"), ["total(3.75)"]);
    assert_eq!(facts(src, "least"), ["least(0.5)"]);
    assert_eq!(facts(src, "most"), ["most(2)"]);
}

mod tables_common;
use tables_common::scratch;

/// A schema attribute typed `number` takes an int or a float and sends a
/// JSON number; a string is an error naming the attribute. A cpu
/// position still reads `0.5` as `500m` (R-66).
#[test]
fn a_number_attribute_takes_an_int_or_a_float() {
    let s = scratch("number-attr");
    s.write(
        "p.df",
        "\nuse fake\n\
         resource compute.vm a { weight = 0.5 }\n\
         resource compute.vm b { weight = 2 }\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout
            .contains("  + compute.vm a  p.df:3\n      weight = 0.5\n")
            && r.stdout
                .contains("  + compute.vm b  p.df:4\n      weight = 2\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "--json", "p.df"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let after = |i: usize| j["ticks"][0]["changes"][i]["changes"][0]["after"].clone();
    assert_eq!(
        (after(0), after(1)),
        (serde_json::json!(0.5), serde_json::json!(2))
    );
    s.write(
        "p.df",
        "\nuse fake\nresource compute.vm a { weight = \"heavy\" }\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("compute.vm[\"a\"].weight is number, not the string \"heavy\""),
        "{}",
        r.stderr
    );
}

/// An input typed `float` reads an int literal as the float it names;
/// `--set` reads its text by the declared type (a `string` input keeps
/// `1.5` as text); NaN and infinity are errors at the edge.
#[test]
fn a_float_input_and_set_by_the_declared_type() {
    let s = scratch("float-input");
    s.write(
        "p.df",
        "\ninput ratio: float = 1\ninput label: string = \"x\"\nuse fake\n\
         got(r, l) where r = ratio, l = label\n",
    );
    let r = s.run(&["query", "got(R, L)", "p.df"]).success();
    assert!(r.stdout.contains("1.0"), "{}", r.stdout);
    let r = s
        .run(&[
            "query",
            "got(R, L)",
            "p.df",
            "--set",
            "ratio=0.25",
            "--set",
            "label=1.5",
        ])
        .success();
    assert!(
        r.stdout.contains("0.25") && r.stdout.contains("\"1.5\""),
        "{}",
        r.stdout
    );
    for bad in ["nan", "inf", "x"] {
        let r = s
            .run(&[
                "query",
                "got(R, L)",
                "p.df",
                "--set",
                &format!("ratio={bad}"),
            ])
            .failure();
        assert!(
            r.stderr
                .contains(&format!("--set ratio={bad}: input ratio is float")),
            "{bad}: {}",
            r.stderr
        );
    }
}

/// A document's `1.5` is a float and `2` an int; a column the first row
/// types `float` reads a later row's int as a float.
#[test]
fn a_documents_decimal_is_a_float() {
    let s = scratch("float-doc");
    s.write(
        "t.json",
        "[{\"n\": \"a\", \"r\": 1.5}, {\"n\": \"b\", \"r\": 2}]",
    );
    s.write(
        "p.df",
        "\ninput t from json.decode(io.read(\"t.json\"))\nuse fake\n\
         heavy(n) where t(n, r), r > 1.75\n",
    );
    let r = s.run(&["query", "t(N, R)", "p.df"]).success();
    assert!(
        r.stdout.contains("1.5") && r.stdout.contains("2.0"),
        "{}",
        r.stdout
    );
    let r = s.run(&["query", "heavy(N)", "p.df"]).success();
    assert!(
        r.stdout.contains("\"b\"") && !r.stdout.contains("\"a\""),
        "{}",
        r.stdout
    );
}

/// `list.sum` is a float when one element is.
#[test]
fn list_sum_promotes() {
    let src = "s(x) where x = list.sum([1, 0.5])\ni(x) where x = list.sum([1, 2])\n";
    assert_eq!(facts(src, "s"), ["s(1.5)"]);
    assert_eq!(facts(src, "i"), ["i(3)"]);
}
