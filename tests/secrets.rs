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
//! (`FakeCloud::load`'s doc comment: "The world as stored, secrets
//! included. Only the provider sees this."), the mock equivalent of the
//! real cloud's own database, not a `dform`-facing surface, so this is by
//! design, not a leak. `leaky.oops`'s secret, mislabeled public, leaks
//! through plan/apply output too — expected, it's what mislabeling means.
//!
//! Both secrets leak through `show` and `query`: the board already knows
//! this (ticket "show and query print secrets unredacted", Phase 3) — those
//! assertions are `#[ignore]`d with that ticket named in the reason, and
//! demonstrated (not ignored) that they currently fail, so the suite stays
//! green while the claim stays a claim until that ticket lands.

mod common;
use common::{Scratch, repo};

const VAULT_SECRET: &str = "VAULT-SECRET-DO-NOT-PRINT";
const OOPS_SECRET: &str = "OOPS-SECRET-DO-NOT-PRINT";

const PROGRAM: &str = r#"
resource leaky.vault v {
  password = "VAULT-SECRET-DO-NOT-PRINT"
}.

resource leaky.oops o {
  password = "OOPS-SECRET-DO-NOT-PRINT"
}.
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
    // Documented, not a leak: FakeCloud::load's own doc comment says the
    // world file holds secrets, because it stands in for the real cloud's
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

/// Known-broken: `show` prints the raw fact-derived attrs with no
/// redaction at all, so even the correctly labeled secret leaks.
#[test]
#[ignore = "ticket 'show and query print secrets unredacted': show does not redact yet"]
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
}

/// Known-broken: same as above, for `query`.
#[test]
#[ignore = "ticket 'show and query print secrets unredacted': query does not redact yet"]
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

/// Demonstrates the leak these two ignored tests above assert against,
/// so the report can name it precisely without relying on an ignored,
/// unrun test: `show` and `query` currently print the vault secret in
/// full, byte for byte.
#[test]
fn show_and_query_currently_leak_the_labeled_secret_documenting_the_bug() {
    let s = Scratch::new("secrets-known-bug");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let show = s
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
    assert!(
        show.stdout.contains(VAULT_SECRET),
        "show no longer leaks the labeled secret -- if this is now fixed, \
         un-ignore show_never_prints_the_labeled_secret and delete this test:\n{}",
        show.stdout
    );
    let query = s
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
    assert!(
        query.stdout.contains(VAULT_SECRET),
        "query no longer leaks the labeled secret -- if this is now fixed, \
         un-ignore query_never_prints_the_labeled_secret and delete this test:\n{}",
        query.stdout
    );
}
