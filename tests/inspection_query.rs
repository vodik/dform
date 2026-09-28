//! `dform query`: patterns and conjunctions over the final fact store,
//! printed as a table with one column per variable.

mod common;
mod inspection_common;
use common::{Scratch, repo};
use inspection_common::{dform, golden};

#[test]
fn a_pattern_prints_one_column_per_variable() {
    let out = dform(
        "dform.df",
        &["--set", "env=prod", "query", "attr(net.vpc, N, .cidr, C)"],
    );
    assert!(out.starts_with("N "), "{out}");
    assert!(
        out.contains(r#""network.main::vpc"  10.20.0.0/16"#),
        "{out}"
    );
    assert!(out.ends_with("(2 rows)\n"), "{out}");
    golden("query_dform_prod_vpc_cidr", &out);
}

#[test]
fn a_conjunction_is_evaluated_against_the_final_fact_store() {
    let out = dform(
        "dform.df",
        &[
            "--set",
            "env=prod",
            "query",
            "attr(T, A, .cidr, C), want(T, A), T != net.subnet",
        ],
    );
    assert!(out.starts_with("T "), "{out}");
    assert!(
        out.contains(r#""net.vpc""#) && !out.contains(r#""net.subnet""#),
        "{out}"
    );
    let ground = dform(
        "dform.df",
        &["query", r#"want(net.vpc, "network.main::vpc")"#],
    );
    assert_eq!(ground, "yes\n");
}

/// A bare predicate name keeps working, printed as facts, not Debug.
#[test]
fn a_predicate_name_lists_its_facts() {
    let out = dform("dform.df", &["query", "env"]);
    assert_eq!(out, "env(\"staging\")\nmatches: 1\n");
}

#[test]
fn a_query_that_does_not_parse_says_so() {
    let s = Scratch::new("query-parse");
    let file = repo().join("dform.df");
    let r = s
        .run(&[
            "--file",
            file.to_str().unwrap(),
            "--world",
            "w.json",
            "query",
            "attr(T,",
        ])
        .failure();
    assert!(
        r.stderr.contains("cannot parse query 'attr(T,'"),
        "{}",
        r.stderr
    );
}

/// The never-prints claim for query: a secret prints as its label in a
/// table cell, in a listed fact, and where a rule forwarded it.
#[test]
fn query_never_prints_a_labeled_secret() {
    let s = Scratch::new("query-secret");
    s.write(
        "p.df",
        r#"edition 2026
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }
           note(n) if attr(leaky.vault, "v", "password", p), n = concat("pw is ", p)"#,
    );
    let schema = repo().join("providers/leaky/schema.df");
    let q = |pattern: &str| {
        s.run(&[
            "--file",
            "p.df",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "query",
            pattern,
        ])
        .success()
        .stdout
    };
    for pattern in ["attr(leaky.vault, v, password, P)", "arg", "note(N)"] {
        let out = q(pattern);
        assert!(!out.contains("VAULT-SECRET"), "{pattern}: {out}");
        assert!(
            out.contains("(sensitive leaky.vault/v#password)"),
            "{pattern}: {out}"
        );
    }
}
