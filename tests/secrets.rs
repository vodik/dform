//! Tests the "never prints" claim (README, "Computed values come from
//! Apply": *"A `sensitive` computed value never leaves the provider... A
//! value at a `sensitive` path the program sets prints as `(sensitive)`."*).
//!
//! providers/leaky/schema.df has two near-identical types: `leaky.vault`
//! declares `password` `[sensitive]`, `leaky.oops` mislabels the same kind
//! of value as public (no flags). A program sets a literal, distinctive
//! password string on one resource of each type and this test drives it
//! through every surface dform prints or persists to, checking whether the
//! literal bytes ever appear.
//!
//! Expected shape of the result: `leaky.vault`'s secret never appears in
//! anything dform prints (plan, state.json) — that's the claim. It DOES
//! appear in world.json: that file is the fake provider's own storage
//! (src/fakecloud.rs: "A sensitive computed value stays in the world"),
//! kept by the mock provider's own process, the mock equivalent of the
//! real cloud's own database, not a `dform`-facing surface, so this is by
//! design, not a leak. `leaky.oops`'s secret, mislabeled public, leaks
//! through plan/apply output too — expected, it's what mislabeling means.
//!
//! `show`, `query`, `plan --json`, `query --json` and the policy
//! messages on stderr print through one redactor (src/query.rs) and never
//! print the labeled secret, also where a rule forwards it into another
//! resource's attribute (ticket "show and query print secrets
//! unredacted").

mod common;
use common::{Scratch, repo};

const VAULT_SECRET: &str = "VAULT-SECRET-DO-NOT-PRINT";
const OOPS_SECRET: &str = "OOPS-SECRET-DO-NOT-PRINT";

const PROGRAM: &str = r#"edition 2026

resource leaky.vault v {
  password = "VAULT-SECRET-DO-NOT-PRINT"
}

resource leaky.oops o {
  password = "OOPS-SECRET-DO-NOT-PRINT"
}
"#;

fn schema() -> String {
    repo()
        .join("providers/leaky/schema.df")
        .to_str()
        .unwrap()
        .to_string()
}

#[test]
fn plan_and_apply_redact_the_labeled_secret_but_not_the_mislabeled_one() {
    let s = Scratch::new("secrets-plan");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let args = ["--file", "p.df", "--provider", &schema, "--world", "w.json"];

    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert!(
        !r.stdout.contains(VAULT_SECRET),
        "vault's labeled secret leaked in plan output:\n{}",
        r.stdout
    );
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    // The mislabeled type has no way to know it should hide this: the
    // schema says it's public, so it prints like any other attribute.
    assert!(
        r.stdout.contains(OOPS_SECRET),
        "expected the mislabeled type's value to print in plan (that's what \
         mislabeling it public means), but it didn't:\n{}",
        r.stdout
    );

    let r = s.run(&[&args[..], &["apply"]].concat()).success();
    assert!(!r.stdout.contains(VAULT_SECRET), "{}", r.stdout);

    // Drift and re-plan: the update-side diff must redact both sides too.
    s.write(
        "p.df",
        &PROGRAM
            .replace(VAULT_SECRET, "VAULT-SECRET-CHANGED")
            .replace(OOPS_SECRET, "OOPS-SECRET-CHANGED"),
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert!(
        !r.stdout.contains(VAULT_SECRET) && !r.stdout.contains("VAULT-SECRET-CHANGED"),
        "vault's labeled secret leaked in an update diff:\n{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(r#"password: (sensitive) -> (sensitive)"#),
        "{}",
        r.stdout
    );
}

#[test]
fn state_file_never_carries_either_secret() {
    // state.json is the address -> remote-name identity mapping (see
    // state::State); it never stores attribute values at all, labeled or
    // not, so this holds for both types regardless of the schema flag.
    let s = Scratch::new("secrets-state");
    s.write("p.df", PROGRAM);
    let schema = schema();
    s.run(&[
        "--file",
        "p.df",
        "--provider",
        &schema,
        "--world",
        "w.json",
        "apply",
    ])
    .success();
    let state = s.read("w.state.json");
    assert!(!state.contains(VAULT_SECRET), "{state}");
    assert!(!state.contains(OOPS_SECRET), "{state}");
}

#[test]
fn world_file_is_the_providers_own_storage_and_holds_both() {
    // Documented, not a leak: src/fakecloud.rs says the world file holds
    // secrets, because it stands in for the real cloud's
    // storage. This test pins that on purpose, as the boundary of the
    // never-prints claim: it is about what dform itself prints and
    // persists as the *consumer*, not the mock backend's storage.
    let s = Scratch::new("secrets-world");
    s.write("p.df", PROGRAM);
    let schema = schema();
    s.run(&[
        "--file",
        "p.df",
        "--provider",
        &schema,
        "--world",
        "w.json",
        "apply",
    ])
    .success();
    let world = s.read("w.json");
    assert!(world.contains(VAULT_SECRET), "{world}");
    assert!(world.contains(OOPS_SECRET), "{world}");
}

/// `show` prints the resource's attributes through the redactor.
#[test]
fn show_never_prints_the_labeled_secret() {
    let s = Scratch::new("secrets-show");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "show",
            "leaky.vault",
            "v",
        ])
        .success();
    assert!(!r.stdout.contains(VAULT_SECRET), "{}", r.stdout);
    assert!(
        r.stdout
            .contains(r#""password": {"sensitive": "leaky.vault/v#password"}"#)
            || r.stdout
                .contains("\"sensitive\": \"leaky.vault/v#password\""),
        "{}",
        r.stdout
    );
}

