//! The function registry (DESIGN.org R-6): every function a program calls
//! is declared in `std/*.df`, named by its package, and the engine's
//! bodies, the resolver and the reference read the same declarations.

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
fn qualified_functions_evaluate() {
    let got = facts(
        r#"net(inet("10.50.0.0/16"))
sub(c) where net(n), c = inet.subnet(n, 8, 2)
host(h) where net(n), h = inet.host(n, 1)
size(p) where net(n), p = inet.prefix_len(n)
inside(a) where a = ip("10.50.3.4"), net(n), inet.contains(n, a)
words(w) where w = str.split("a,b", ",")
joined(j) where j = list.join(["a", 1], "-")
counted(a, b) where a = len([1, 2]), b = list.len("abc")
port(p) where p = int("8080") + 1
"#,
        "sub",
    );
    assert_eq!(got, ["sub(10.50.2.0/24)"]);
    let one = |pred: &str, src: &str| facts(src, pred);
    assert_eq!(
        one(
            "inside",
            "net(inet(\"10.50.0.0/16\"))\ninside(a) where a = ip(\"10.50.3.4\"), net(n), inet.contains(n, a)\n"
        ),
        ["inside(10.50.3.4)"]
    );
    assert_eq!(
        one("port", "port(p) where p = int(\"8080\") + 1\n"),
        ["port(8081)"]
    );
    assert_eq!(
        one(
            "joined",
            "joined(j) where j = list.join(str.split(\"a,b\", \",\"), \"-\")\n"
        ),
        ["joined(\"a-b\")"]
    );
}

/// A name `std/*.df` does not declare does not resolve: the deleted
/// builtins and the old spellings, with the name meant.
#[test]
fn a_function_missing_from_std_does_not_resolve() {
    for (call, meant) in [
        (
            "inet_subnet(inet(\"10.0.0.0/8\"), 8, 1)",
            Some("inet.subnet"),
        ),
        ("to_int(\"1\")", Some("int")),
        ("split(\"a,b\", \",\")", Some("str.split")),
        ("cidrsubnet(\"10.0.0.0/8\", 8, 1)", None),
        ("gref(\"a\", \"b\", \"c\")", None),
        ("concat(\"a\", \"b\")", None),
        ("geo.distance(1, 2)", None),
    ] {
        let e = error(&format!("p(x) where x = {call}\n"));
        let name = call.split('(').next().unwrap();
        assert!(e.contains(&format!("unknown function {name}")), "{e}");
        if let Some(m) = meant {
            assert!(e.contains(&format!("the function is `{m}`")), "{e}");
        }
    }
}

/// Arithmetic is the lowering's: `a + b`, not `add(a, b)`.
#[test]
fn internal_functions_are_not_callable() {
    let e = error("p(x) where x = add(1, 2)\n");
    assert!(
        e.contains("add is the lowering's") && e.contains("a + b"),
        "{e}"
    );
    assert_eq!(facts("p(x) where x = 1 + 2\n", "p"), ["p(3)"]);
}

/// A dotted name's head names one thing: a component named like a
/// function package is an error naming both.
#[test]
fn a_head_two_things_claim_is_an_error() {
    let e = error("component inet {\n  output k = 1\n}\ninstance inet main\n");
    assert!(
        e.contains(
            "`inet` is both the component `inet` and the function package `inet` (std/inet.df)"
        ),
        "{e}"
    );
}

/// Hover and signature help read the declarations.
#[test]
fn the_reference_reads_std() {
    let r = engine::reference("inet.subnet", true).unwrap();
    assert_eq!(
        r.signature,
        "inet.subnet(net: inet, bits: int, n: int) -> inet?"
    );
    assert!(r.example.contains("inet.subnet("), "{r:?}");
    assert!(engine::reference("add", true).is_none());
    assert!(engine::reference("to_int", true).is_none());
}

/// `str.dedent` (R-61): a string literal that spans lines keeps what is
/// written; dedent removes the indentation its lines share.
#[test]
fn dedent_strips_the_shared_indentation() {
    let src = "s(str.dedent(\"\n    #!/bin/sh\n      echo hi\n\n    done\n  \"))\n";
    assert_eq!(
        facts(src, "s"),
        [r##"s("#!/bin/sh\n  echo hi\n\ndone\n")"##]
    );
    // Tabs and spaces share only what is the same.
    assert_eq!(
        facts("s(str.dedent(\"\\t a\\n\\t b\\n  c\"))\n", "s"),
        [r#"s("\t a\n\t b\n  c")"#]
    );
}

/// A string literal may span lines (R-61): nothing is stripped, and a hole
/// on a later line interpolates.
#[test]
fn a_string_spans_lines_as_written() {
    let src = "n(\"web\")\ns(t) where n(x), t = \"one\n  ${x} two\n\"\n";
    assert_eq!(facts(src, "s"), [r#"s("one\n  web two\n")"#]);
    assert_eq!(
        facts(
            "n(\"web\")\ns(str.dedent(\"\n    a ${x}\n      b\n  \")) where n(x)\n",
            "s"
        ),
        [r#"s("a web\n  b\n")"#]
    );
}

/// `str.split(s, sep, limit)` (R-58) splits at the first `limit`
/// separators only: at most `limit + 1` parts, the rest kept whole. The
/// limit may be left out.
#[test]
fn split_takes_an_optional_limit() {
    let src = r#"s0(p) where p = str.split("a:b:c", ":", 0)
s1(p) where p = str.split("a:b:c", ":", 1)
s9(p) where p = str.split("a:b:c", ":", 9)
all(p) where p = str.split("a:b:c", ":")
"#;
    assert_eq!(facts(src, "s0"), [r#"s0(["a:b:c"])"#]);
    assert_eq!(facts(src, "s1"), [r#"s1(["a", "b:c"])"#]);
    assert_eq!(facts(src, "s9"), [r#"s9(["a", "b", "c"])"#]);
    assert_eq!(facts(src, "all"), [r#"all(["a", "b", "c"])"#]);
    let r = engine::reference("str.split", true).unwrap();
    assert_eq!(
        r.signature,
        "str.split(text: string, sep: string, limit?: int) -> list(string)?"
    );
}
