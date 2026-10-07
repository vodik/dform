//! Column types (R-34): a relation with no `decl` gets each column's type
//! from its uses, and the uses are checked against it. The errors without
//! a schema each have a case in tests/syntax/err/column_types.df; these
//! are the signatures, the literals read as their column's type, the
//! first source of an undeclared table, and the check against the
//! provider's schema.

mod common;
use common::Scratch;
use dform::parser::parse_file;
use dform::partition::fmt_atom;
use dform::transform;

fn program(src: &str) -> String {
    format!("\n{src}")
}

/// The lowered program's signatures, as hover prints them.
fn signatures(src: &str) -> Vec<String> {
    let p = parse_file("t.df", &program(src)).unwrap_or_else(|e| panic!("{e:#}"));
    let l = transform::lower(&p).unwrap_or_else(|e| panic!("{e:#}"));
    l.signatures.values().map(|s| s.to_string()).collect()
}

/// What lowering `src` reports.
fn error(src: &str) -> String {
    let p = parse_file("t.df", &program(src)).unwrap_or_else(|e| panic!("{e:#}"));
    match transform::lower(&p) {
        Ok(_) => panic!("lowers: {src}"),
        Err(e) => format!("{e:#}"),
    }
}

fn facts(src: &str, pred: &str) -> Vec<String> {
    let p = parse_file("t.df", &program(src)).unwrap_or_else(|e| panic!("{e:#}"));
    let (r, _) = dform::engine::eval(&p, &[]).unwrap_or_else(|e| panic!("{e:#}"));
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(fmt_atom)
        .collect()
}

/// Literals give a fact's columns their types; a rule's head takes its
/// body's, through the variables; a decl names them.
#[test]
fn columns_are_inferred_through_rules() {
    let sigs = signatures(
        "az(\"us-east-1a\", 1)\naz(\"us-east-1b\", 2)\n\
         first(zone, index) where az(zone, index), index < 2\n\
         decl tier(name: string, size: int)\ntier(\"a\", 1)\n",
    );
    assert_eq!(
        sigs,
        [
            "az(string, int)",
            "first(zone: string, index: int)",
            "tier(name: string, size: int)"
        ]
    );
}

/// A function's parameter types its argument's column, and its result
/// the column it is bound into.
#[test]
fn a_function_types_the_columns_it_reads_and_writes() {
    let sigs = signatures(
        "net(\"10.0.0.0/8\")\n\
         inside(n, a) where net(n), a = \"10.1.2.3\", inet.contains(n, a)\n\
         subnet_of(s) where net(n), s = inet.subnet(n, 8, 1)\n",
    );
    assert_eq!(
        sigs,
        ["inside(n: inet, a: ip)", "net(inet)", "subnet_of(s: inet)"]
    );
}

/// A string in a column a use types `inet` is read as one (R-31): the
/// function gets a network, and the rule derives.
#[test]
fn a_string_in_an_inet_column_is_read_as_one() {
    assert_eq!(
        facts(
            "net(\"10.0.0.0/8\")\nnet(\"192.168.0.0/16\")\n\
             inside(n) where net(n), inet.contains(n, \"10.1.2.3\")\n",
            "inside"
        ),
        ["inside(10.0.0.0/8)"]
    );
}

/// Two typed uses that disagree: an error naming both.
#[test]
fn two_typed_uses_that_disagree_name_both() {
    let e = error(
        "decl count_of(n: int)\ndecl name_of(n: string)\ncount_of(1)\nname_of(\"a\")\n\
         both(n) where count_of(n), name_of(n)\n",
    );
    assert!(
        e.contains(
            "t.df:3:1: `name_of`'s column `n` is string here (decl name_of), and joins \
             `count_of`'s column `n`, which is int"
        ),
        "{e}"
    );
    assert!(
        e.contains("t.df:2:1: `count_of`'s column `n` is int here (decl count_of)"),
        "{e}"
    );
}

/// The amendment's example: with a use that types the column `inet`, the
/// int and the string that does not parse are both errors; with none, the
/// int against the strings is one.
#[test]
fn literals_are_checked_against_the_settled_type() {
    let e = error(
        "az(\"10.1.1.0/24\")\naz(4)\naz(\"foo\")\n\
         inside(n) where az(n), inet.contains(n, \"10.1.1.1\")\n",
    );
    assert!(
        e.contains("t.df:3:1: `az`'s column 1 is inet, not the int 4"),
        "{e}"
    );
    assert!(
        e.contains("t.df:4:1: `az`'s column 1 is an inet: \"foo\" is not a network"),
        "{e}"
    );
    assert_eq!(
        e.lines().filter(|l| l.starts_with("t.df")).count(),
        2,
        "{e}"
    );
    let e = error("az(\"10.1.1.0/24\")\naz(4)\naz(\"foo\")\n");
    assert!(
        e.contains("t.df:3:1: `az`'s column 1 holds the int 4 here and strings elsewhere"),
        "{e}"
    );
    assert_eq!(
        e.lines().filter(|l| l.starts_with("t.df")).count(),
        1,
        "{e}"
    );
}

