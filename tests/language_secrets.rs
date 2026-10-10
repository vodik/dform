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
            "\ninput pw: secret(string) check pw.len >= 3\n\
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
    let r = run("resource leaky.vault v {\n  password = p\n} where pw(p)\noutput token: secret(string) = p where pw(p)\nresource leaky.vault w {\n  backup = q\n} where pw(p), q = str.format(\"pw:%s\", p)\n")
    .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

#[test]
fn e0301_a_comparison_or_an_inspecting_function() {
    refused(
        "deny \"short\" where pw(p), p.len < 12\n",
        "p.df:4:1: E0301: a comparison over a secret",
    );
    refused(
        "deny \"short\" where pw(p), p != \"x\"\n",
        "E0301: a comparison over a secret",
    );
    refused(
        "n(l) where pw(p), l = p.len\n",
        "E0301: `.len` over a secret",
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
        "\ninput pw: secret(string) check pw.len >= 12\nuse fake\n",
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
            .contains("--set pw: input pw is secret(string) check pw.len >= 12\n"),
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

/// A uri's password is a secret's when it came from one (R-134, R-118):
/// the uri a secret was written into is secret, and so is its
/// `.password`; a uri of no secret is public.
#[test]
fn a_uris_password_from_a_secret_is_secret() {
    let r = run("resource leaky.vault v {\n  password = w\n} where pw(p), d = uri.with_password(\"postgres://app@db/x\", p), w = d.password\n\
                 resource leaky.oops o {\n  password = h\n} where d = uri.with_password(\"postgres://app@db/x\", \"public\"), h = d.password\n")
        .success();
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    assert!(r.stdout.contains("password = \"public\""), "{}", r.stdout);
    refused(
        "resource leaky.oops o {\n  password = w\n} where pw(p), d = uri.with_password(\"postgres://app@db/x\", p), w = d.password\n",
        "E0304",
    );
}

/// What a coeffect is asked with leaves the machine at plan (R-167): a
/// location's host and path go to DNS and the server, an extern's `+`
/// column to its provider. A secret there is E0306, refused before any
/// read; a column declared `+x: secret(T)` takes one on purpose, and a
/// declassified value is the program's to send.
#[test]
fn e0306_a_location_or_an_externs_input() {
    refused(
        "let x = io.read(\"https://example.invalid/${pw}\")\n",
        "p.df:4:9: E0306: a secret reaches a location, which is sent over the network and printed",
    );
    refused(
        "let x = io.read(\"https://${random.password(\"a\")}.example.invalid/\")\n",
        "E0306: a secret reaches a location",
    );
    refused(
        "extern lookup(+name, -id: string)\nq(i) where lookup(pw, i)\n",
        "E0306: a secret reaches lookup's argument `name`",
    );
    // `vault.read`'s `+path` is public: a secret there is refused too.
    refused(
        "q(1) where pw(p), vault.read(p, _)\n",
        "E0306: a secret reaches vault.read's argument `path`",
    );
    run("extern sealed.read(+path: secret(string), -value: string)\nq(v) where sealed.read(pw, v)\n")
        .success();
}

/// A provider's setting may take a secret (R-45: `use k8s { kubeconfig =
/// .. }`): the provider is given it at Configure, in memory, and dform
/// prints it as a secret and keeps it nowhere. It is no E0306.
#[test]
fn a_secret_setting_is_configured_and_never_printed() {
    let s = Scratch::new("lang-secrets-setting");
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    s.write(
        "p.df",
        "\ninput pw: secret(string)\nq(1)\nuse fake { token = pw }\n",
    );
    let r = s
        .run(&[
            "dev",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "--set",
            "pw=hunter2",
            "query",
            "provider_config",
            "p.df",
        ])
        .success();
    assert!(r.stdout.contains("{token: secret(7 B)}"), "{}", r.stdout);
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

/// R-109: each code's help is the fix at its site, and no help string
/// serves two codes: a check where the secret is an input's, a
/// declassify of what a function reads, the declaration an output
/// needs, the attributes a type marks sensitive.
#[test]
fn each_code_has_its_own_help_computed_from_the_site() {
    let cases = [
        (
            "E0301",
            "deny \"short\" where pw(p), p.len < 12\n",
            "check it where it is declared, `input pw: secret(string) check ..`",
        ),
        (
            "E0301",
            "copy(v) where vault.read(\"db\", v)\nn(l) where copy(p), l = p.len\n",
            "`.len` reads the secret's value: give it `secret.declassify(p, \"why\")`",
        ),
        (
            "E0302",
            "known(\"a\")\nnew(p) where pw(p), not known(p)\n",
            "whether `p` is there is a bit of it",
        ),
        (
            "E0303",
            "n(c) where c = count(p), pw(p)\n",
            "`collect_list(p)` gathers the secrets",
        ),
        (
            "E0304",
            "output token: string = p where pw(p)\n",
            "declare it secret: `output token: secret(string)`",
        ),
        (
            "E0304",
            "resource leaky.oops o {\n  password = p\n} where pw(p)\n",
            "leaky.oops .password is printed in every plan; leaky.oops marks no attribute sensitive",
        ),
        (
            "E0305",
            "resource leaky.vault \"${n}\" {\n  password = \"x\"\n} where pw(n)\n",
            "name the resource by a public value (a key, a label), not `n`",
        ),
        (
            "E0306",
            "n(v) where pw(p), vault.read(p, v)\n",
            "vault.read is asked with `path` as it is",
        ),
    ];
    let mut codes: std::collections::BTreeMap<String, String> = Default::default();
    for (code, body, want) in cases {
        let r = run(body).failure();
        // `Error: p.df:L:C: E030N: ..`, then its `Help: ..` line.
        let mut lines = r.stderr.lines();
        let mut found = false;
        while let Some(l) = lines.next() {
            if !l.starts_with("Error: ") || !l.contains(&format!("{code}:")) {
                continue;
            }
            let help = lines
                .by_ref()
                .find_map(|l| l.split_once("Help: ").map(|(_, h)| h.to_string()))
                .unwrap_or_else(|| panic!("{code} has no help:\n{}", r.stderr));
            if let Some(other) = codes.insert(help.clone(), code.to_string())
                && other != code
            {
                panic!("`{help}` serves {other} and {code}");
            }
            found |= help.contains(want);
        }
        assert!(found, "{code}: want help {want}\n{}", r.stderr);
    }
}
