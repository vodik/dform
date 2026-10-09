//! A negated membership is the membership's negation (R-219): `v not in
//! PATH[_]` holds when no value the path reaches is `v`, and `r not in T`,
//! `r` a reference column's (R-42), when the reference is of another
//! type. `not x in e` is the same literal.

mod common;

/// The rows of `pred` the program file `src` derives.
fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = dform_core::parser::parse_file("t.df", src).unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = dform_core::engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(dform_core::spell::atom)
        .collect()
}

const TAGS: &str = "resource net.vpc a { tags = [\"x\", \"y\"] }\n\
    resource net.vpc b { tags = [\"y\"] }\n\
    c(\"x\")\nc(\"z\")\n";

/// `[_]` over every resource of a type and every element of its list
/// (R-162): `"z"` is no vpc's tag, `"x"` is one.
#[test]
fn not_in_a_wildcard_path_is_no_value_it_reaches() {
    for out in [
        "out(t) where c(t), t not in net.vpc[_].tags[_]",
        "out(t) where c(t), not t in net.vpc[_].tags[_]",
        "out(t) where c(t), not { t in net.vpc[_].tags[_] }",
    ] {
        assert_eq!(
            facts(&format!("{TAGS}{out}\n"), "out"),
            [r#"out("z")"#],
            "{out}"
        );
    }
    let out = "out(t) where c(t), t in net.vpc[_].tags[_]";
    assert_eq!(facts(&format!("{TAGS}{out}\n"), "out"), [r#"out("x")"#]);
}

/// The same over a value's list: `4` is no element of `xs`.
#[test]
fn not_in_a_wildcard_value_path_is_no_element() {
    let src = "let xs = [1, 2, 3]\nc(1)\nc(4)\n";
    for out in [
        "out(v) where c(v), v not in xs[_]",
        "out(v) where c(v), not v in xs[_]",
    ] {
        assert_eq!(facts(&format!("{src}{out}\n"), "out"), ["out(4)"], "{out}");
    }
}

const REFS: &str = "resource net.vpc a { cidr = \"10.0.0.0/16\" }\n\
    resource net.subnet b { cidr = \"10.0.1.0/24\" }\n\
    lifecycle(a, \"keep\")\nlifecycle(b, \"keep\")\n";

/// `r` is the reference column of `lifecycle(r, _)`: `r not in net.vpc`
/// is the subnet's row, `r in net.vpc` the vpc's.
#[test]
fn not_in_a_type_of_a_reference_column_is_another_type() {
    for out in [
        "out(r) where lifecycle(r, _), r not in net.vpc",
        "out(r) where lifecycle(r, _), not r in net.vpc",
        "out(r) where r not in net.vpc, lifecycle(r, _)",
    ] {
        assert_eq!(
            facts(&format!("{REFS}{out}\n"), "out"),
            ["out(ref(net.subnet, b, ))"],
            "{out}"
        );
    }
    let out = "out(r) where lifecycle(r, _), r in net.vpc";
    assert_eq!(
        facts(&format!("{REFS}{out}\n"), "out"),
        ["out(ref(net.vpc, a, ))"]
    );
}
