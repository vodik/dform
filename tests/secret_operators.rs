//! Every operator and test over a secret is a read of it (R-178): `==`,
//! `!=`, an order, `in` either side, `has` and `not has`, a comparison of a
//! `where` binding, an interpolation compared, a field of a secret object.
//! Each is refused at compile time unless `secret.declassify` says why.

mod common;
use common::{Scratch, repo};

fn run(body: &str) -> common::Run {
    let s = Scratch::new("secret-ops");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.write(
        "p.df",
        &format!(
            "\ninput pw: secret(string) check pw.len >= 3\n\
             input conn: {{ host: string, password: secret(string) }} = \
             {{ host: \"h\", password: \"opensesame\" }}\n{body}\nuse fake\n"
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
    assert!(r.stderr.contains(want), "{body}\nwant {want}\n{}", r.stderr);
}

/// The old README's example compiled and ran the comparison: it is the
/// regression. `pw == "hunter2"` lowers to `pw("hunter2")`, a value at a
/// secret position, and is refused there.
#[test]
fn the_readme_comparison_is_e0301() {
    refused(
        "deny \"weak password\" where pw == \"hunter2\"\n",
        "p.df:4:28: E0301: an equality test over a secret",
    );
}

#[test]
fn equality_through_a_binding() {
    refused(
        "deny \"x\" where pw(p), p == \"hunter2\"\n",
        "E0301: an equality test over a secret",
    );
    refused(
        "deny \"x\" where p = pw, q = \"hunter2\", p == q\n",
        "E0301: an equality test over a secret",
    );
}

#[test]
fn inequality_and_order() {
    refused("deny \"x\" where pw != \"a\"\n", "E0301: a comparison");
    for op in ["<", "<=", ">", ">="] {
        refused(
            &format!("deny \"x\" where pw {op} \"m\"\n"),
            "E0301: a comparison",
        );
    }
}

#[test]
fn membership_either_side() {
    refused(
        "deny \"x\" where pw in [\"a\", \"b\"]\n",
        "E0301: member/2 over a secret",
    );
    refused(
        "deny \"x\" where \"a\" in pw\n",
        "E0301: member/2 over a secret",
    );
}

#[test]
fn definedness() {
    refused(
        "deny \"x\" where not has pw\n",
        "E0302: `not pw(...)` over a secret",
    );
    refused(
        "deny \"x\" where has pw\n",
        "E0301: a definedness test (`has`) over a secret",
    );
    refused(
        "deny \"x\" where has conn.password\n",
        "E0301: a definedness test (`has`) over a secret",
    );
    refused(
        "deny \"x\" where not has conn.password\n",
        "E0301: a definedness test (`has`) over a secret",
    );
}

#[test]
fn a_field_and_an_interpolation() {
    refused(
        "deny \"x\" where conn.password == \"opensesame\"\n",
        "E0301: an equality test over a secret",
    );
    refused(
        "deny \"x\" where \"${pw}!\" == \"hunter2!\"\n",
        "E0301: an equality test over a secret",
    );
}

/// What is not a secret is tested freely: the object's public field, its
/// presence; a declassified value may be compared; a secret forwarded
/// into a sensitive attribute is no test.
#[test]
fn what_stays_allowed() {
    let r = run("deny \"host\" where conn.host == \"elsewhere\"\n\
         deny \"no host\" where not has conn.host\n\
         deny \"declassified\" where secret.declassify(pw, \"a test of the test\") == \"nope\"\n\
         resource leaky.vault v { password = \"${pw}!\" }\n")
    .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    let r = run(
        "deny \"weak\" where secret.declassify(pw, \"the check is the point\") == \"hunter2\"\n",
    )
    .failure();
    let out = format!("{}{}", r.stdout, r.stderr);
    assert!(out.contains("- weak"), "{out}");
}
