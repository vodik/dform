//! A value type (`oci`, `url`, `inet`, `ip`, `time`, a quantity) given to
//! a builtin's `string` parameter is its canonical print, as it is in a
//! string column (After R-133): `not str.contains(c.image, ":")` over an
//! `oci` image. A null there is a content position (the literal waits);
//! an argument with no value is a located error naming it.

mod common;
use common::{Scratch, mock};

/// The facts of `pred` the program file `src` derives.
fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = dform_core::parser::parse_file("t.df", &format!("\n{src}"))
        .unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = dform_core::engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(dform_core::partition::fmt_atom)
        .collect()
}

/// Each value type, with a needle its print holds, one it does not, a
/// prefix of it, and a regex over it.
const VALUES: &[(&str, &str, &str, &str, &str, &str)] = &[
    (
        "oci",
        "ghcr.io/o/app:1",
        ":1",
        "@sha",
        "ghcr.io/",
        "^ghcr[.]io/o/app:1$",
    ),
    (
        "url",
        "https://example.com/a",
        "example",
        "http:",
        "https://",
        "^https://[a-z.]+/a$",
    ),
    (
        "inet",
        "10.0.0.0/16",
        "/16",
        "/24",
        "10.0.",
        "^10[.]0[.]0[.]0/16$",
    ),
    ("ip", "10.0.0.1", ".0.1", ".9", "10.", "^10[.]0[.]0[.]1$"),
    (
        "time",
        "2026-01-02T03:04:05+00:00[UTC]",
        "T03:",
        "T04:",
        "2026-",
        "^2026-01-02T03:04:05[+]00:00",
    ),
    ("bytes", "2Gi", "Gi", "Mi", "2", "^2Gi$"),
    ("duration", "1m30s", "m30", "h", "1m", "^1m30s$"),
];

/// `str.contains`, `str.starts_with` and `regex.match` over a value of
/// each type read its print, positively and under `not`.
#[test]
fn a_value_type_is_its_print_in_a_string_parameter() {
    for (ty, text, has, lacks, prefix, re) in VALUES {
        let src = format!(
            "let v: {ty} = \"{text}\"\n\
             contains() where str.contains(v, \"{has}\")\n\
             lacks() where not str.contains(v, \"{lacks}\")\n\
             starts() where str.starts_with(v, \"{prefix}\")\n\
             matches() where regex.match(v, \"{re}\")\n\
             nomatch() where not regex.match(v, \"^x\")\n\
             shown(s) where s = str.replace(v, \"{has}\", \"_\")\n"
        );
        for p in ["contains", "lacks", "starts", "matches", "nomatch"] {
            assert_eq!(facts(&src, p), [format!("{p}()")], "{ty}: {p}");
        }
        let shown = text.replacen(has, "_", 1);
        assert_eq!(
            facts(&src, "shown"),
            [format!("shown(\"{shown}\")")],
            "{ty}"
        );
    }
}

/// The program that failed `unsafe builtin predicate str.contains(...)`:
/// a deny over a resource's `oci` attribute checks its print, through the
/// type check and the plan.
#[test]
fn a_deny_over_an_oci_attribute_reads_its_print() {
    let s = Scratch::new("builtin-oci-deny");
    s.write(
        "schema.df",
        "type_provider(app.thing, \"app\")\n\
         type_attr(app.thing, \"image\", \"oci\", [])\n",
    );
    s.write(
        "p.df",
        "\n\nlet base: oci = \"nginx\"\n\
         resource app.thing pinned { image = oci.with_tag(base, \"1.27\") }\n\
         resource app.thing floating { image = base }\n\
         deny \"image not pinned: ${t}\" where t in app.thing, not str.contains(t.image, \":\")\n\
         deny \"from ghcr: ${t}\" where t in app.thing, str.starts_with(t.image, \"ghcr.io/\")\n",
    );
    let r = s
        .run(&[
            "dev",
            "--provider",
            "schema.df",
            "--world",
            "w.json",
            "plan",
            "p.df",
        ])
        .failure();
    assert!(
        r.stderr
            .contains("constraint violations:\n- image not pinned: app.thing[\"floating\"]\nError"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("unsafe"), "{}", r.stderr);
}

/// A null in a builtin's argument is a content position (Rule 2): a block
/// gated on `str.contains` of a computed value waits for it, under `not`
/// too, rather than failing the plan.
#[test]
fn a_builtin_over_a_computed_value_waits() {
    for clause in [
        "str.contains(d.endpoint, \"db\")",
        "not str.contains(d.endpoint, \"x\")",
    ] {
        let s = Scratch::new("builtin-null");
        s.write(
            "p.df",
            &format!(
                "\n\nuse fake\nresource db.postgres d {{ size = 1 }}\n\
                 resource net.vpc v {{ cidr = \"10.0.0.0/16\" }} where {clause}\n"
            ),
        );
        let r = mock(&s, &["plan"]).success();
        assert!(
            r.stdout.contains("waits on d.endpoint"),
            "{clause}: {}",
            r.stdout
        );
    }
}

/// An argument with no value in the row is an error at the literal naming
/// it, never "unsafe builtin predicate". The resolver refuses an unknown
/// name first, so the lowered rule is written by hand.
#[test]
fn an_unbound_argument_is_a_located_error() {
    use dform_core::ast::{Lit, Program, RuleStmt, Stmt, Term, atom, str_term};
    let span = Default::default();
    let x = || Term::Var("X".into());
    let program = Program {
        statements: vec![
            Stmt::Fact(atom("q", vec![str_term("a")], span)),
            Stmt::Rule(RuleStmt {
                head: atom("p", vec![x()], span),
                body: vec![
                    Lit::Pos(atom("q", vec![x()], span)),
                    Lit::Pos(atom(
                        "str.contains",
                        vec![Term::Var("ImageRef".into()), str_term("a")],
                        span,
                    )),
                ],
            }),
        ],
        stack: None,
    };
    let e = dform_core::engine::eval(&program, &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("`image_ref` is not bound here"), "{e}");
    assert!(e.starts_with("`str.contains(..)`"), "{e}");
}