/// `query` prints facts through the redacting printer (src/query.rs).
#[test]
fn query_never_prints_the_labeled_secret() {
    let s = Scratch::new("secrets-query");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "query",
            "arg",
        ])
        .success();
    assert!(!r.stdout.contains(VAULT_SECRET), "{}", r.stdout);
}

/// A rule that would copy the secret into another resource's public
/// attribute is refused before evaluation (E DR-19's static pass, E0304),
/// and the error does not print it either.
#[test]
fn a_secret_into_a_public_attribute_is_a_compile_error() {
    let s = Scratch::new("secrets-e0304");
    s.write(
        "p.df",
        &format!(
            "{PROGRAM}
resource leaky.oops copy {{
  if p = v.password
  password = p
}}
"
        ),
    );
    let schema = schema();
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "plan",
        ])
        .failure();
    assert!(
        r.stderr.contains(
            "p.df:13:3: E0304: a secret reaches leaky.oops .password, not marked sensitive in the schema"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains(VAULT_SECRET), "{}", r.stderr);
    assert!(!s.path("w.json").exists(), "refused before anything ran");
}

/// A rule that copies the secret into another resource's sensitive
/// attribute, and a conflict at the sensitive path: neither the plan (text
/// or JSON), `query --json`, nor the policy messages on stderr print it.
#[test]
fn a_forwarded_secret_and_a_conflict_never_print() {
    let s = Scratch::new("secrets-forwarded");
    s.write(
        "p.df",
        &format!(
            "{PROGRAM}
resource leaky.vault copy {{
  if p = v.password
  backup = p
}}
"
        ),
    );
    let schema = schema();
    let args = ["--file", "p.df", "--provider", &schema, "--world", "w.json"];
    for cmd in [
        &["plan"][..],
        &["plan", "--json"],
        &["query", "arg", "--json"],
    ] {
        let r = s.run(&[&args[..], cmd].concat()).success();
        assert!(!r.stdout.contains(VAULT_SECRET), "{cmd:?}: {}", r.stdout);
        assert!(!r.stderr.contains(VAULT_SECRET), "{cmd:?}: {}", r.stderr);
    }
    let r = s.run(&[&args[..], &["plan"]].concat()).success();
    assert!(r.stdout.contains("backup = (sensitive)\n"), "{}", r.stdout);

    s.write(
        "p.df",
        &format!("{PROGRAM}\nv.password = \"VAULT-SECRET-TWO\"\n"),
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).failure();
    assert!(
        r.stderr.contains("conflicting attribute contributions"),
        "{}",
        r.stderr
    );
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("VAULT-SECRET"), "{out}");
    }
}

/// An input declared `secret(T)` and set with `--set`: `query`, `why` and
/// the plan file print its label, never the value. The plan file keeps a
/// keyed digest, so `apply PLAN` needs the value again and refuses another.
#[test]
fn a_secret_input_never_prints_in_query_why_or_the_plan_file() {
    let s = Scratch::new("secrets-input");
    s.write(
        "p.df",
        "edition 2026\ninput pw: secret(string)\noutput token: secret(string)\noutput(\"token\", p) if pw(p)\nresource leaky.vault v {\n  for pw(p)\n  password = p\n}\n",
    );
    let schema = schema();
    let args = ["--file", "p.df", "--provider", &schema, "--world", "w.json"];
    let set = ["--set", "pw=HUNTER-TWO-SECRET"];
    for cmd in [
        &["query", "input"][..],
        &["query", "pw(P)"],
        &["query", "attr", "--json"],
        &["why", "pw(P)"],
        &["plan", "--out", "plan.json"],
    ] {
        let r = s.run(&[&args[..], &set, cmd].concat()).success();
        for out in [&r.stdout, &r.stderr] {
            assert!(!out.contains("HUNTER-TWO"), "{cmd:?}: {out}");
        }
        if cmd[0] != "plan" {
            assert!(r.stdout.contains("input/#pw"), "{cmd:?}: {}", r.stdout);
        }
    }
    let file = s.read("plan.json");
    assert!(!file.contains("HUNTER-TWO"), "{file}");
    let f: serde_json::Value = serde_json::from_str(&file).unwrap();
    let entry = &f["inputs"]["set"][0];
    assert_eq!(entry["sensitive"], "input/#pw", "{file}");
    assert_eq!(entry["digest"].as_str().map(str::len), Some(64), "{file}");

    // apply PLAN cannot restore the value from the file.
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains("plan file plan.json: input pw is secret"),
        "{}",
        r.stderr
    );
    // Another value is a stale plan; the one planned applies.
    let r = s
        .run(&["apply", "plan.json", "--set", "pw=SOMETHING-ELSE"])
        .failure();
    assert!(r.stderr.contains("stale plan"), "{}", r.stderr);
    assert!(!r.stderr.contains("SOMETHING-ELSE"), "{}", r.stderr);
    s.run(&[&["apply", "plan.json"][..], &set].concat())
        .success();
}

