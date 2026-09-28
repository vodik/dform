//! Static secret labels (E DR-19): one dataflow pass over predicate
//! signatures; a secret reaching a comparison, a negation, a counting
//! aggregate, a public place or a name is a compile error with a span.

mod common;
use common::{Scratch, repo};

fn run(body: &str) -> common::Run {
    let s = Scratch::new("lang-secrets");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.write(
        "p.df",
        &format!(
            "edition 2026\ninput pw: secret(string) where len(pw) >= 3\n\
             extern vault.read(+path, -value: secret(string))\n{body}"
        ),
    );
    s.run(&[
        "dev",
        "--provider",
        schema.to_str().unwrap(),
        "--world",
        "w.json",
        "--set",
        "pw=hunter2",
        "plan",
        "p.df",
    ])
}

fn refused(body: &str, want: &str) {
    let r = run(body).failure();
    assert!(r.stderr.contains(want), "want {want}\n{}", r.stderr);
    assert!(!r.stderr.contains("hunter2"), "{}", r.stderr);
}

/// A secret goes where the schema says it may: a sensitive attribute, a
/// secret output; its own refinement may inspect it.
#[test]
fn a_secret_flows_to_sensitive_places() {
    let r = run("resource leaky.vault v {\n  for pw(p)\n  password = p\n}\noutput token: secret(string)\noutput(\"token\", p) if pw(p)\nresource leaky.vault w {\n  for pw(p), q = format(\"pw:%s\", p)\n  backup = q\n}\n")
    .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

#[test]
fn e0301_a_comparison_or_an_inspecting_function() {
    refused(
        "deny \"short\" if pw(p), len(p) < 12\n",
        "p.df:4:1: E0301: a comparison over a secret",
    );
    refused(
        "deny \"short\" if pw(p), p != \"x\"\n",
        "E0301: a comparison over a secret",
    );
    refused("n(l) if pw(p), l = len(p)\n", "E0301: len() over a secret");
}

#[test]
fn e0302_a_negation() {
    refused(
        "known(\"a\")\nnew(p) if pw(p), not known(p)\n",
        "E0302: `not known(...)` over a secret",
    );
}

#[test]
fn e0303_a_count() {
    refused("n(count(p)) if pw(p)\n", "E0303: count() over a secret");
}

#[test]
fn e0304_a_public_place() {
    refused(
        "resource leaky.oops o {\n  for pw(p)\n  password = p\n}\n",
        "p.df:6:3: E0304: a secret reaches leaky.oops .password, not marked sensitive in the schema",
    );
    refused(
        "warn \"pw\" { p: p } if pw(p)\n",
        "E0304: a secret reaches a warn message or context",
    );
    refused(
        "output token: string\noutput(\"token\", p) if pw(p)\n",
        "E0304: a secret reaches output token, not declared secret(T)",
    );
    // Through a derived relation and an extern's secret column.
    refused(
        "copy(v) if vault.read(\"db\", v)\nresource leaky.oops o {\n  for copy(v)\n  password = v\n}\n",
        "E0304: a secret reaches leaky.oops .password",
    );
}

#[test]
fn e0305_a_name() {
    refused(
        "resource leaky.vault n {\n  for pw(n)\n  password = \"x\"\n}\n",
        "E0305: a secret reaches a resource address",
    );
}

/// The refinement is checked, and its deny does not print the value.
#[test]
fn a_secret_input_refinement_does_not_print_it() {
    let s = Scratch::new("lang-secrets-refine");
    s.write(
        "p.df",
        "edition 2026\ninput pw: secret(string) where len(pw) >= 12\n",
    );
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "--set",
            "pw=hunter2",
            "plan",
            "p.df",
        ])
        .failure();
    assert!(
        r.stderr
            .contains("input pw fails its refinement: len(pw) >= 12 ctx={}"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("hunter2"), "{}", r.stderr);
}
