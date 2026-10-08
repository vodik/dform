//! Spread in literals (R-199): `..x` leads an object's field or a list's
//! element. `{ ..base, k: v }` takes base's fields then the written ones,
//! the last by position winning; `[..a, ..b]` concatenates; `[..0..3]`
//! enumerates a discrete range. No deep merge: depth is written, `{ ..b,
//! spec: { ..b.spec, replicas: 3 } }`.

mod common;
use common::{Scratch, error, facts};

const BASE: &str = "base({a: 1, b: 2, spec: {replicas: 1, image: \"x\"}})\n";

#[test]
fn an_object_takes_the_fields_then_the_written_ones_last_wins() {
    let p = |body: &str| facts(&format!("{BASE}p(x) where base(b), x = {body}\n"), "p");
    assert_eq!(
        p("{ ..b, b: 3, c: 4 }"),
        ["p({a: 1, b: 3, c: 4, spec: {image: \"x\", replicas: 1}})"]
    );
    // Before the spread, a written key is a default the spread replaces.
    assert_eq!(
        p("{ b: 3, z: 0, ..b }"),
        ["p({a: 1, b: 2, spec: {image: \"x\", replicas: 1}, z: 0})"]
    );
    // No deep merge: a nested object written replaces, depth is spelled.
    assert_eq!(
        p("{ ..b, spec: { replicas: 3 } }"),
        ["p({a: 1, b: 2, spec: {replicas: 3}})"]
    );
    assert_eq!(
        p("{ ..b, spec: { ..b.spec, replicas: 3 } }"),
        ["p({a: 1, b: 2, spec: {image: \"x\", replicas: 3}})"]
    );
    assert_eq!(
        facts("p(x) where x = { ..{a: 1}, b: 2 }\n", "p"),
        ["p({a: 1, b: 2})"]
    );
}

#[test]
fn a_list_concatenates_and_a_discrete_range_enumerates() {
    assert_eq!(
        facts("xs([1, 2])\np(x) where xs(a), x = [..a, 3, ..a]\n", "p"),
        ["p([1, 2, 3, 1, 2])"]
    );
    assert_eq!(facts("p(x) where x = [..0..3]\n", "p"), ["p([0, 1, 2])"]);
    assert_eq!(
        facts("p(x) where x = [..1..=3, 9]\n", "p"),
        ["p([1, 2, 3, 9])"]
    );
    // Ends known only when it runs.
    assert_eq!(
        facts("n(3)\np(x) where n(k), x = [..0..k]\n", "p"),
        ["p([0, 1, 2])"]
    );
    assert_eq!(
        facts("p(x) where x = [..\"10.0.0.1\"..=\"10.0.0.2\"]\n", "p"),
        ["p([10.0.0.1, 10.0.0.2])"]
    );
    // A range without the dots is one element, as before.
    assert_eq!(facts("p(x) where x = [0..3]\n", "p"), ["p([0..=2])"]);
}

#[test]
fn a_spread_of_the_wrong_kind_is_an_error_where_it_is_known() {
    let e = error("p(x) where x = { ..[1], b: 2 }\n");
    assert!(
        e.contains(
            "t.df:2:18: `..[1]` spreads a list into an object, which takes an object's fields\n  \
             help: a list spreads into a list, `[..[1]]`"
        ),
        "{e}"
    );
    let e = error("p(x) where x = [..{a: 1}]\n");
    assert!(
        e.contains(
            "`..{a: 1}` spreads an object into a list, which takes a list's elements\n  \
             help: an object spreads into an object, `{ ..{a: 1} }`"
        ),
        "{e}"
    );
    let e = error("p(x) where x = [..str.lower(\"A\")]\n");
    assert!(
        e.contains("`..str.lower(\"A\")` spreads a string into a list")
            && e.contains("help: an element is written without the dots, `[str.lower(\"A\")]`"),
        "{e}"
    );
    // A dense range has no next member: R-180's error.
    let e = error("p(x) where x = [..0.5..1.5]\n");
    assert!(
        e.contains("`..0.5..1.5`: 0.5..1.5 is a range of floats, which has no next member"),
        "{e}"
    );
    let e = error("p(x) where x = { .. }\n");
    assert!(
        e.contains("`..` here spreads nothing: a spread names the value it spreads"),
        "{e}"
    );
    // A call takes its arguments one by one.
    let e = error("p(x) where y = [1], x = list.sort(..y)\n");
    assert!(
        e.contains(
            "`..y` is a spread: it leads a field of an object or an element of a list\n  \
             help: write the value it makes, `{ ..y }` or `[..y]`"
        ),
        "{e}"
    );
}