/// `apply PLAN` compares a sensitive leaf by a keyed digest of its bytes:
/// the world's secret changed between plan and apply is refused, and
/// neither the file nor the refusal carries the bytes.
#[test]
fn a_sensitive_leaf_changed_between_plan_and_apply_is_refused() {
    let s = Scratch::new("secrets-plan-file");
    s.write(
        "w.json",
        r#"{"resources": {"leaky.vault::v": {"typ": "leaky.vault", "name": "v",
  "attrs": {"password": "OLD-VAULT-SECRET"}, "computed": {"id": "v-1"}}}}"#,
    );
    s.write("p.df", PROGRAM);
    let schema = schema();
    let args = ["--file", "p.df", "--provider", &schema, "--world", "w.json"];
    s.run(&[&args[..], &["plan", "--out", "plan.json"]].concat())
        .success();
    let file = s.read("plan.json");
    assert!(!file.contains("VAULT-SECRET"), "{file}");
    assert!(file.contains("\"digest\""), "{file}");
    // The key lives beside the stack's state, not in the file.
    assert_eq!(std::fs::read(s.path("w.state.key")).unwrap().len(), 32);

    s.write(
        "w.json",
        &s.read("w.json")
            .replace("OLD-VAULT-SECRET", "MUTATED-VAULT-SECRET"),
    );
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("leaky.vault.v password: the plan saw (sensitive, digest "),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("stale plan"), "{}", r.stderr);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("VAULT-SECRET"), "{out}");
    }
    assert!(s.read("w.json").contains("MUTATED-VAULT-SECRET"));
}

/// E DR-19: a secret reaches a public output only through
/// `declassify(V, Reason)`, which lowers its label and derives
/// `declassified(Site, Reason)` for policy to deny.
#[test]
fn a_secret_reaches_a_public_output_only_through_declassify() {
    let s = Scratch::new("secrets-declassify");
    let prog = |body: &str, policy: &str| {
        format!(
            "edition 2026\ninput pw: secret(string)\noutput pw_len: int\n\
             output(\"pw_len\", n) if pw(p), {body}\n{policy}"
        )
    };
    let args = [
        "--file",
        "p.df",
        "--world",
        "w.json",
        "--set",
        "pw=HUNTER-TWO",
    ];
    s.write("p.df", &prog("n = len(p)", ""));
    let r = s.run(&[&args[..], &["plan"]].concat()).failure();
    assert!(
        r.stderr
            .contains("p.df:4:1: E0304: a secret reaches output pw_len, not declared secret(T)"),
        "{}",
        r.stderr
    );

    s.write(
        "p.df",
        &prog("n = declassify(len(p), \"its length is public\")", ""),
    );
    let r = s
        .run(&[&args[..], &["query", "attr(\"output\", S, K, V)"]].concat())
        .success();
    assert!(r.stdout.contains("\"pw_len\"  10"), "{}", r.stdout);
    let r = s
        .run(&[&args[..], &["query", "declassified(At, R)"]].concat())
        .success();
    assert!(
        r.stdout.contains("\"p.df:4:1\"  \"its length is public\""),
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        &prog(
            "n = declassify(len(p), \"its length is public\")",
            "deny(m) if declassified(at, r), m = \"declassified at {at}: {r}\"\n",
        ),
    );
    let r = s.run(&[&args[..], &["plan"]].concat()).failure();
    assert!(
        r.stderr
            .contains("- declassified at p.df:4:1: its length is public\n"),
        "{}",
        r.stderr
    );
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("HUNTER"), "{out}");
    }
}
