//! `dform query`: patterns and conjunctions over the final fact store,
//! printed as a table with one column per variable.

mod common;
mod inspection_common;
use common::{Scratch, repo};
use inspection_common::{dform, golden};

#[test]
fn a_pattern_prints_one_column_per_variable() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["query", "attr(net.vpc, N, \"cidr\", C)"],
    );
    assert!(out.starts_with("N "), "{out}");
    assert!(out.contains(r#""main.vpc"  10.20.0.0/16"#), "{out}");
    assert_eq!(out.lines().count(), 3, "{out}");
    golden("query_dform_prod_vpc_cidr", &out);
}

#[test]
fn a_conjunction_is_evaluated_against_the_final_fact_store() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &[
            "query",
            "attr(T, A, \"cidr\", C), want(T, A), T != net.subnet",
        ],
    );
    assert!(out.starts_with("T "), "{out}");
    assert!(
        out.contains(r#""net.vpc""#) && !out.contains(r#""net.subnet""#),
        "{out}"
    );
    let ground = dform(
        "examples/demo/stacks/dform.df",
        &["query", r#"want(net.vpc, "main.vpc")"#],
    );
    assert_eq!(ground, "yes\n");
}

/// A bare predicate name lists its facts as a result set: columns named
/// by its `decl`, a core relation's names, else `a`, `b`, ..
#[test]
fn a_predicate_name_lists_its_facts() {
    let out = dform("examples/demo/stacks/dform.df", &["query", "env"]);
    assert_eq!(out, "a\n\"staging\"\n");
    let out = dform("examples/demo/stacks/dform.df", &["query", "want"]);
    assert!(out.starts_with("type "), "{out}");
    assert!(out.ends_with(" rows)\n"), "{out}");
}

#[test]
fn a_query_that_does_not_parse_says_so() {
    let s = Scratch::new("query-parse");
    let file = repo().join("examples/demo/stacks/dform.df");
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "query",
            "attr(T,",
            file.to_str().unwrap(),
        ])
        .failure();
    assert!(
        r.stderr.contains("cannot parse query 'attr(T,'"),
        "{}",
        r.stderr
    );
}

/// The never-prints claim for query: a secret prints as its size in a
/// table cell, in a listed fact, and where a rule forwarded it.
#[test]
fn query_never_prints_a_labeled_secret() {
    let s = Scratch::new("query-secret");
    s.write(
        "p.df",
        r#"
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }
           note(n) where p = v.password, n = format("pw is %s", p)"#,
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let q = |pattern: &str| {
        s.run(&[
            "dev",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "query",
            pattern,
            "p.df",
        ])
        .success()
        .stdout
    };
    for pattern in ["attr(leaky.vault, v, password, P)", "arg", "note(N)"] {
        let out = q(pattern);
        assert!(!out.contains("VAULT-SECRET"), "{pattern}: {out}");
        assert!(out.contains("secret("), "{pattern}: {out}");
    }
}

/// Rows print values as the program writes them: a reference is the
/// address it names as the plan prints it, `T a.p` (R-111), not the
/// core's `ref(T, A, p)`.
#[test]
fn a_row_prints_a_reference_as_its_address() {
    let at = "examples/crud-api/stacks/crud_api.df";
    let out = dform(
        at,
        &[
            "query",
            r#"attr("google.sql_database", "crud_db", "instance", V)"#,
        ],
    );
    assert_eq!(out, "V\ngoogle.sql_database_instance db.name\n");
    let facts = dform(at, &["query", "arg"]);
    assert!(
        facts
            .lines()
            .any(|l| l.starts_with(r#""google.sql_database""#)
                && l.contains(r#""instance""#)
                && l.contains(r#"  google.sql_database_instance db.name  "#)),
        "{facts}"
    );
    assert!(!facts.contains("ref("), "{facts}");
}

/// An address's path below its top attribute reads the field out of the
/// attribute's object (After R-124): before, no `attr` row was at it and
/// the query said nothing.
#[test]
fn a_path_below_the_top_attribute_is_its_field() {
    let out = dform(
        "examples/demo/stacks/dform.df",
        &["query", r#"net.vpc["main.vpc"].tags.team"#],
    );
    assert_eq!(out, "\"platform\"\n");
}