/// A key written twice in one literal is an error naming both; a key
/// after a spread is its override, not a second write.
#[test]
fn a_key_written_twice_names_both() {
    let e = error("p(x) where x = { a: 1, ..{b: 1}, a: 2 }\n");
    assert!(
        e.contains("t.df:2:34: key `a` given twice\n  t.df:2:18: `a` is first given here"),
        "{e}"
    );
}

/// A source known only at run time: an error at the statement, naming
/// what the spread gave.
#[test]
fn a_spread_of_the_wrong_kind_at_run_time_is_located() {
    let s = Scratch::project("spread-runtime");
    s.write(
        "stacks/lab.df",
        "use fake\nlet base = [\"web\"]\nresource compute.vm a {\n  size = \"small\"\n  labels = { ..base, tier: \"back\" }\n}\n",
    );
    let r = s.run(&["plan", "lab"]).failure();
    assert!(
        r.stderr.contains(
            "stacks/lab.df:5:3: a spread `..` in an object gives a list `[\"web\"]`, and an \
             object takes an object's fields, so compute.vm a.labels has no value"
        ),
        "{}",
        r.stderr
    );
}

/// `why` of a field that came through a spread: the literal's site, and
/// under it the source's.
#[test]
fn why_shows_the_spread_and_its_source() {
    let s = Scratch::project("spread-why");
    s.write(
        "stacks/lab.df",
        "use fake\n\nlet base = { team: \"web\", tier: \"front\" }\n\nresource compute.vm a {\n  size = \"small\"\n  labels = { ..base, tier: \"back\" }\n}\n",
    );
    let r = s.run(&["plan", "lab"]).success();
    assert!(
        r.stdout
            .contains("labels = { team: \"web\", tier: \"back\" }"),
        "{}",
        r.stdout
    );
    let r = s
        .run(&["why", "--tree", "compute.vm a.labels", "lab"])
        .success();
    for line in [
        "stacks/lab.df:7  resource compute.vm a { .. labels = { ..base, tier: \"back\" } }",
        "with base = {team: \"web\", tier: \"front\"}",
        "└─ let base = {team: \"web\", tier: \"front\"}",
        "stacks/lab.df:3",
    ] {
        assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
    }
}

/// An unknown source: the literal's fields are unknown until the source
/// is, so its resource waits for the tick that gives it; an unknown inside
/// a source is carried, as an unknown term inside a literal is (R-183).
#[test]
fn an_unknown_source_waits() {
    let s = Scratch::project("spread-unknown");
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df"))
            .unwrap()
            + "type_provider(db.volume, \"fakecloud\")\n\
               type_attr(db.volume, \"id\", \"string\", [\"computed\", \"id\"])\n\
               type_attr(db.volume, \"size\", \"int\", [])\n\
               type_attr(db.volume, \"status\", \"map(string)\", [\"computed\"])\n"),
    );
    s.write(
        "stacks/lab.df",
        "use fake\nresource db.volume v { size = 1 }\nresource compute.vm a {\n  size = \"small\"\n  labels = { ..v.status, tier: \"back\" }\n}\n",
    );
    let r = s.run(&["plan", "lab"]).success();
    assert!(
        r.stdout
            .contains("tick 2  1 change\n  waits on  v.status\n  + compute.vm a"),
        "{}",
        r.stdout
    );
    s.run(&["apply", "lab"]).success();
    let r = s.run(&["plan", "lab"]).success();
    assert_eq!(r.summary(), "stack lab is up to date", "{}", r.stdout);
}

/// `set r.spec = { ..r.spec, x: 1 }` makes a value from itself: the cycle
/// error, with the fix a write of each key it adds.
#[test]
fn a_spread_of_what_it_writes_is_the_cycle_with_its_fix() {
    let s = Scratch::project("spread-cycle");
    s.write(
        "stacks/lab.df",
        "use fake\nresource compute.vm a {\n  size = \"small\"\n  labels = { x: \"1\" }\n}\nset a.labels = { ..a.labels, tier: \"back\" } where a.size == \"small\"\n",
    );
    let r = s.run(&["plan", "lab"]).failure();
    assert!(
        r.stderr.contains(
            "help: compute.vm a.labels spreads its own value, which it has only once it is \
             given: write what it adds, `set a.labels.tier = \"back\"`"
        ),
        "{}",
        r.stderr
    );
}
