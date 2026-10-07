//! A chain continues after a call (R-71): `f(x).p`, `f(x)[i]`,
//! `f(x).p[k].q`, in every term position, `has` and `not` included. It
//! lowers as the rebinding does: a variable bound to the call, then the
//! read.

mod common;
use common::{error, facts};

#[test]
fn a_field_of_a_call() {
    let src = r#"image("ghcr.io/o/app:1.2")
image("ghcr.io/o/app")
tag(i, t) where image(i), t = oci.with_registry(i, "r.example").tag
"#;
    assert_eq!(facts(src, "tag"), [r#"tag("ghcr.io/o/app:1.2", "1.2")"#]);
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
    assert_eq!(
        facts(src, "second"),
        [r#"second("{\"a\": {\"b\": [{\"c\": 1}, {\"c\": 2}]}}", 2)"#]
    );
}

#[test]
fn has_and_not_has_a_field_of_a_call() {
    let src = r#"image("ghcr.io/o/app:1.2")
image("ghcr.io/o/app")
tagged(i) where image(i), has oci.with_registry(i, "r.example").tag
untagged(i) where image(i), not has oci.with_registry(i, "r.example").tag
"#;
    assert_eq!(facts(src, "tagged"), [r#"tagged("ghcr.io/o/app:1.2")"#]);
    assert_eq!(facts(src, "untagged"), [r#"untagged("ghcr.io/o/app")"#]);
}

/// The chain reads what the rebinding reads: one rule written both ways
/// derives the same rows.
#[test]
fn a_chain_after_a_call_is_the_rebinding() {
    let src = r#"net("10.0.0.0/8")
net("192.168.0.0/16")
a(n, m) where net(n), m = inet.subnet(n, 8, 1).bits
b(n, m) where net(n), p = inet.subnet(n, 8, 1), m = p.bits
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

/// A partial call with no answer under `has` (R-134, the R-133
/// carry-over): `has f(x).p` is false and `not has f(x).p` holds, never
/// "not defined for these arguments"; a total function's bad input is an
/// error under `has` as anywhere.
#[test]
fn has_of_a_call_with_no_answer_is_false() {
    let src = r#"list([])
list([{ "x": 1 }])
some(l) where list(l), has list.first(l).x
none(l) where list(l), not has list.first(l).x
"#;
    assert_eq!(facts(src, "some"), [r#"some([{x: 1}])"#]);
    assert_eq!(facts(src, "none"), ["none([])"]);
    let program = dform_core::parser::parse_program(
        "img(\"nginx\")\np(i) where img(i), not has oci.with_tag(i, \"no tag!\").digest\n",
    )
    .unwrap();
    let err = dform_core::engine::eval(&program, &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not defined for these arguments"), "{err}");
}
