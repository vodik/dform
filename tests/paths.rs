//! A quoted path segment carries any character (R-77): a Kubernetes
//! annotation's key is one key, written `metadata.annotations."a.b/c"`,
//! read the same way, and printed quoted wherever a path is (the plan,
//! `why`). The plan file's round trip is in
//! tests/plan_file.rs.

mod common;
use common::{Scratch, mock};

const KEY: &str = "traefik.ingress.kubernetes.io/router.tls.certresolver";

const PROGRAM: &str = "\n\nprovider k8s\n\n\
    resource k8s.namespace ns {\n\
    \x20 metadata.name = \"ns\"\n\
    \x20 metadata.annotations.\"traefik.ingress.kubernetes.io/router.tls.certresolver\" = \"letsencrypt\"\n\
    }\n\
    resource k8s.config_map cm {\n\
    \x20 metadata.name = \"cm\"\n\
    \x20 metadata.namespace = ns.metadata.name\n\
    \x20 data.resolver = ns.metadata.annotations.\"traefik.ingress.kubernetes.io/router.tls.certresolver\"\n\
    }\n";

fn world(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.json")).unwrap()
}

/// The object a world holds for `name`.
fn object<'a>(w: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    w["resources"]
        .as_object()
        .unwrap()
        .values()
        .find(|o| o["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {w}"))
}

/// The write is one key of the annotations, planned quoted; once applied
/// the world holds it as that key, and the plan is undeformed.
#[test]
fn a_quoted_segment_writes_one_key() {
    let s = Scratch::new("paths-write");
    s.write("p.df", PROGRAM);
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(&format!(
            "+ k8s.namespace[\"ns\"]\n  metadata.annotations.\"{KEY}\" = \"letsencrypt\"\n"
        )),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  data.resolver = \"letsencrypt\"\n"),
        "{}",
        r.stdout
    );
    mock(&s, &["apply"]).success();
    let w = world(&s);
    let ns = object(&w, "ns");
    assert_eq!(
        ns["attrs"]["metadata"]["annotations"][KEY], "letsencrypt",
        "{w}"
    );
    let r = mock(&s, &["plan"]).success();
    assert!(r.stdout.contains("is undeformed"), "{}", r.stdout);
}

/// The read is the one key's value.
#[test]
fn a_quoted_segment_reads_one_key() {
    let s = Scratch::new("paths-read");
    s.write("p.df", PROGRAM);
    mock(&s, &["apply"]).success();
    let w = world(&s);
    assert_eq!(
        object(&w, "cm")["attrs"]["data"]["resolver"],
        "letsencrypt",
        "{w}"
    );
    let q = mock(
        &s,
        &[
            "query",
            &format!("v = n.metadata.annotations.\"{KEY}\", n in k8s.namespace"),
        ],
    )
    .success();
    assert!(q.stdout.contains("letsencrypt"), "{}", q.stdout);
    // An object's key read through a quoted segment, not a walk of `a`, `b`.
    s.write(
        "p.df",
        "\n\nprovider k8s\n\nlet anns = { \"a.b/c\": \"one\" }\n\
         resource k8s.namespace ns {\n  metadata.name = \"ns\"\n  \
         metadata.labels.x = anns.\"a.b/c\"\n}\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("  metadata.labels.x: <none> -> \"one\"\n")
            && r.stdout.contains(&format!(
                "  metadata.annotations.\"{KEY}\": \"letsencrypt\" -> <none>\n"
            )),
        "{}",
        r.stdout
    );
}

/// `why` takes the path as the plan prints it and finds the contribution.
#[test]
fn why_takes_a_quoted_segment() {
    let s = Scratch::new("paths-why");
    s.write("p.df", PROGRAM);
    let r = mock(
        &s,
        &[
            "why",
            &format!("k8s.namespace[\"ns\"].metadata.annotations.\"{KEY}\""),
        ],
    )
    .success();
    assert!(
        r.stdout.contains(&format!(
            "{{annotations: {{{KEY}: \"letsencrypt\"}}}}   p.df:7"
        )),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("{name: \"ns\"}"), "{}", r.stdout);
}

/// fmt keeps the quoted segment, and folds siblings into the object form
/// (R-52), which plans the same.
#[test]
fn fmt_keeps_a_quoted_segment() {
    let s = Scratch::new("paths-fmt");
    s.write("p.df", PROGRAM);
    s.run(&["fmt", "p.df"]).success();
    assert!(
        s.read("p.df").contains(&format!(
            "  metadata.annotations.\"{KEY}\" = \"letsencrypt\"\n"
        )),
        "{}",
        s.read("p.df")
    );
    let siblings = "\n\nprovider k8s\n\n\
        resource k8s.namespace ns {\n\
        \x20 metadata.name = \"ns\"\n\
        \x20 metadata.annotations.\"a.b/c\" = \"1\"\n\
        \x20 metadata.annotations.\"d.e/f\" = \"2\"\n\
        }\n";
    s.write("p.df", siblings);
    let before = mock(&s, &["plan"]).success().stdout;
    assert!(
        before.contains(
            "  metadata.annotations.\"a.b/c\" = \"1\"\n  metadata.annotations.\"d.e/f\" = \"2\"\n"
        ),
        "{before}"
    );
    s.run(&["fmt", "p.df"]).success();
    assert!(
        s.read("p.df")
            .contains("  metadata.annotations = { \"a.b/c\": \"1\", \"d.e/f\": \"2\" }\n"),
        "{}",
        s.read("p.df")
    );
    assert_eq!(mock(&s, &["plan"]).success().stdout, before);
}
