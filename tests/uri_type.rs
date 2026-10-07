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

/// IDNA is a wire encoding (R-134): a host is held and printed as
/// written and equal by its A-labels, so `bücher.example` and
/// `xn--bcher-kva.example` are one host.
#[test]
fn a_unicode_host_is_one_host_with_its_a_labels() {
    let src = r#"let written: uri = "https://Bücher.example/shop"
let wire: uri = "https://xn--bcher-kva.example/shop"
same() where written == wire
host(h) where h = written.host
text(t) where t = "${written}"
"#;
    assert_eq!(facts(src, "same"), ["same()"]);
    assert_eq!(facts(src, "host"), [r#"host("bücher.example")"#]);
    assert_eq!(
        facts(src, "text"),
        [r#"text("https://bücher.example/shop")"#]
    );
}

fn mock(s: &common::Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on(
        "p.df",
        &["--provider", "schema.df", "--world", "w.json"],
        args,
    ))
}

/// The provider receives the A-labels and never the Unicode form; what
/// it holds, read back, is the program's host, so a second plan changes
/// nothing; the plan prints both forms of the host, uncoloured.
#[test]
fn a_provider_receives_a_labels_and_a_round_trip_is_no_change() {
    let s = common::Scratch::new("uri-idna");
    s.write(
        "schema.df",
        "\ntype_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"link\", \"uri\", [])\n",
    );
    s.write(
        "p.df",
        "\nresource app.thing t { link = \"https://bücher.example/shop\" }\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("link = \"https://bücher.example/shop\"  https://xn--bcher-kva.example/shop"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains('\u{1b}'), "{}", r.stdout);
    mock(&s, &["apply", "--yes"]).success();
    let world = std::fs::read_to_string(s.dir.join("w.json")).unwrap();
    assert!(
        world.contains("xn--bcher-kva.example") && !world.contains("bücher"),
        "{world}"
    );
    let r = mock(&s, &["plan"]).success();
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
    // A host read back that is not the program's prints as read, never
    // decoded: the change from the world's to the program's.
    s.write(
        "p.df",
        "\nresource app.thing t { link = \"https://bücher.example/books\" }\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "link = \"https://xn--bcher-kva.example/shop\" → \"https://bücher.example/books\"  \
             https://xn--bcher-kva.example/books"
        ),
        "{}",
        r.stdout
    );
}

/// A plan is the review surface (R-134): a host with a label that is not
/// ASCII prints both forms at every level, `-q` and `--json` included,
/// and a label mixing scripts, or wholly in one confusable with Latin, is
/// a warning naming it and its attribute, kept at `-q`; the same for a
/// `--set`, a document's row and a literal.
#[test]
fn a_confusable_host_is_a_warning_at_every_level() {
    let s = common::Scratch::new("uri-confusable");
    s.write(
        "hosts.csv",
        "name,host\nshop,\u{440}\u{430}\u{443}.example\n",
    );
    s.write(
        "p.df",
        "\ninput host: string = \"example.com\"\ninput site from csv(\"hosts.csv\")\nuse fake\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  tags = { host }\n}\n\
         resource net.vpc shop {\n  cidr = \"10.1.0.0/16\"\n  tags = { host: h }\n} where site(_, h)\n\
         resource net.vpc books {\n  cidr = \"10.2.0.0/16\"\n  tags = { host: \"bücher.example\" }\n}\n",
    );
    let cyrillic_a = "ex\u{430}mple.com";
    let set = format!("host={cyrillic_a}");
    for level in [&[][..], &["-q"][..], &["-vv"][..]] {
        let mut args = vec!["plan", "p.df", "--set", &set];
        args.extend_from_slice(level);
        let r = s.run(&args).success();
        let out = &r.stdout;
        assert!(
            out.contains(&format!("\"{cyrillic_a}\"  xn--exmple-4nf.com")),
            "{level:?}: {out}"
        );
        assert!(
            out.contains("\"bücher.example\"  xn--bcher-kva.example"),
            "{level:?}: {out}"
        );
        assert!(
            out.contains(
                "host label \"ex\u{430}mple\" mixes Latin and Cyrillic  net.vpc main.tags.host"
            ),
            "{level:?}: {out}"
        );
        assert!(
            out.contains("host label \"\u{440}\u{430}\u{443}\" is Cyrillic that reads as the Latin `pay`  net.vpc shop.tags.host"),
            "{level:?}: {out}"
        );
        assert!(!out.contains("host label \"bücher\""), "{level:?}: {out}");
        assert!(!out.contains('\u{1b}'), "{level:?}: {out}");
    }
    let r = s.run(&["plan", "p.df", "--json", "--set", &set]).success();
    assert!(
        r.stdout.contains("\"host_ascii\": \"xn--exmple-4nf.com\"")
            && r.stdout.contains("\"confusable_hosts\""),
        "{}",
        r.stdout
    );
}
