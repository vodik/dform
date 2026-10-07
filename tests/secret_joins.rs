//! A join on a secret is a read of it (After R-178): `pw(p), known(p)`
//! tests the secret against `known`'s rows as `p == "hunter2"` would, and is
//! E0301 unless `secret.declassify` says why. A secret forwarded through
//! a relation, bound once, is no test.
//!
//! An object's key may be computed, `{ "${k}": v }`: it interpolates like
//! any string, and a key is a name printed wherever the object is, so a
//! secret one is E0301; a secret value under a computed key is carried.

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

#[test]
fn a_computed_key_interpolates() {
    let r = run("resource leaky.vault v {\n  \
         tags = { \"${k}-a\": \"1\", b: \"2\", k }\n  \
         backup = { \"${k}\": pw }\n}\n")
    .success();
    for want in [
        "tags = { b: \"2\", k: \"team\", team-a: \"1\" }",
        "backup.team = (sensitive)",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

#[test]
fn a_secret_key_is_e0301() {
    refused(
        "resource leaky.vault v { backup = { \"${pw}\": \"1\" } }\n",
        "E0301: an object's key over a secret",
    );
    refused(
        "deny \"x\" { at: { \"x-${pw}\": 1 } } where k == \"team\"\n",
        "E0301: an object's key over a secret",
    );
}

#[test]
fn two_keys_alike_have_no_value() {
    let r = run("resource leaky.vault v { tags = { \"${k}\": \"1\", team: \"2\" } }\n").failure();
    assert!(
        r.stderr.contains("leaky.vault v.tags has no value"),
        "{}",
        r.stderr
    );
}