/// `n + 1` on a column of strings, and a comparison that is never true,
/// are compile errors, not a silent non-match.
#[test]
fn arithmetic_and_comparisons_are_checked() {
    let e = error("zone(\"a\", \"1\")\nnext(z, m) where zone(z, n), m = n + 1\n");
    assert!(
        e.contains("`n + 1`: `n` is string, and arithmetic is on numbers"),
        "{e}"
    );
    let e = error("zone(\"a\", 1)\nbig(z) where zone(z, n), n > \"0\"\n");
    assert!(
        e.contains("`n > \"0\"`: `n` is int, not the string \"0\""),
        "{e}"
    );
    let e = error("zone(\"a\", 1)\nname(\"a\")\nodd(z) where zone(z, n), name(m), n != m\n");
    assert!(
        e.contains("`n != m` compares int with string: they are never equal"),
        "{e}"
    );
}

/// A column declared `any` takes values of every type.
#[test]
fn a_column_declared_any_takes_both() {
    assert_eq!(
        facts(
            "decl release(key, value: any)\nrelease(\"digest\", \"sha256:ab\")\n\
             release(\"schema\", 42)\n",
            "release"
        ),
        [
            "release(\"digest\", \"sha256:ab\")",
            "release(\"schema\", 42)"
        ]
    );
}

/// `input p from FORMAT(PATH)` with no `decl`: its columns are the first
/// source's, in the document's order, and the uses type them.
#[test]
fn an_undeclared_table_takes_its_first_sources_columns() {
    let s = Scratch::project("infer-table");
    s.write(
        "zones.json",
        r#"[{"zone": "a", "index": 0, "cidr": "10.0.0.0/16"},
            {"zone": "b", "index": 1, "cidr": "10.1.0.0/16"}]"#,
    );
    s.write("nets.csv", "name,cidr\na,10.0.0.0/16\n");
    s.write(
        "p.df",
        "\n\ninput zone from json(\"zones.json\")\ninput net from csv(\"nets.csv\")\n\n\
         use fake\n\n\
         next(z, i) where zone(z, n, _), i = n + 1\n\
         inside(z) where zone(z, _, c), inet.contains(c, \"10.1.2.3\")\n\
         named(n) where net(n, c), inet.contains(c, \"10.0.0.1\")\n",
    );
    let r = s.run(&["query", "next(z, i)", "p.df"]).success();
    assert!(r.stdout.contains("\"a\"  1\n\"b\"  2\n"), "{}", r.stdout);
    let r = s.run(&["query", "inside(z)", "p.df"]).success();
    assert!(r.stdout.ends_with("\n\"b\"\n"), "{}", r.stdout);
    let r = s.run(&["query", "named(n)", "p.df"]).success();
    assert!(r.stdout.ends_with("\n\"a\"\n"), "{}", r.stdout);
    // A first source the compiler cannot read: declare the columns.
    s.write(
        "p.df",
        "\n\ninput zone from json(\"missing.json\")\n\nuse fake\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "input zone from ..: zone has no `decl`, so its columns are its first source's, \
             and missing.json cannot be read for them"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("decl zone(a: T, ..)"), "{}", r.stderr);
}

/// With the provider's schema, an attribute read into a column types it:
/// `net.vpc`'s `cidr` is a string, so an int in the column is an error.
#[test]
fn an_attribute_read_types_its_column() {
    let s = Scratch::new("infer-schema");
    s.write(
        "p.df",
        "\n\nuse fake\n\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
         decl cidr_of(c) mixed\ncidr_of(c) where v in net.vpc, c = v.cidr\ncidr_of(5)\n",
    );
    let r = s
        .run(&common::on("p.df", &["--world", "w.json"], &["plan"]))
        .failure();
    assert!(
        r.stderr
            .contains("`cidr_of`'s column `c` is string, not the int 5"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("(net.vpc.cidr)"), "{}", r.stderr);
}

/// A column filled from a copy's output takes the output's declared type,
/// and one filled from a `let` the type of its value; not `any`.
#[test]
fn outputs_and_lets_type_the_columns_they_reach() {
    let sigs = signatures(
        "component network {\n  output cidr: inet = \"10.0.0.0/16\"\n  output n: int = 3\n}\n\
         resource network main {}\nlet width = 8\n\
         p(c, k) where c = main.cidr, k = main.n\nq(w) where w = width\n",
    );
    assert!(sigs.contains(&"p(c: inet, k: int)".to_string()), "{sigs:?}");
    assert!(sigs.contains(&"q(w: int)".to_string()), "{sigs:?}");
}

/// A url is its own column type: a url literal types its column, and an
/// input typed `url` takes one (its default is a url, not a string).
#[test]
fn a_url_types_its_column() {
    let sigs = signatures(
        "input home: url = \"https://a.example/x\"\n\
         let b: url = \"https://b.example\"\np(u) where u = b\nq(h) where h = home\n",
    );
    assert!(sigs.contains(&"p(u: url)".to_string()), "{sigs:?}");
    assert!(sigs.contains(&"q(h: url)".to_string()), "{sigs:?}");
}
