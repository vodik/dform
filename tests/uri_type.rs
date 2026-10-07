//! `uri` (R-134): RFC 3986's generic syntax, every scheme alike, the
//! forms with no authority included; parsed where a uri is wanted and
//! held normalized, its parts fields, `with_*` per part; `url`, the
//! browser's word, is an error naming `uri`.

mod common;
use common::{error, facts};

/// Two spellings of one uri are equal, a uri never equals its string,
/// its parts are fields and JSON carries its text.
#[test]
fn a_uri_is_a_value() {
    let src = r#"let shouting: uri = "HTTPS://Example.COM:443"
let plain: uri = "https://example.com/"
let full: uri = "http://ops:s3cr%3At@h.example.com:8080/a/./b/../c?x=1&y=a%20b#top"
let bare: uri = "https://h/"
let home: uri = "https://h"
same() where shouting == plain
text() where plain == "https://example.com/"
parts(s, u, w, h, p, a, q, y, f) where s = full.scheme, u = full.user, w = full.password, h = full.host, p = full.port, a = full.path, q = full.query.x, y = full.query.y, f = full.fragment
noport(p) where p = bare.port
enc(j) where j = json.encode({ u: home })
"#;
    assert_eq!(facts(src, "same"), ["same()"]);
    assert!(facts(src, "text").is_empty());
    assert_eq!(
        facts(src, "parts"),
        [r#"parts("http", "ops", "s3cr%3At", "h.example.com", 8080, "/a/c", "1", "a b", "top")"#]
    );
    assert!(facts(src, "noport").is_empty());
    assert_eq!(facts(src, "enc"), [r#"enc("{\"u\":\"https://h/\"}")"#]);
}

/// The schemes a program holds, generic syntax all: userinfo kept, a
/// path normalized only where it is hierarchical; the forms with no
/// authority parse to a scheme and a path and print back as written.
#[test]
fn every_scheme_reads_alike() {
    for (text, scheme, host) in [
        ("s3://bucket/key/x.json", "s3", Some("bucket")),
        (
            "postgres://app@db.internal:5432/orders",
            "postgres",
            Some("db.internal"),
        ),
        ("git+ssh://git@host/o/r.git", "git+ssh", Some("host")),
        ("ssh://ops@host", "ssh", Some("host")),
        ("oci://ghcr.io/o/app", "oci", Some("ghcr.io")),
        ("file:///etc/hosts", "file", Some("")),
        ("tel:+15551234", "tel", None),
        ("mailto:ops@example.com", "mailto", None),
        ("sip:alice@host", "sip", None),
        ("urn:ietf:rfc:3986", "urn", None),
        ("data:text/plain,hi", "data", None),
    ] {
        let src = format!(
            "let u: uri = {text:?}\nt(x) where x = \"${{u}}\"\ns(x) where x = u.scheme\n\
             h() where has u.host\n"
        );
        assert_eq!(facts(&src, "t"), [format!("t({text:?})")], "{text}");
        assert_eq!(facts(&src, "s"), [format!("s({scheme:?})")], "{text}");
        assert_eq!(
            facts(&src, "h").len(),
            usize::from(host.is_some()),
            "{text}"
        );
    }
}

/// `with_*` per part, `join`, `escape`; a host given to a uri with no
/// authority adds one as RFC 3986 composes it.
#[test]
fn uri_functions_evaluate() {
    let src = r#"let a: uri = "https://example.com/a"
j(x) where x = uri.join(a, "b")
sc(x) where x = uri.with_scheme("http://h/p", "https")
us(x) where x = uri.with_user("postgres://h/db", "app")
pw(x) where x = uri.with_password("postgres://app@h/db", "p:w")
ho(x) where x = uri.with_host("http://h/p", "other")
mh(x) where x = uri.with_host("mailto:ops@example.com", "mx.example")
po(x) where x = uri.with_port("http://h/p", 8080)
dp(x) where x = uri.with_port("http://h/p", 80)
pa(x) where x = uri.with_path("http://h/p", "/q")
qu(x) where x = uri.with_query("http://h/p", { a: "1", b: "x y" })
fr(x) where x = uri.with_fragment("http://h/p", "top")
en(x) where x = uri.escape("a b/c")
"#;
    for (pred, want) in [
        ("j", "https://example.com/a/b"),
        ("sc", "https://h/p"),
        ("us", "postgres://app@h/db"),
        ("pw", "postgres://app:p%3Aw@h/db"),
        ("ho", "http://other/p"),
        ("mh", "mailto://mx.example/ops@example.com"),
        ("po", "http://h:8080/p"),
        ("dp", "http://h/p"),
        ("pa", "http://h/q"),
        ("qu", "http://h/p?a=1&b=x%20y"),
        ("fr", "http://h/p#top"),
    ] {
        assert_eq!(facts(src, pred), [format!("{pred}({want})")], "{pred}");
    }
    assert_eq!(facts(src, "en"), [r#"en("a%20b%2Fc")"#]);
}

/// A bad uri literal where a uri is wanted is a compile error saying
/// why; a bad part given to `with_*` is an error at the call (R-134
/// rule 3).
#[test]
fn a_bad_uri_is_an_error_saying_why() {
    let e = error("p(x) where x = uri.with_scheme(\"no scheme\", \"https\")\n");
    assert!(
        e.contains("is a uri") && e.contains("it has no scheme"),
        "{e}"
    );
    let e = error("let u: uri = \"http://h:99999/\"\n");
    assert!(e.contains("is no port"), "{e}");
    let program =
        dform_core::parser::parse_program("p(x) where x = uri.with_port(\"http://h/\", 70000)\n")
            .unwrap();
    let err = dform_core::engine::eval(&program, &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not defined for these arguments"), "{err}");
}

/// `url` is the browser's word: the type and the package are `uri`.
#[test]
fn url_is_an_error_naming_uri() {
    let e = error("let u: url = \"https://h\"\n");
    assert!(
        e.contains("unknown type url") && e.contains("the type is `uri`"),
        "{e}"
    );
    let e = error("p(x) where x = url.with_host(\"https://h\", \"g\")\n");
    assert!(e.contains("`uri.with_host`"), "{e}");
    let e = error("p(x) where x = url.encode(\"a b\")\n");
    assert!(e.contains("`uri.escape`"), "{e}");
}

/// A schema attribute typed `uri` (R-31): a good literal plans, a bad one
/// is a compile error at the resource.
#[test]
fn a_uri_typed_attribute_checks_its_literal() {
    let s = common::Scratch::new("uri-attr");
    s.write(
        "schema.df",
        "\ntype_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"link\", \"uri\", [])\n",
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
    assert!(r.stderr.contains("is a uri"), "{}", r.stderr);
}
