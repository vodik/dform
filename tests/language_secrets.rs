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
            "\ninput pw: secret(string) check len(pw) >= 3\n\
             extern vault.read(+path, -value: secret(string))\n{body}\nuse fake\n"
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
    let r = run("resource leaky.vault v {\n  password = p\n} where pw(p)\noutput token: secret(string) = p where pw(p)\nresource leaky.vault w {\n  backup = q\n} where pw(p), q = format(\"pw:%s\", p)\n")
    .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

#[test]
fn e0301_a_comparison_or_an_inspecting_function() {
    refused(
        "deny \"short\" where pw(p), len(p) < 12\n",
        "p.df:4:1: E0301: a comparison over a secret",
    );
    refused(
        "deny \"short\" where pw(p), p != \"x\"\n",
        "E0301: a comparison over a secret",
    );
    refused(
        "n(l) where pw(p), l = len(p)\n",
        "E0301: len() over a secret",
    );
}

#[test]
fn e0302_a_negation() {
    refused(
        "known(\"a\")\nnew(p) where pw(p), not known(p)\n",
        "E0302: `not known(...)` over a secret",
    );
}

#[test]
fn e0303_a_count() {
    refused(
        "n(c) where c = count(p), pw(p)\n",
        "E0303: count() over a secret",
    );
}

#[test]
fn e0304_a_public_place() {
    refused(
        "resource leaky.oops o {\n  password = p\n} where pw(p)\n",
        "p.df:5:3: E0304: a secret reaches leaky.oops .password, not marked sensitive in the schema",
    );
    refused(
        "warn \"pw\" { p: p } where pw(p)\n",
        "E0304: a secret reaches a warn message or context",
    );
    refused(
        "output token: string = p where pw(p)\n",
        "E0304: a secret reaches output token, not declared secret(T)",
    );
    // Through a derived relation and an extern's secret column.
    refused(
        "copy(v) where vault.read(\"db\", v)\nresource leaky.oops o {\n  password = v\n} where copy(v)\n",
        "E0304: a secret reaches leaky.oops .password",
    );
}

#[test]
fn e0305_a_name() {
    refused(
        "resource leaky.vault \"${n}\" {\n  password = \"x\"\n} where pw(n)\n",
        "E0305: a secret reaches a resource address",
    );
}

/// The refinement is checked, and its deny does not print the value.
#[test]
fn a_secret_input_refinement_does_not_print_it() {
    let s = Scratch::new("lang-secrets-refine");
    s.write(
        "p.df",
        "\ninput pw: secret(string) check len(pw) >= 12\nuse fake\n",
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

/// `forwards` by content (R-134 rule 5): an encoding, a `min`, a slice
/// carries a secret to a sensitive place as the secret itself does; a
/// judgment of one (`str.starts_with`, a digest) inspects it.
#[test]
fn a_secret_flows_through_its_content_and_not_through_a_judgment() {
    let r = run(
        "resource leaky.vault v {\n  password = e\n} where pw(p), e = base64.encode(p)\n\
                 resource leaky.vault w {\n  password = m\n} where pw(p), m = list.min([p, p])\n\
                 resource leaky.vault x {\n  password = s\n} where pw(p), s = str.slice(p, 0, 2)\n",
    )
    .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    refused(
        "resource leaky.oops v {\n  password = e\n} where pw(p), e = base64.encode(p)\n",
        "E0304",
    );
    refused(
        "deny \"x\" where pw(p), str.starts_with(p, \"h\")\n",
        "E0301",
    );
    refused(
        "n(d) where pw(p), d = hash.sha256(p)\n",
        "E0301: hash.sha256() over a secret",
    );
}
