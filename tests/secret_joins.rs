//! A join on a secret is a read of it (After R-178): `pw(p), known(p)`
//! tests the secret against `known`'s rows as `p == "hunter2"` would, and is
//! E0301 unless `secret.declassify` says why. A secret forwarded through
//! a relation, bound once, is no test.

mod common;
use common::{Scratch, repo};

fn run(body: &str) -> common::Run {
    let s = Scratch::new("secret-joins");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.write(
        "p.df",
        &format!(
            "\ninput pw: secret(string) check pw.len >= 3\n\
             input k: string = \"team\"\n{body}\nuse fake\n"
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

#[test]
fn a_join_on_a_secret_is_e0301() {
    refused(
        "known(\"hunter2\")\ndeny \"x\" where pw(p), known(p)\n",
        "p.df:5:23: E0301: a join over a secret",
    );
    // Two secrets joined: whether they are equal is a bit of each.
    refused(
        "copy(p) where pw(p)\nsame(1) where copy(p), pw(p)\n",
        "E0301: a join over a secret",
    );
    // A pattern's field is a join too.
    refused(
        "known({ v: \"hunter2\" })\ndeny \"x\" where pw(p), known({ v: p })\n",
        "E0301: a join over a secret",
    );
}

/// Declassified, the value joined is public; a secret bound once and
/// forwarded is no test.
#[test]
fn a_declassified_join_and_a_forward_stay_allowed() {
    let r = run("known(\"hunter2\")\n\
         deny \"leaked\" where pw(p), known(secret.declassify(p, \"a test of the test\"))\n\
         copy(p) where pw(p)\n\
         resource leaky.vault v { password = p } where copy(p)\n")
    .failure();
    let out = format!("{}{}", r.stdout, r.stderr);
    assert!(out.contains("- leaked"), "{out}");
    assert!(!out.contains("E0301"), "{out}");
    assert!(out.contains("password = (sensitive)"), "{out}");
}
