//! `oci` is a value type like `uri` (R-133): an image reference parsed at
//! the edge (a string where an `oci` is declared; no constructor), `[registry/]repository[:tag][@digest]`, its parts read as
//! fields, changed by `oci.with_tag`, `oci.with_digest` and
//! `oci.with_registry`, printed canonically, and its text where a string
//! is wanted.

mod common;
use common::error;

/// A program file's facts (a `let` and a typed position are the
/// surface's).
fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = dform_core::parser::parse_file("t.df", &format!("\n{src}"))
        .unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = dform_core::engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(dform_core::spell::atom)
        .collect()
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// A string where an `oci` is declared is read as one: two spellings of
/// one reference are equal (the default registry and its `library/` left
/// out of the text), an `oci` never equals a string, and its parts are
/// fields: `registry` absent for the default registry, `tag` and
/// `digest` absent when it has none.
#[test]
fn an_oci_is_a_value_with_fields() {
    let d = digest('a');
    let src = format!(
        r#"let long: oci = "docker.io/library/nginx:1.27"
let short: oci = "nginx:1.27"
let index: oci = "index.docker.io/org/app"
let org: oci = "org/app"
let bare: oci = "nginx"
let mine: oci = "ghcr.io/element-hq/synapse:v1.2@{d}"
let local: oci = "localhost:5000/app"
same() where long == short
same_index() where index == org
text() where bare == "nginx"
show(x) where x = long
shown(x) where x = mine
parts(g, r, t, h) where g = mine.registry, r = mine.repository, t = mine.tag, h = mine.digest
library(r) where r = bare.repository
noreg() where not has short.registry
notag() where not has local.tag
port(g) where g = local.registry
interp(s) where s = "${{short}}"
enc(j) where j = json.encode({{ i: short }})
"#
    );
    assert_eq!(facts(&src, "same"), ["same()"]);
    assert_eq!(facts(&src, "same_index"), ["same_index()"]);
    assert!(facts(&src, "text").is_empty());
    assert_eq!(facts(&src, "show"), ["show(nginx:1.27)"]);
    assert_eq!(
        facts(&src, "shown"),
        [format!("shown(ghcr.io/element-hq/synapse:v1.2@{d})")]
    );
    assert_eq!(
        facts(&src, "parts"),
        [format!(
            r#"parts("ghcr.io", "element-hq/synapse", "v1.2", "{d}")"#
        )]
    );
    assert_eq!(facts(&src, "library"), [r#"library("library/nginx")"#]);
    assert_eq!(facts(&src, "noreg"), ["noreg()"]);
    assert_eq!(facts(&src, "notag"), ["notag()"]);
    assert_eq!(facts(&src, "port"), [r#"port("localhost:5000")"#]);
    assert_eq!(facts(&src, "interp"), [r#"interp("nginx:1.27")"#]);
    assert_eq!(facts(&src, "enc"), [r#"enc("{\"i\":\"nginx:1.27\"}")"#]);
}

/// `with_tag` sets the tag and drops a digest (it pinned the old tag's
/// content); `with_digest` keeps the tag; `with_registry` moves the
/// reference, `docker.io` being the default. A string argument is read
/// as a reference. A tag, digest or registry that is not one answers
/// nothing.
#[test]
fn with_tag_with_digest_with_registry() {
    let (a, b) = (digest('a'), digest('b'));
    let src = format!(
        r#"let pinned: oci = "ghcr.io/o/app:1@{a}"
let app: oci = "app"
t(x) where x = oci.with_tag("ghcr.io/element-hq/synapse", "v1.2")
t2(x) where x = oci.with_tag(pinned, "2")
d(x) where x = oci.with_digest("ghcr.io/o/app:1", "{b}")
d2(x) where x = oci.with_digest(pinned, "{b}")
r(x) where x = oci.with_registry("nginx:1", "mirror.gcr.io")
r2(x) where x = oci.with_registry("ghcr.io/o/app", "docker.io")
"#
    );
    assert_eq!(facts(&src, "t"), ["t(ghcr.io/element-hq/synapse:v1.2)"]);
    assert_eq!(facts(&src, "t2"), ["t2(ghcr.io/o/app:2)"]);
    assert_eq!(facts(&src, "d"), [format!("d(ghcr.io/o/app:1@{b})")]);
    assert_eq!(facts(&src, "d2"), [format!("d2(ghcr.io/o/app:1@{b})")]);
    assert_eq!(facts(&src, "r"), ["r(mirror.gcr.io/library/nginx:1)"]);
    assert_eq!(facts(&src, "r2"), ["r2(o/app)"]);
    for call in [
        "oci.with_tag(app, \".x\")",
        "oci.with_digest(app, \"16.0.4\")",
        "oci.with_registry(app, \"a b\")",
    ] {
        let program = dform_core::parser::parse_file(
            "t.df",
            &format!("\nlet app: oci = \"app\"\np(x) where x = {call}\n"),
        )
        .unwrap();
        let e = dform_core::engine::eval(&program, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains(&format!("{call} is not defined")), "{e}");
    }
}

/// `oci.pinned` is `has r.digest`, a function to bool and a predicate;
/// a string read at run time that is no reference is not pinned (a
/// literal one is a compile error, the column's type `oci`).
#[test]
fn pinned_is_has_digest() {
    let d = digest('c');
    let src = format!(
        r#"image(i) where l = json.decode("[\"ghcr.io/o/app@{d}\", \"ghcr.io/o/app:1\", \"Not A Reference\"]"), i in l
pinned(i) where image(i), oci.pinned(i)
unpinned(i) where image(i), not oci.pinned(i)
b(x) where x = oci.pinned("app@{d}")
"#
    );
    assert_eq!(
        facts(&src, "pinned"),
        [format!(r#"pinned("ghcr.io/o/app@{d}")"#)]
    );
    assert_eq!(
        facts(&src, "unpinned"),
        [
            r#"unpinned("Not A Reference")"#,
            r#"unpinned("ghcr.io/o/app:1")"#
        ]
    );
    assert_eq!(facts(&src, "b"), ["b(true)"]);
}

/// A bad literal in an `oci` position is a compile error naming the
/// reference grammar; so is a short digest, a tag with a leading dot and
/// an upper-case repository.
#[test]
fn a_bad_oci_literal_is_a_compile_error() {
    for bad in [
        "Not A Reference",
        "ghcr.io/o/App",
        "app:.x",
        "app@sha256:9f2c",
        "ghcr.io/",
    ] {
        let e = error(&format!("p(x) where x = oci.with_tag(\"{bad}\", \"1\")\n"));
        assert!(e.contains("is an oci"), "{bad}: {e}");
        assert!(
            e.contains("[registry/]repository[:tag][@digest]"),
            "{bad}: {e}"
        );
    }
    let e = error("let base: oci = \"ghcr.io/o/App\"\n");
    assert!(e.contains("let base is an oci"), "{e}");
}

/// `oci.parse` is gone: the error says how a string becomes an `oci`
/// and names the fields.
#[test]
fn oci_parse_is_gone() {
    let e = error("p(x) where x = oci.parse(\"nginx\").digest\n");
    assert!(e.contains("unknown function oci.parse"), "{e}");
    assert!(e.contains("`let r: oci ="), "{e}");
    assert!(e.contains("`r.digest`"), "{e}");
}

/// An `oci` where a schema attribute takes a string is its text: the
/// image field of a workload written from a typed base and a release
/// (the R-133 "Done when"); an `oci`-typed attribute checks its literal
/// (and, as a `uri`-typed one, keeps its text).
#[test]
fn an_oci_in_a_string_attribute_is_its_text() {
    let s = common::Scratch::new("oci-attr");
    s.write(
        "schema.df",
        "\ntype_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"image\", \"string\", [])\n\
         type_attr(app.thing, \"ref\", \"oci\", [])\n",
    );
    let run = |body: &str| {
        s.write("p.df", &format!("\n{body}"));
        s.run(&common::on(
            "p.df",
            &["--provider", "schema.df", "--world", "w.json"],
            &["plan"],
        ))
    };
    let r = run("let base: oci = \"ghcr.io/element-hq/synapse\"\n\
                 let release = \"v1.2\"\n\
                 resource app.thing t { image = oci.with_tag(base, release) }\n")
    .success();
    assert!(
        r.stdout
            .contains("image = \"ghcr.io/element-hq/synapse:v1.2\""),
        "{}",
        r.stdout
    );
    // A string-typed `let` takes one too: its column is the reference's.
    let r = run("let img: oci = \"docker.io/library/nginx:1\"\n\
                 let s: string = img\n\
                 resource app.thing t { image = s }\n")
    .success();
    assert!(r.stdout.contains("image = \"nginx:1\""), "{}", r.stdout);
    run("resource app.thing t { ref = \"docker.io/library/nginx:1\" }\n").success();
    let r = run("resource app.thing t { ref = \"Nope Nope\" }\n").failure();
    assert!(r.stderr.contains("is an oci"), "{}", r.stderr);
}

/// An input typed `oci` reads its default and a `--set` string as one.
#[test]
fn an_oci_input() {
    let s = common::Scratch::new("oci-input");
    s.write(
        "p.df",
        "\ninput image: oci = \"docker.io/library/nginx:1\"\n\
         resource app.thing t { repo = image.repository, image = \"${image}\" }\n",
    );
    s.write(
        "schema.df",
        "\ntype_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"repo\", \"string\", [])\n\
         type_attr(app.thing, \"image\", \"string\", [])\n",
    );
    let mock = ["--provider", "schema.df", "--world", "w.json"];
    let r = s.run(&common::on("p.df", &mock, &["plan"])).success();
    assert!(
        r.stdout.contains("repo = \"library/nginx\""),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("image = \"nginx:1\""), "{}", r.stdout);
    let r = s
        .run(&common::on(
            "p.df",
            &mock,
            &["plan", "--set", "image=ghcr.io/o/app:2"],
        ))
        .success();
    assert!(
        r.stdout.contains("image = \"ghcr.io/o/app:2\""),
        "{}",
        r.stdout
    );
}
