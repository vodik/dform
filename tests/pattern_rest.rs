//! An object pattern's rest (R-199): `..name` as the last entry binds the
//! object without the keys the pattern names, the same `..` a literal
//! spreads with, so `{ metadata: m, ..body } = doc` takes a document apart
//! and `{ ..body, metadata: m2 }` puts it back together.

mod common;
use common::{Scratch, error, facts};

const DOC: &str = "doc({metadata: {name: \"a\"}, kind: \"Pod\", spec: {replicas: 1}})\n";

#[test]
fn the_rest_binds_the_fields_the_pattern_does_not_name() {
    assert_eq!(
        facts(
            &format!("{DOC}p(m, body) where doc(d), {{ metadata: m, ..body }} = d\n"),
            "p"
        ),
        ["p({name: \"a\"}, {kind: \"Pod\", spec: {replicas: 1}})"]
    );
    assert_eq!(
        facts(
            &format!(
                "{DOC}p(k, body) where doc(d), {{ kind: k, metadata, ..body }} = d, metadata.name == \"a\"\n"
            ),
            "p"
        ),
        ["p(\"Pod\", {spec: {replicas: 1}})"]
    );
    // The rest alone is the whole object.
    assert_eq!(
        facts("doc({a: 1})\np(x) where doc(d), { ..x } = d\n", "p"),
        ["p({a: 1})"]
    );
    // Taken apart and put back together, with one field replaced.
    assert_eq!(
        facts(
            &format!(
                "{DOC}p(x) where doc(d), {{ metadata: m, ..body }} = d, x = {{ ..body, metadata: {{ ..m, name: \"b\" }} }}\n"
            ),
            "p"
        ),
        ["p({kind: \"Pod\", metadata: {name: \"b\"}, spec: {replicas: 1}})"]
    );
}

/// A pattern is a test: a value that is not an object, or one without a
/// named field, does not match.
#[test]
fn the_rest_matches_only_an_object() {
    assert!(facts("doc([1])\np(x) where doc(d), { ..x } = d\n", "p").is_empty());
    assert!(facts("doc({b: 1})\np(x) where doc(d), { a: _a, ..x } = d\n", "p").is_empty());
}

#[test]
fn the_rest_is_a_name_and_comes_last() {
    let e = error("doc({a: 1})\np(x) where doc(d), { a: y, .. } = d, x = y\n");
    assert!(
        e.contains(
            "`..` in `{ a: y, .. }` ignores the rest, which an object pattern does already\n  \
             help: leave it out, `{ a: y }`; to bind the rest, name it, `..rest`"
        ),
        "{e}"
    );
    let e = error("doc({a: 1})\np(x, y) where doc(d), { ..x, a: y } = d\n");
    assert!(
        e.contains(
            "`a: y` follows the rest `..x`: the rest is a pattern's last entry\n  \
             help: move `..x` to the end of the pattern"
        ),
        "{e}"
    );
    let e = error("doc({a: {b: 1}})\np(x) where doc(d), { ..{ b: x } } = d\n");
    assert!(
        e.contains("`..{ b: x }` in an object pattern binds the rest to a name"),
        "{e}"
    );
}

/// A tuple matches a list of exactly its arity; a list is walked with
/// `in`.
#[test]
fn a_tuple_has_no_rest() {
    let e = error("xs([1, 2, 3])\np(x) where xs(l), (a, ..x) = l, a == 1\n");
    assert!(
        e.contains(
            "`..x` in a tuple pattern: a tuple matches a list of exactly as many elements as it names\n  \
             help: a list's elements are each `(i, x) in xs`, its first `xs[0]`"
        ),
        "{e}"
    );
}

/// `why` prints the pattern as written and the rest's value.
#[test]
fn why_shows_the_rest_as_written() {
    let s = Scratch::project("pattern-rest-why");
    s.write(
        "stacks/lab.df",
        "use fake\n\ndecl doc(d)\ndoc({ size: \"small\", labels: { team: \"web\" } })\n\nresource compute.vm \"a\" {\n  size = size\n  labels = labels\n} where doc(d), { size, ..rest } = d, { labels } = rest\n",
    );
    let r = s.run(&["plan", "lab"]).success();
    assert!(r.stdout.contains("labels.team = \"web\""), "{}", r.stdout);
    let r = s
        .run(&["why", "--tree", "compute.vm a.labels", "lab"])
        .success();
    for line in [
        "where doc(d), { size, ..rest } = d, { labels } = rest",
        "rest = {labels: {team: \"web\"}}",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
}
