//! A chain continues after a call (R-71): `f(x).p`, `f(x)[i]`,
//! `f(x).p[k].q`, in every term position, `has` and `not` included. It
//! lowers as the rebinding does: a variable bound to the call, then the
//! read.

mod common;
use dform_core::engine;
use dform_core::parser::{parse_file, parse_program};
use dform_core::partition::fmt_atom;

fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = parse_program(src).unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(fmt_atom)
        .collect()
}

fn error(src: &str) -> String {
    parse_file("t.df", &format!("edition 2026\n{src}"))
        .map(|_| ())
        .unwrap_err()
        .to_string()
}

#[test]
fn a_field_of_a_call() {
    let src = r#"image("gcr.io/shop/api@sha256:9f2c")
image("gcr.io/shop/api:v1")
digest(i, d) where image(i), d = oci.parse(i).digest
"#;
    assert_eq!(
        facts(src, "digest"),
        [r#"digest("gcr.io/shop/api@sha256:9f2c", "sha256:9f2c")"#]
    );
}

#[test]
fn an_index_of_a_call() {
    let src = r#"image("api:v1")
repo(i, r) where image(i), r = str.split(i, ":")[0]
tag(i) where image(i), str.split(i, ":")[1] == "v1"
"#;
    assert_eq!(facts(src, "repo"), [r#"repo("api:v1", "api")"#]);
    assert_eq!(facts(src, "tag"), [r#"tag("api:v1")"#]);
}

#[test]
fn a_nested_chain_after_a_call() {
    let src = r#"doc("{\"a\": {\"b\": [{\"c\": 1}, {\"c\": 2}]}}")
second(d, v) where doc(d), v = json.decode(d).a.b[1].c
"#;
    assert_eq!(facts(src, "second"), [r#"second("{\"a\": {\"b\": [{\"c\": 1}, {\"c\": 2}]}}", 2)"#]);
}

#[test]
fn has_and_not_has_a_field_of_a_call() {
    let src = r#"image("gcr.io/shop/api@sha256:9f2c")
image("gcr.io/shop/api:v1")
pinned(i) where image(i), has oci.parse(i).digest
unpinned(i) where image(i), not has oci.parse(i).digest
"#;
    assert_eq!(
        facts(src, "pinned"),
        [r#"pinned("gcr.io/shop/api@sha256:9f2c")"#]
    );
    assert_eq!(facts(src, "unpinned"), [r#"unpinned("gcr.io/shop/api:v1")"#]);
    // Under `not` the call is inside what is negated: a text that is no
    // reference has no digest.
    let src = r#"image("Not A Reference")
unpinned(i) where image(i), not has oci.parse(i).digest
"#;
    assert_eq!(facts(src, "unpinned"), [r#"unpinned("Not A Reference")"#]);
}

/// The chain reads what the rebinding reads: one rule written both ways
/// derives the same rows.
#[test]
fn a_chain_after_a_call_is_the_rebinding() {
    let src = r#"image("gcr.io/shop/api@sha256:9f2c")
image("gcr.io/shop/api:v1")
a(i, r) where image(i), r = oci.parse(i).repository
b(i, r) where image(i), p = oci.parse(i), r = p.repository
"#;
    let a: Vec<String> = facts(src, "a").iter().map(|f| f[1..].to_string()).collect();
    let b: Vec<String> = facts(src, "b").iter().map(|f| f[1..].to_string()).collect();
    assert_eq!(a.len(), 2);
    assert_eq!(a, b);
}

/// A truth test and a value: `f(x).p` alone, and in a field.
#[test]
fn a_truth_test_and_a_value() {
    let src = r#"doc("{\"on\": true, \"n\": 3}")
doc("{\"on\": false, \"n\": 4}")
on(n) where doc(d), json.decode(d).on, n = json.decode(d).n
"#;
    assert_eq!(facts(src, "on"), ["on(3)"]);
}

#[test]
fn a_call_after_a_call_is_not_a_function() {
    let e = error("p(x) where q(y), x = str.split(y, \":\").first(1)\nq(\"a\")\n");
    assert!(e.contains("a function is named by a plain name"), "{e}");
    let e = error("p(x) where q(y), x = str.split(y, \":\")(1)\nq(\"a\")\n");
    assert!(e.contains("a function is named by a plain name"), "{e}");
}
