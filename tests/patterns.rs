//! Patterns (R-58): one production, used on the left of `in` (`(k, v) in
//! obj`, `(i, x) in list`), on the left of `=` (`(a, b) = pair`, `{ host,
//! port } = conn`) and as a relation's argument (`zone({ name, index })`).
//! A tuple needs the exact arity, an object pattern binds the fields it
//! names and ignores the rest, `_` matches anything.

mod common;
use common::Scratch;
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
fn a_tuple_in_an_object_binds_each_key_and_value() {
    let src = r#"labels({ app: "web", team: "core" })
label(k, v) where labels(l), (k, v) in l
team(v) where labels(l), ("team", v) in l
keys(k) where labels(l), (k, _) in l
"#;
    assert_eq!(
        facts(src, "label"),
        [r#"label("app", "web")"#, r#"label("team", "core")"#]
    );
    assert_eq!(facts(src, "team"), [r#"team("core")"#]);
    assert_eq!(facts(src, "keys"), [r#"keys("app")"#, r#"keys("team")"#]);
}

#[test]
fn a_tuple_in_a_list_binds_each_index_and_element() {
    let src = r#"zones(["b", "a"])
zone(i, z) where zones(zs), (i, z) in zs
"#;
    assert_eq!(facts(src, "zone"), [r#"zone(0, "b")"#, r#"zone(1, "a")"#]);
}

/// Patterns nest: a tuple in a tuple, an object in a tuple.
#[test]
fn patterns_nest() {
    let src = r#"pairs([[1, "a"], [2, "b"]])
second(i, x) where pairs(ps), (i, (n, x)) in ps, n > 1
nets({ prod: { cidr: "10.0.0.0/16", zone: "a" }, dev: { cidr: "10.1.0.0/16" } })
cidr(e, c) where nets(m), (e, { cidr: c }) in m
zoned(e, z) where nets(m), (e, { zone: z }) in m
"#;
    assert_eq!(facts(src, "second"), [r#"second(1, "b")"#]);
    assert_eq!(
        facts(src, "cidr"),
        [
            r#"cidr("dev", "10.1.0.0/16")"#,
            r#"cidr("prod", "10.0.0.0/16")"#
        ]
    );
    // A field the pattern names and the object lacks: no match.
    assert_eq!(facts(src, "zoned"), [r#"zoned("prod", "a")"#]);
}

#[test]
fn not_a_tuple_in_means_no_entry_matches() {
    let src = r#"labels({ app: "web" })
labels({ app: "db", tier: "data" })
untiered(l) where labels(l), not ("tier", _) in l
"#;
    assert_eq!(facts(src, "untiered"), [r#"untiered({app: "web"})"#]);
}

/// What lowering a program file reports.
fn lower_error(src: &str) -> String {
    let p = parse_file("t.df", &format!("edition 2026\n{src}")).unwrap_or_else(|e| panic!("{e:#}"));
    match dform_core::transform::lower(&p) {
        Ok(_) => panic!("lowers: {src}"),
        Err(e) => format!("{e:#}"),
    }
}

/// `x in obj` is an error that names the pattern, at compile time when the
/// program's objects say so; a tuple after `in` has two parts; a resource
/// type is enumerated by a name.
#[test]
fn an_object_is_entered_by_a_pattern() {
    let e = lower_error("labels({ app: \"web\" })\nbad(x) where labels(l), x in l\n");
    assert!(
        e.contains("`x in l`: `l` is an object, and an object's entries are matched by a pattern"),
        "{e}"
    );
    assert!(e.contains("`(k, v) in l` takes each key and value"), "{e}");
    let e = lower_error("bad(x) where x in { a: 1 }\n");
    assert!(e.contains("`{..}` is an object"), "{e}");
    let e = lower_error("bad(x) where o = oci.parse(\"a:b\"), x in o\n");
    assert!(e.contains("`o` is an object"), "{e}");
    // A column of lists and objects is decided row by row, as before.
    let program = parse_program(
        "labels({ app: \"web\" })\nlabels([\"a\"])\nbad(x) where labels(l), x in l\n",
    )
    .unwrap();
    let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
    assert!(e.contains("`x in e` over an object, {app: \"web\"}"), "{e}");
    assert!(e.contains("`(key, value) in e`"), "{e}");
    let e = error("pairs([1])\nbad(a) where pairs(ps), (a, b, c) in ps\n");
    assert!(
        e.contains("`(a, b, c)` has 3 parts: after `in`, a tuple is `(key, value)`"),
        "{e}"
    );
    let e = error("bad(a) where (a, b) in net.vpc\n");
    assert!(
        e.contains("`(a, b)` is a pattern: a resource is enumerated by a name"),
        "{e}"
    );
}

#[test]
fn a_tuple_on_the_left_of_eq_needs_the_arity() {
    let src = r#"pair([1, 2])
pair([1, 2, 3])
two(a, b) where pair(p), (a, b) = p
"#;
    assert_eq!(facts(src, "two"), ["two(1, 2)"]);
}

#[test]
fn an_object_pattern_binds_the_named_fields_and_ignores_the_rest() {
    let src = r#"conn({ host: "db", port: 5432, user: "app" })
conn({ host: "cache" })
hp(h, p) where conn(c), { host: h, port: p } = c
host(host) where conn(c), { host } = c
"#;
    assert_eq!(facts(src, "hp"), [r#"hp("db", 5432)"#]);
    assert_eq!(facts(src, "host"), [r#"host("cache")"#, r#"host("db")"#]);
}

/// A pattern against a function's result: `(repo, tag) = str.split(i,
/// ":", 1)` binds when there is a tag and fails otherwise, so its `not`
/// reads "no tag"; in a `let`'s clause too.
#[test]
fn a_pattern_matches_a_functions_result() {
    let src = r#"image("nginx:1.25")
image("redis")
image("registry:5000/app:v2")
tagged(repo, tag) where image(i), (repo, tag) = str.split(i, ":", 1)
untagged(i) where image(i), not (_, _) = str.split(i, ":", 1)
let first_tag = t where image("nginx:1.25"), (_, t) = str.split("nginx:1.25", ":", 1)
seen(t) where t = first_tag
"#;
    assert_eq!(
        facts(src, "tagged"),
        [
            r#"tagged("nginx", "1.25")"#,
            r#"tagged("registry", "5000/app:v2")"#
        ]
    );
    assert_eq!(facts(src, "untagged"), [r#"untagged("redis")"#]);
    assert_eq!(facts(src, "seen"), [r#"seen("1.25")"#]);
}

/// A relation of named columns takes one object pattern: the record
/// pattern of the columns it names.
#[test]
fn a_relation_argument_is_a_pattern() {
    let src = r#"decl zone(name, index)
zone("a", 0)
zone("b", 1)
names(name) where zone({ name })
late(name, index) where zone({ name, index }), index > 0
at(n) where zone({ name: n, index: 1 })
pair([1, "x"])
pair([2, "y"])
second(s) where pair((2, s))
"#;
    assert_eq!(facts(src, "names"), [r#"names("a")"#, r#"names("b")"#]);
    assert_eq!(facts(src, "late"), [r#"late("b", 1)"#]);
    assert_eq!(facts(src, "at"), [r#"at("b")"#]);
    assert_eq!(facts(src, "second"), [r#"second("y")"#]);
    let program =
        parse_program("decl zone(name, index)\nz(n) where zone({ name: n, size: 1 })\n").unwrap();
    let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
    assert!(
        e.contains("unknown field 'size' for predicate 'zone'"),
        "{e}"
    );
}

/// A tuple is a pattern, never a value; a list on the left of `=` is
/// written as a tuple.
#[test]
fn a_tuple_is_not_a_value() {
    for src in [
        "p(1)\nq(x) where p(a), x = (a, a)\n",
        "p(1)\nq((a, a)) where p(a)\n",
        "resource net.vpc v { cidrs = (1, 2) }\n",
    ] {
        let e = error(src);
        assert!(e.contains("is a pattern, not a value"), "{src}: {e}");
    }
    let e = error("p([1, 2])\nq(a) where p(l), [a, b] = l\n");
    assert!(
        e.contains("`[a, b]` is a list: a pattern is a tuple"),
        "{e}"
    );
    assert!(e.contains("write `(a, b)`"), "{e}");
}

/// The crud-api example's worked case: a plain value under a secret's
/// name in a container's env.
#[test]
fn a_plaintext_secret_in_env_is_denied() {
    let src = r#"container("web", { name: "api", env: [{ name: "DB_PASSWORD", value: "hunter2" }, { name: "API_TOKEN", valueFrom: { secretKeyRef: { name: "t" } } }, { name: "PORT", value: "80" }] })
container("job", { name: "migrate" })
secret_word("SECRET")
secret_word("PASSWORD")
secret_word("TOKEN")
deny "plaintext secret in env" { workload: w, container: c.name, env: e.name } where {
  container(w, c)
  (_, e) in c.env
  has e.value
  secret_word(s)
  (_, _) = str.split(e.name, s, 1)
}
"#;
    let deny = facts(src, "deny");
    assert_eq!(deny.len(), 1, "{deny:?}");
    assert!(deny[0].contains(r#"env: "DB_PASSWORD""#), "{deny:?}");
    let example =
        std::fs::read_to_string(common::repo().join("examples/crud-api/stacks/crud_api.df"))
            .unwrap();
    assert!(example.contains("deny \"plaintext secret in env\""));
    assert!(example.contains("(_, e) in c.env"));
}

/// `why` prints the statement with its patterns as written.
#[test]
fn why_shows_the_pattern_as_written() {
    let s = Scratch::project("lang-patterns");
    s.write(
        "p.df",
        "edition 2026\nprovider fake\n\nlabels({ app: \"web\" })\n\nlabel(k, v) where labels(l), (k, v) in l\n",
    );
    let r = s.run(&["why", "label(_, _)", "p.df"]).success();
    assert!(
        r.stdout
            .contains("label(k, v) where labels(l), (k, v) in l\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("with k = \"app\", v = \"web\""),
        "{}",
        r.stdout
    );
}

/// A tuple pattern against a partial function with no value fails the
/// match (docs/grammar.md "Patterns"); a name bound to it alone is still
/// an error naming the call (tests/functions.rs).
#[test]
fn a_pattern_against_a_call_with_no_value_fails_the_match() {
    let src = r#"text("[1, 2]")
text("not json")
two(a, b) where text(t), (a, b) = json.decode(t)
"#;
    assert_eq!(facts(src, "two"), ["two(1, 2)"]);
}
