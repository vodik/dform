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
        r#"net("10.50.0.0/16")
sub(c) where net(n), c = inet.subnet(n, 8, 2)
host(h) where net(n), h = inet.host(n, 1)
size(p) where net(n), p = n.bits
inside(a) where a = "10.50.3.4", net(n), a in n
words(w) where w = str.split("a,b", ",")
joined(j) where j = list.join(["a", 1], "-")
counted(a, b) where xs = [1, 2], s = "abc", a = xs.len, b = s.len
"#,
        "sub",
    );
    assert_eq!(got, ["sub(10.50.2.0/24)"]);
    let one = |pred: &str, src: &str| facts(src, pred);
    assert_eq!(
        one(
            "inside",
            "decl net(n: inet)\nnet(\"10.50.0.0/16\")\ninside(a) where a = \"10.50.3.4\", net(n), a in n\n"
        ),
        [r#"inside("10.50.3.4")"#]
    );
    assert_eq!(
        one(
            "port",
            "let t = \"8080\"\nlet n: int = t\nport(p) where p = n + 1\n"
        ),
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
        ("inet_subnet(\"10.0.0.0/8\", 8, 1)", Some("inet.subnet")),
        ("to_int(\"1\")", None),
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
    // A conversion's old name says how a value of the type is had (R-155).
    let e = error("p(x) where x = to_int(\"1\")\n");
    assert!(
        e.contains("`int.trunc(f)`") && e.contains("`let n: int = s`"),
        "{e}"
    );
}

/// Arithmetic is the lowering's: `a + b`, not `add(a, b)`.
#[test]
fn internal_functions_are_not_callable() {
    let e = error("p(x) where x = add(1, 2)\n");
    assert!(
        e.contains("add is dform's own") && e.contains("a + b"),
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
    let r = dform_core::reference::reference("inet.subnet", true).unwrap();
    assert_eq!(
        r.signature,
        "inet.subnet(net: inet, bits: int, n: int) -> inet"
    );
    assert!(r.example.contains("inet.subnet("), "{r:?}");
    assert!(dform_core::reference::reference("add", true).is_none());
    assert!(dform_core::reference::reference("to_int", true).is_none());
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
    let r = dform_core::reference::reference("str.split", true).unwrap();
    assert_eq!(
        r.signature,
        "str.split(text: string, sep: string, limit?: int) -> list(string)"
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

/// A `semver` is a type (R-134): its parts are fields, a string where
/// one is wanted is read as one, versions compare with `<`;
/// `semver.satisfies`.
#[test]
fn semver_functions_evaluate() {
    let src = r#"let v: semver = "1.2.3-rc.1"
p(maj, pre) where maj = v.major, pre = v.pre
s(ok) where ok = semver.satisfies("1.5.0", "^1.0")
let next: semver = "2.0.0"
c() where next > "1.9.9"
pre() where v < "1.2.3"
"#;
    assert_eq!(facts(src, "p"), [r#"p(1, "rc.1")"#]);
    assert_eq!(facts(src, "s"), ["s(true)"]);
    assert_eq!(facts(src, "c"), ["c()"]);
    assert_eq!(facts(src, "pre"), ["pre()"]);
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
/// `format`, `pad_left`, `pad_right`, `.len`, `slice`.
#[test]
fn str_additions_evaluate() {
    let src = r#"t(s) where s = str.trim("  web  ")
r(s) where s = str.replace("a_b_c", "_", "-")
sw(b) where b = str.starts_with("web-1", "web-")
ew(b) where b = str.ends_with("image:latest", ":latest")
co() where "team=" in "team=platform"
nco() where not "x" in "team=platform"
fo(s) where s = str.format("%s-%s", "a", "b")
pl(s) where s = str.pad_left("7", 3, "0")
pr(s) where s = str.pad_right("ab", 4, "-")
ln(n) where s = "hello", n = s.len
sl(s) where s = str.slice("hello world", 6)
sl2(s) where s = str.slice("hello world", 0, 5)
"#;
    assert_eq!(facts(src, "t"), [r#"t("web")"#]);
    assert_eq!(facts(src, "r"), [r#"r("a-b-c")"#]);
    assert_eq!(facts(src, "sw"), ["sw(true)"]);
    assert_eq!(facts(src, "ew"), ["ew(true)"]);
    assert_eq!(facts(src, "co"), ["co()"]);
    assert_eq!(facts(src, "nco"), ["nco()"]);
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
co() where 2 in [1, 2, 3]
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
    assert_eq!(facts(src, "co"), ["co()"]);
    assert_eq!(facts(src, "fi"), ["fi(1)"]);
    assert_eq!(facts(src, "la"), ["la(3)"]);
    let src2 = "so(l) where l = list.sort_by([{name: \"b\"}, {name: \"a\"}], \"name\")\n";
    assert_eq!(facts(src2, "so"), [r#"so([{name: "a"}, {name: "b"}])"#]);
}

/// `hash.sha256`, and a short digest as a slice of it.
#[test]
fn hash_functions_evaluate() {
    let src = r#"h(d) where d = hash.sha256("hello")
s(d) where d = str.slice(hash.sha256("hello"), 0, 8)
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
    // Bad base64 has no answer (`?`, R-134): the literal fails.
    let src =
        "p(x) where x = base64.decode(\"not base64!\")\nq() where not has base64.decode(\"!\").x\n";
    assert!(facts(src, "p").is_empty());
    assert_eq!(facts(src, "q"), ["q()"]);
}

/// `path.join`, `dir`, `base`, `ext`, `rel`, `clean`.
#[test]
fn path_functions_evaluate() {
    let src = r#"j(s) where s = path.join(["a", "b/", "/c.yaml"])
d(s) where s = path.dir("a/b/c.yaml")
b(s) where s = path.base("a/b/c.yaml")
e(s) where s = path.ext("c.yaml")
r(s) where s = path.rel("a/b/c/d.yaml", "a/b")
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
absent(k) where v = json.decode("{\"a\": 1, \"b\": null}"), k = v.len
when(t) where t = toml.decode("a = 2026-10-02T09:00:00Z\n").a, t < "2027-01-01T00:00:00Z"
"#;
    assert_eq!(facts(src, "whole"), ["whole(2.0, 2)"]);
    assert_eq!(facts(src, "frac"), ["frac(1.5)"]);
    assert_eq!(facts(src, "absent"), ["absent(1)"]);
    assert_eq!(facts(src, "when").len(), 1);
    // A null element and a tag: text that is no document has no answer
    // (`?`), so the literal fails.
    for call in [r#"json.decode("[1, null]")"#, r#"yaml.decode("a: !x 1\n")"#] {
        assert!(facts(&format!("p(x) where x = {call}\n"), "p").is_empty());
    }
}

/// `?` is for a valid input with no answer (R-134 rule 3): a partial
/// function's none fails its literal, wherever it stands; any other
/// function's none is an input it does not take (a bad unit, layout,
/// template or port), an error at the rule naming the call.
#[test]
fn a_total_functions_bad_input_is_an_error() {
    let src = "p(x) where x = regex.capture(\"abc\", \"z(.)\", 1)\n\
               q() where regex.capture(\"abc\", \"z(.)\", 1) == \"x\"\n";
    assert!(facts(src, "p").is_empty() && facts(src, "q").is_empty());
    for body in [
        "x = str.pad_left(\"a\", 3, \"\")",
        "str.pad_left(\"a\", 3, \"\") == x, x = \"b\"",
        "x = quantity.to(1536Mi, \"Gi\")",
        "x = quantity.to(1536Mi, \"GB\")",
        "x = time.in_zone(\"2026-10-02T09:00:00Z\", \"Mars/Olympus\")",
        "semver.satisfies(\"1.0.0\", \"not a range\"), x = 1",
    ] {
        let program = parse_program(&format!("p(x) where {body}\n")).unwrap();
        let err = engine::eval(&program, &[])
            .err()
            .unwrap_or_else(|| panic!("{body}: no error"))
            .to_string();
        assert!(
            err.contains("is not defined for these arguments"),
            "{body}: {err}"
        );
    }
}

/// There are no constructors (R-134): a call of one is an error that
/// says how a string becomes the type, a typed position or a typed
/// `let`; `X.parse` and `string(x)` likewise.
#[test]
fn a_constructor_is_an_error_naming_the_typed_position() {
    for (call, help) in [
        ("inet(\"10.0.0.0/8\")", "`let n: inet = \"10.0.0.0/16\"`"),
        ("ip(\"10.0.0.1\")", "`let a: ip = \"10.0.0.1\"`"),
        ("time(\"2026-10-02T09:00:00Z\")", "`let t: time ="),
        ("duration(\"P1M\")", "`let d: duration = 30m`"),
        ("bytes(\"1Gi\")", "`let b: bytes ="),
        ("cpu(\"500m\")", "`let c: cpu = 500m`"),
        ("url(\"https://h\")", "`let u: uri ="),
        ("semver.parse(\"1.2.3\")", "`v.major`"),
        ("time.parse(\"2026-10-02T09:00:00Z\")", "`let t: time ="),
        ("inet.prefix_len(\"10.0.0.0/8\")", "`n.bits`"),
        ("string(1)", "`\"${x}\"`"),
    ] {
        let e = error(&format!("p(x) where x = {call}\n"));
        let name = call.split('(').next().unwrap();
        assert!(e.contains(&format!("unknown function {name}")), "{e}");
        assert!(e.contains(help), "{call}: {e}");
    }
}

/// A typed `let` over a computed string reads it as the type at run time
/// (R-134, the R-133 carry-over): the only way a computed string becomes
/// a value of a type, and one that is not of it is an error naming the
/// `let`.
#[test]
fn a_typed_let_reads_a_computed_string_at_run_time() {
    let src = r#"let cfg = { net: "10.5.0.0/16", image: "ghcr.io/o/app:1.2", v: "1.2.3", bad: "x" }
let net: inet = cfg.net
let image: oci = cfg.image
let v: semver = cfg.v
p(b, t, m) where b = net.bits, t = image.tag, m = v.minor
"#;
    assert_eq!(facts(src, "p"), [r#"p(16, "1.2", 2)"#]);
    let program =
        parse_program("let cfg = { bad: \"x\" }\nlet net: inet = cfg.bad\nq(n) where n = net\n")
            .unwrap();
    let err = engine::eval(&program, &[]).unwrap_err().to_string();
    assert!(
        err.contains("let net is an inet: \"x\" is not a network"),
        "{err}"
    );
}

/// A type with operators has no functions for them (R-134): `t + d`,
/// `b - a`, `a < b` for times, `<` for versions; the functions they
/// replace are errors naming the operator. A string beside a time is
/// read as one.
#[test]
fn a_times_functions_are_its_operators() {
    for (call, help) in [
        ("time.add(t, 1d)", "`t + d`"),
        ("time.until(a, b)", "`b - a`"),
        ("time.before(a, b)", "`a < b`"),
        ("semver.compare(a, b)", "`a < b`"),
    ] {
        let e = error(&format!("p(x) where x = {call}\n"));
        assert!(e.contains(help), "{call}: {e}");
    }
    let src = r#"let a: time = "2026-10-02T09:00:00Z"
let b: time = "2026-10-03T10:30:00Z"
gap(d) where d = b - a
text_gap(d) where d = "2026-10-03T10:30:00Z" - a
"#;
    assert_eq!(facts(src, "gap"), ["gap(25h30m)"]);
    assert_eq!(facts(src, "text_gap"), ["text_gap(25h30m)"]);
}

/// One name per idea (R-134 rule 4): `quantity.to(q, unit)` for every quantity,
/// its unit as its literals write it; one `len`; one `format`, its values
/// after the template; the names they replace are errors naming them.
#[test]
fn one_name_per_idea() {
    let src = r#"let ttl: duration = 36h
let millis: cpu = 1500m
g(n) where n = quantity.to(3Gi, "Gi")
h(n) where n = quantity.to(ttl, "h")
m(n) where n = quantity.to(millis, "m")
l(a, b, c) where xs = [1], s = "ab", o = { x: 1 }, a = xs.len, b = s.len, c = o.len
f(s) where s = str.format("%s:%s", "a", 1)
"#;
    assert_eq!(facts(src, "g"), ["g(3)"]);
    assert_eq!(facts(src, "h"), ["h(36)"]);
    assert_eq!(facts(src, "m"), ["m(1500)"]);
    assert_eq!(facts(src, "l"), ["l(1, 2, 1)"]);
    // `.len` is the count; a key named `len` is `o."len"` (R-155).
    let keyed = "let o = { len: 7, a: 1 }\nk(n, v) where n = o.len, v = o.\"len\"\n";
    assert_eq!(facts(keyed, "k"), ["k(2, 7)"]);
    assert_eq!(facts(src, "f"), [r#"f("a:1")"#]);
    for (call, help) in [
        ("bytes.to(1Gi, \"Mi\")", "`quantity.to(q, unit)`"),
        ("duration.total(1h, \"hours\")", "`quantity.to(q, unit)`"),
        ("list.len([1])", "`x.len`"),
        ("len([1])", "`x.len`"),
        ("format(\"%s\", 1)", "`str.format(\"%s-%s\", a, b)`"),
        ("to(1Gi, \"Mi\")", "`quantity.to(q, unit)`"),
        (
            "int(2.5)",
            "`int.trunc(f)`, `int.round(f)`, `int.floor(f)`, `int.ceil(f)`",
        ),
        ("float(2)", "`let f: float = n`"),
        ("declassify(\"a\", \"b\")", "`secret.declassify(v, reason)`"),
        ("hash.short(\"a\", 8)", "`str.slice(hash.sha256(s), 0, n)`"),
        ("inet.addr(\"10.0.0.0/8\", 1)", "`inet.host(net, n)`"),
        ("random.bytes(\"k\", 32)", "`random.base64(key, length)`"),
    ] {
        let e = error(&format!("p(x) where x = {call}\n"));
        assert!(e.contains(help), "{call}: {e}");
    }
}

/// Subject first, options last (R-134 rule 6): `path.join` takes a list
/// as `list.join` does, and `path.rel` the path first, its base after.
#[test]
fn a_functions_subject_comes_first() {
    let src = r#"j(s) where s = path.join(["etc", "dform", "prod.yaml"])
r(s) where s = path.rel("/etc/dform/prod.yaml", "/etc")
"#;
    assert_eq!(facts(src, "j"), [r#"j("etc/dform/prod.yaml")"#]);
    assert_eq!(facts(src, "r"), [r#"r("dform/prod.yaml")"#]);
    let e = error("p(x) where x = path.join(\"a\", \"b\")\n");
    assert!(e.contains("path.join"), "{e}");
}

/// There is no prelude (R-155): every function a program calls is named
/// by its package; the lowering's own (`__ref`, `__scoped`, `__cloud_ref`,
/// `add`) are no program's to write, and `scoped(..)` says what a program
/// writes instead. `ref(..)` and `cloud_ref(T, n, p)` stay forms of the
/// language, lowered to `__ref` and `__cloud_ref`, listed nowhere.
#[test]
fn there_is_no_prelude() {
    let bare: Vec<&str> = dform_core::functions::registry()
        .functions()
        .filter(|f| !f.internal && !f.name.contains('.'))
        .map(|f| f.name.as_str())
        .collect();
    assert!(bare.is_empty(), "{bare:?}");
    let e = error("p(x) where x = scoped(\"a\", \"b\")\n");
    assert!(
        e.contains("unknown function scoped") && e.contains("`m.x`"),
        "{e}"
    );
    for internal in ["__scoped", "__ref", "__cloud_ref"] {
        let e = error(&format!("p(x) where x = {internal}(\"a\", \"b\", \"c\")\n"));
        assert!(e.contains(&format!("{internal} is dform's own")), "{e}");
    }
    assert!(dform_core::reference::reference("ref", true).is_none());
}

/// `in` is the one membership (R-155): an element of a list, a substring
/// of a string, an address of an `inet`, a member of a range, under `not`
/// too; `inet.contains`, `list.contains` and `str.contains` are gone, an
/// error naming `in` as a call and as a predicate.
#[test]
fn in_is_the_one_membership() {
    let src = r#"let n: inet = "10.0.0.0/8"
let r: range(ip) = "10.0.0.10..=10.0.0.20"
net_in() where "10.1.2.3" in n
net_out() where not "11.1.2.3" in n
range_in() where "10.0.0.15" in r
range_out() where not "10.0.0.21" in r
sub_in() where "ell" in "hello"
sub_out() where not "z" in "hello"
list_in() where 2 in [1, 2, 3]
"#;
    for p in [
        "net_in",
        "net_out",
        "range_in",
        "range_out",
        "sub_in",
        "sub_out",
        "list_in",
    ] {
        assert_eq!(facts(src, p), [format!("{p}()")], "{p}");
    }
    // A type with no `in` names the ones that have it, and its text.
    let program =
        parse_program("let v: oci = \"ghcr.io/o/app:1\"\np() where \":\" in v\n").unwrap();
    let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
    assert!(
        e.contains("`in` takes a list, a string, an `inet` or a range")
            && e.contains("its text is `\"${v}\"`"),
        "{e}"
    );
    for (old, new) in [
        ("inet.contains(n, \"10.0.0.1\")", "`a in net`"),
        ("list.contains([1], 1)", "`v in xs`"),
        ("str.contains(\"ab\", \"a\")", "`\"x\" in s`"),
    ] {
        let e = error(&format!(
            "let n: inet = \"10.0.0.0/8\"\np(b) where b = {old}\n"
        ));
        assert!(e.contains(new), "{old}: {e}");
        let program =
            parse_program(&format!("let n: inet = \"10.0.0.0/8\"\nq() where {old}\n")).unwrap();
        let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
        assert!(
            e.contains("unknown function") && e.contains(new),
            "{old}: {e}"
        );
    }
}
