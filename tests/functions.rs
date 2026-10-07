//! The function registry (DESIGN.org R-6): every function a program calls
//! is declared in `std/*.df`, named by its package, and the engine's
//! bodies, the resolver and the reference read the same declarations.

mod common;
use common::{error, facts};
use dform_core::engine;
use dform_core::parser::parse_program;

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
    let e = error("component inet {\n  output k = 1\n}\nresource inet main {}\n");
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

/// `regex.match`, `regex.capture`, `regex.replace`; a bad pattern
/// literal is a compile error (R-31), not a quiet no-value.
#[test]
fn regex_functions_evaluate() {
    let src = r#"m(b) where b = regex.match("web-1", "^[a-z]+-[0-9]+$")
c(g) where g = regex.capture("app:1.2.3", "^(.+):(.+)$", 2)
r(s) where s = regex.replace("a_b_c", "_", "-")
"#;
    assert_eq!(facts(src, "m"), ["m(true)"]);
    assert_eq!(facts(src, "c"), [r#"c("1.2.3")"#]);
    assert_eq!(facts(src, "r"), [r#"r("a-b-c")"#]);
}

/// A bad regex pattern literal fails at the call, not at evaluation.
#[test]
fn a_bad_regex_literal_is_a_compile_error() {
    let e = error("p(x) where x = regex.match(\"a\", \"[\")\n");
    assert!(
        e.contains("is a regex") && e.contains("not a valid pattern"),
        "{e}"
    );
}

/// `semver.parse`, `semver.satisfies`, `semver.compare`.
#[test]
fn semver_functions_evaluate() {
    let src = r#"p(maj) where v = semver.parse("1.2.3-rc.1"), maj = v.major
s(ok) where ok = semver.satisfies("1.5.0", "^1.0")
c(n) where n = semver.compare("2.0.0", "1.9.9")
"#;
    assert_eq!(facts(src, "p"), ["p(1)"]);
    assert_eq!(facts(src, "s"), ["s(true)"]);
    assert_eq!(facts(src, "c"), ["c(1)"]);
}

/// `oci.pinned`, `oci.with_digest` (the OCI distribution
/// reference grammar; tests/oci_type.rs has the rest, R-133).
#[test]
fn oci_functions_evaluate() {
    let digest = format!("sha256:{}", "0".repeat(64));
    let src = format!(
        r#"p0(x) where x = oci.pinned("app:1.2.3")
p1(x) where x = oci.pinned("app@{digest}")
w(ref) where ref = oci.with_digest("org/app:1.2.3", "{digest}")
"#
    );
    assert_eq!(facts(&src, "p0"), ["p0(false)"]);
    assert_eq!(facts(&src, "p1"), ["p1(true)"]);
    assert_eq!(facts(&src, "w"), [format!("w(org/app:1.2.3@{digest})")]);
}

/// `str.trim`, `replace`, `starts_with`, `ends_with`, `contains`,
/// `format`, `pad_left`, `pad_right`, `len`, `slice`.
#[test]
fn str_additions_evaluate() {
    let src = r#"t(s) where s = str.trim("  web  ")
r(s) where s = str.replace("a_b_c", "_", "-")
sw(b) where b = str.starts_with("web-1", "web-")
ew(b) where b = str.ends_with("image:latest", ":latest")
co(b) where b = str.contains("team=platform", "team=")
fo(s) where s = str.format("%s-%s", ["a", "b"])
pl(s) where s = str.pad_left("7", 3, "0")
pr(s) where s = str.pad_right("ab", 4, "-")
ln(n) where n = str.len("hello")
sl(s) where s = str.slice("hello world", 6)
sl2(s) where s = str.slice("hello world", 0, 5)
"#;
    assert_eq!(facts(src, "t"), [r#"t("web")"#]);
    assert_eq!(facts(src, "r"), [r#"r("a-b-c")"#]);
    assert_eq!(facts(src, "sw"), ["sw(true)"]);
    assert_eq!(facts(src, "ew"), ["ew(true)"]);
    assert_eq!(facts(src, "co"), ["co(true)"]);
    assert_eq!(facts(src, "fo"), [r#"fo("a-b")"#]);
    assert_eq!(facts(src, "pl"), [r#"pl("007")"#]);
    assert_eq!(facts(src, "pr"), [r#"pr("ab--")"#]);
    assert_eq!(facts(src, "ln"), ["ln(5)"]);
    assert_eq!(facts(src, "sl"), [r#"sl("world")"#]);
    assert_eq!(facts(src, "sl2"), [r#"sl2("hello")"#]);
}

/// `list.sort`, `sort_by`, `unique`, `flatten`, `zip`, `min`, `max`,
/// `sum`, `contains`, `first`, `last`.
#[test]
fn list_additions_evaluate() {
    let src = r#"so(l) where l = list.sort([3, 1, 2])
un(l) where l = list.unique([1, 2, 1, 3, 2])
fl(l) where l = list.flatten([[1, 2], [3]])
zi(l) where l = list.zip([1, 2], ["a", "b", "c"])
mn(n) where n = list.min([3, 1, 2])
mx(n) where n = list.max([3, 1, 2])
su(n) where n = list.sum([1, 2, 3])
co(b) where b = list.contains([1, 2, 3], 2)
fi(n) where n = list.first([1, 2, 3])
la(n) where n = list.last([1, 2, 3])
"#;
    assert_eq!(facts(src, "so"), ["so([1, 2, 3])"]);
    assert_eq!(facts(src, "un"), ["un([1, 2, 3])"]);
    assert_eq!(facts(src, "fl"), ["fl([1, 2, 3])"]);
    assert_eq!(facts(src, "zi"), [r#"zi([[1, "a"], [2, "b"]])"#]);
    assert_eq!(facts(src, "mn"), ["mn(1)"]);
    assert_eq!(facts(src, "mx"), ["mx(3)"]);
    assert_eq!(facts(src, "su"), ["su(6)"]);
    assert_eq!(facts(src, "co"), ["co(true)"]);
    assert_eq!(facts(src, "fi"), ["fi(1)"]);
    assert_eq!(facts(src, "la"), ["la(3)"]);
    let src2 = "so(l) where l = list.sort_by([{name: \"b\"}, {name: \"a\"}], \"name\")\n";
    assert_eq!(facts(src2, "so"), [r#"so([{name: "a"}, {name: "b"}])"#]);
}

/// `hash.sha256`, `hash.short`.
#[test]
fn hash_functions_evaluate() {
    let src = r#"h(d) where d = hash.sha256("hello")
s(d) where d = hash.short("hello", 8)
"#;
    assert_eq!(
        facts(src, "h"),
        [r#"h("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")"#]
    );
    assert_eq!(facts(src, "s"), [r#"s("2cf24dba")"#]);
}

/// `base64.encode`, `base64.decode` round-trip, and bad text is no value.
#[test]
fn base64_functions_evaluate() {
    let src = r#"e(s) where s = base64.encode("hello")
d(s) where s = base64.decode("aGVsbG8=")
"#;
    assert_eq!(facts(src, "e"), [r#"e("aGVsbG8=")"#]);
    assert_eq!(facts(src, "d"), [r#"d("hello")"#]);
    // Bad base64 has no value: `x = ..` with nothing else to bind `x`
    // from is an error naming the call (engine.rs's `failed_builtin`).
    let program = parse_program("p(x) where x = base64.decode(\"not base64!\")\n").unwrap();
    let err = engine::eval(&program, &[]).unwrap_err();
    assert!(
        err.to_string()
            .contains("is not defined for these arguments"),
        "{err}"
    );
}

/// A `url` literal is canonicalized and checked at compile time (R-31);
/// `url.join`, `with_scheme`, `with_host`, `with_port`, `with_path`,
/// `with_query`, `url.encode`. A url prints as its canonical text, as an
/// `inet` does; `url.join` and `url.encode` are about strings.
#[test]
fn url_functions_evaluate() {
    let src = r#"u(x) where x = url("https://example.com/a")
j(x) where x = url.join("https://example.com/a", "b")
sc(x) where x = url.with_scheme(url("http://h/p"), "https")
ho(x) where x = url.with_host(url("http://h/p"), "other")
po(x) where x = url.with_port(url("http://h/p"), 8080)
pa(x) where x = url.with_path(url("http://h/p"), "/q")
qu(x) where x = url.with_query(url("http://h/p"), { a: "1" })
en(x) where x = url.encode("a b/c")
pr(h) where p = url.parse("https://h.example.com:8080/x?a=1"), h = p.host
"#;
    assert_eq!(facts(src, "u"), [r#"u(https://example.com/a)"#]);
    assert_eq!(facts(src, "j"), [r#"j("https://example.com/a/b")"#]);
    assert_eq!(facts(src, "sc"), [r#"sc(https://h/p)"#]);
    assert_eq!(facts(src, "ho"), [r#"ho(http://other/p)"#]);
    assert_eq!(facts(src, "po"), [r#"po(http://h:8080/p)"#]);
    assert_eq!(facts(src, "pa"), [r#"pa(http://h/q)"#]);
    assert_eq!(facts(src, "qu"), [r#"qu(http://h/p?a=1)"#]);
    assert_eq!(facts(src, "en"), [r#"en("a%20b%2Fc")"#]);
    assert_eq!(facts(src, "pr"), [r#"pr("h.example.com")"#]);
}

/// A url is a value (the url ticket's decisions): two spellings of one
/// url are equal, a url never equals its string, `.scheme`, `.host`,
/// `.port`, `.path`, `.query` and `.fragment` read its components, and
/// JSON carries its canonical text.
#[test]
fn a_url_is_a_value() {
    let src = r#"same() where url("HTTPS://Example.COM:443") == url("https://example.com/")
text() where url("https://example.com/") == "https://example.com/"
parts(s, h, p, a, q, f) where u = url("http://h.example.com:8080/a/b?x=1#top"), s = u.scheme, h = u.host, p = u.port, a = u.path, q = u.query.x, f = u.fragment
noport(p) where u = url("https://h/"), p = u.port
enc(j) where j = json.encode({ u: url("https://h") })
"#;
    assert_eq!(facts(src, "same"), ["same()"]);
    assert!(facts(src, "text").is_empty());
    assert_eq!(
        facts(src, "parts"),
        [r#"parts("http", "h.example.com", 8080, "/a/b", "1", "top")"#]
    );
    assert!(facts(src, "noport").is_empty());
    assert_eq!(facts(src, "enc"), [r#"enc("{\"u\":\"https://h/\"}")"#]);
}

/// A bad url literal in a `url`-typed position is a compile error.
/// `url(text: string)`'s own literal, like `inet`'s and `ip`'s, is a
/// string argument: checked where its position's type is `url`
/// (`with_scheme`'s `u`), not at the bare constructor's own call.
#[test]
fn a_bad_url_literal_is_a_compile_error() {
    let e = error("p(x) where x = url.with_scheme(\"not a url\", \"https\")\n");
    assert!(e.contains("is a url"), "{e}");
}

/// A schema attribute typed `url` (R-31; the url ticket's "Done when"):
/// a good literal plans, a bad one is a compile error at the resource.
#[test]
fn a_url_typed_attribute_checks_its_literal() {
    let s = common::Scratch::new("url-attr");
    s.write(
        "schema.df",
        "\ntype_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"link\", \"url\", [])\n",
    );
    let run = |body: &str| {
        s.write("p.df", &format!("\n{body}"));
        s.run(&common::on(
            "p.df",
            &["--provider", "schema.df", "--world", "w.json"],
            &["plan"],
        ))
    };
    let r = run("resource app.thing t { link = \"https://example.com/a\" }\n").success();
    assert!(
        r.stdout.contains("link = \"https://example.com/a\""),
        "{}",
        r.stdout
    );
    let r = run("resource app.thing t { link = \"nope\" }\n").failure();
    assert!(r.stderr.contains("is a url"), "{}", r.stderr);
}

/// `path.join`, `dir`, `base`, `ext`, `rel`, `clean`.
#[test]
fn path_functions_evaluate() {
    let src = r#"j(s) where s = path.join("a", "b", "c.yaml")
d(s) where s = path.dir("a/b/c.yaml")
b(s) where s = path.base("a/b/c.yaml")
e(s) where s = path.ext("c.yaml")
r(s) where s = path.rel("a/b", "a/b/c/d.yaml")
cl(s) where s = path.clean("a/../a/./b.yaml")
"#;
    assert_eq!(facts(src, "j"), [r#"j("a/b/c.yaml")"#]);
    assert_eq!(facts(src, "d"), [r#"d("a/b")"#]);
    assert_eq!(facts(src, "b"), [r#"b("c.yaml")"#]);
    assert_eq!(facts(src, "e"), [r#"e(".yaml")"#]);
    assert_eq!(facts(src, "r"), [r#"r("c/d.yaml")"#]);
    assert_eq!(facts(src, "cl"), [r#"cl("a/b.yaml")"#]);
}

/// `json.decode`/`encode`, `yaml.decode`/`encode`, `toml.decode`/`encode`.
#[test]
fn document_decode_and_encode_evaluate() {
    let src = r#"j(n) where v = json.decode("{\"a\": 1}"), n = v.a
je(s) where s = json.encode({ a: 1 })
y(n) where v = yaml.decode("a: 1\n"), n = v.a
ye(s) where s = yaml.encode({ a: 1 })
t(n) where v = toml.decode("a = 1\n"), n = v.a
te(s) where s = toml.encode({ a: 1 })
"#;
    assert_eq!(facts(src, "j"), ["j(1)"]);
    assert_eq!(facts(src, "je"), [r#"je("{\"a\":1}")"#]);
    assert_eq!(facts(src, "y"), ["y(1)"]);
    assert_eq!(facts(src, "t"), ["t(1)"]);
    assert_eq!(facts(src, "te"), [r#"te("a = 1\n")"#]);
    let ye = facts(src, "ye");
    assert_eq!(ye.len(), 1);
    assert!(ye[0].contains("a: 1"), "{ye:?}");
}

/// `json.decode`, `yaml.decode` and `toml.decode` read as a table's
/// document does: a number is an int or a float as written (R-75), a null
/// member is absent, a null element and a YAML tag leave no value, a TOML
/// datetime is a time.
#[test]
fn decode_reads_as_a_document_does() {
    let src = r#"whole(n, i) where n = json.decode("{\"a\": 2.0}").a, i = json.decode("2")
frac(n) where n = yaml.decode("a: 1.5\n").a
absent(k) where v = json.decode("{\"a\": 1, \"b\": null}"), k = len(v)
when(t) where t = toml.decode("a = 2026-10-02T09:00:00Z\n").a, time.before(t, time("2027-01-01T00:00:00Z"))
"#;
    assert_eq!(facts(src, "whole"), ["whole(2.0, 2)"]);
    assert_eq!(facts(src, "frac"), ["frac(1.5)"]);
    assert_eq!(facts(src, "absent"), ["absent(1)"]);
    assert_eq!(facts(src, "when").len(), 1);
    // A null element and a tag: no value, an error naming the call
    // (engine.rs's `failed_builtin`).
    for call in [r#"json.decode("[1, null]")"#, r#"yaml.decode("a: !x 1\n")"#] {
        let program = parse_program(&format!("p(x) where x = {call}\n")).unwrap();
        let err = engine::eval(&program, &[]).unwrap_err().to_string();
        assert!(err.contains("is not defined for these arguments"), "{err}");
    }
}
