//! Tests the "never prints" claim (README, "Computed values come from
//! Apply": *"A `sensitive` computed value never leaves the provider... A
//! value at a `sensitive` path the program sets prints as `(sensitive)`."*).
//!
//! tests/fixtures/providers/leaky/schema.df has two near-identical types: `leaky.vault`
//! declares `password` `[sensitive]`, `leaky.oops` mislabels the same kind
//! of value as public (no flags). A program sets a literal, distinctive
//! password string on one resource of each type and this test drives it
//! through every surface dform prints or persists to, checking whether the
//! literal bytes ever appear.
//!
//! Expected shape of the result: `leaky.vault`'s secret never appears in
//! anything dform prints (plan, state.json) — that's the claim. It DOES
//! appear in world.json: that file is the fake provider's own storage
//! (crates/dform-mock/src/lib.rs: "A sensitive computed value stays in the world"),
//! kept by the mock provider's own process, the mock equivalent of the
//! real cloud's own database, not a `dform`-facing surface, so this is by
//! design, not a leak. `leaky.oops`'s secret, mislabeled public, leaks
//! through plan/apply output too — expected, it's what mislabeling means.
//!
//! `show`, `query`, `plan --json`, `query --json` and the policy
//! messages on stderr print through one redactor (crates/dform-core/src/query.rs) and never
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
        .join("tests/fixtures/providers/leaky/schema.df")
        .to_str()
        .unwrap()
        .to_string()
}

#[test]
fn plan_and_apply_redact_the_labeled_secret_but_not_the_mislabeled_one() {
    let s = Scratch::new("secrets-plan");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let mock = ["--provider", schema.as_str(), "--world", "w.json"];

    let r = s.run(&common::on("p.df", &mock, &["plan"])).success();
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

    let r = s.run(&common::on("p.df", &mock, &["apply"])).success();
    assert!(!r.stdout.contains(VAULT_SECRET), "{}", r.stdout);

    // Drift and re-plan: the update-side diff must redact both sides too.
    s.write(
        "p.df",
        &PROGRAM
            .replace(VAULT_SECRET, "VAULT-SECRET-CHANGED")
            .replace(OOPS_SECRET, "OOPS-SECRET-CHANGED"),
    );
    let r = s.run(&common::on("p.df", &mock, &["plan"])).success();
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
        "dev",
        "--provider",
        &schema,
        "--world",
        "w.json",
        "apply",
        "p.df",
    ])
    .success();
    let state = s.read("w.state.json");
    assert!(!state.contains(VAULT_SECRET), "{state}");
    assert!(!state.contains(OOPS_SECRET), "{state}");
}

#[test]
fn world_file_is_the_providers_own_storage_and_holds_both() {
    // Documented, not a leak: crates/dform-mock/src/lib.rs says the world file holds
    // secrets, because it stands in for the real cloud's
    // storage. This test pins that on purpose, as the boundary of the
    // never-prints claim: it is about what dform itself prints and
    // persists as the *consumer*, not the mock backend's storage.
    let s = Scratch::new("secrets-world");
    s.write("p.df", PROGRAM);
    let schema = schema();
    s.run(&[
        "dev",
        "--provider",
        &schema,
        "--world",
        "w.json",
        "apply",
        "p.df",
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
            "dev",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "show",
            r#"leaky.vault["v"]"#,
            "p.df",
        ])
        .success();
    assert!(!r.stdout.contains(VAULT_SECRET), "{}", r.stdout);
    assert!(
        r.stdout
            .contains(r#""password": {"sensitive": "leaky.vault[\"v\"].password"}"#)
            || r.stdout
                .contains(r#""sensitive": "leaky.vault[\"v\"].password""#),
        "{}",
        r.stdout
    );
}

/// `query` prints facts through the redacting printer (crates/dform-core/src/query.rs).
#[test]
fn query_never_prints_the_labeled_secret() {
    let s = Scratch::new("secrets-query");
    s.write("p.df", PROGRAM);
    let schema = schema();
    let r = s
        .run(&[
            "dev",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "query",
            "arg",
            "p.df",
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
            "dev",
            "--provider",
            &schema,
            "--world",
            "w.json",
            "plan",
            "p.df",
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
    let mock = ["--provider", schema.as_str(), "--world", "w.json"];
    for cmd in [
        &["plan"][..],
        &["plan", "--json"],
        &["query", "arg", "--json"],
    ] {
        let r = s.run(&common::on("p.df", &mock, cmd)).success();
        assert!(!r.stdout.contains(VAULT_SECRET), "{cmd:?}: {}", r.stdout);
        assert!(!r.stderr.contains(VAULT_SECRET), "{cmd:?}: {}", r.stderr);
    }
    let r = s.run(&common::on("p.df", &mock, &["plan"])).success();
    assert!(r.stdout.contains("backup = (sensitive)\n"), "{}", r.stdout);

    s.write(
        "p.df",
        &format!("{PROGRAM}\nok(1)\nset v.password = \"VAULT-SECRET-TWO\" where ok(1)\n"),
    );
    let r = s.run(&common::on("p.df", &mock, &["plan"])).failure();
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
        "edition 2026\ninput pw: secret(string)\noutput token: secret(string) = p where pw(p)\nresource leaky.vault v {\n  password = p\n} where pw(p)\n",
    );
    let schema = schema();
    let mock = ["--provider", schema.as_str(), "--world", "w.json"];
    let set = ["--set", "pw=HUNTER-TWO-SECRET"];
    for cmd in [
        &["query", "input"][..],
        &["query", "pw(P)"],
        &["query", "attr", "--json"],
        &["why", "pw(P)"],
        &["plan", "--out", "plan.json"],
    ] {
        let r = s
            .run(&common::on("p.df", &mock, &[&set, cmd].concat()))
            .success();
        for out in [&r.stdout, &r.stderr] {
            assert!(!out.contains("HUNTER-TWO"), "{cmd:?}: {out}");
        }
        if cmd[0] != "plan" {
            assert!(r.stdout.contains("input.pw"), "{cmd:?}: {}", r.stdout);
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
    s.write_owned_world(
        "w.json",
        r#"{"resources": {"leaky.vault::v": {"typ": "leaky.vault", "name": "v",
  "attrs": {"password": "OLD-VAULT-SECRET"}, "computed": {"id": "v-1"}}}}"#,
    );
    s.write("p.df", PROGRAM);
    let schema = schema();
    let mock = ["--provider", schema.as_str(), "--world", "w.json"];
    s.run(&common::on("p.df", &mock, &["plan", "--out", "plan.json"]))
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
            .contains("leaky.vault[\"v\"].password: the plan saw (sensitive, digest "),
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
            "edition 2026\ninput pw: secret(string)\noutput pw_len: int = n where pw(p), {body}\n{policy}"
        )
    };
    let mock = ["--world", "w.json", "--set", "pw=HUNTER-TWO"];
    s.write("p.df", &prog("n = len(p)", ""));
    let r = s.run(&common::on("p.df", &mock, &["plan"])).failure();
    assert!(
        r.stderr
            .contains("p.df:3:1: E0304: a secret reaches output pw_len, not declared secret(T)"),
        "{}",
        r.stderr
    );

    s.write(
        "p.df",
        &prog("n = declassify(len(p), \"its length is public\")", ""),
    );
    let r = s
        .run(&common::on(
            "p.df",
            &mock,
            &["query", "attr(\"output\", S, K, V)"],
        ))
        .success();
    assert!(r.stdout.contains("\"pw_len\"  10"), "{}", r.stdout);
    let r = s
        .run(&common::on(
            "p.df",
            &mock,
            &["query", "declassified(At, R)"],
        ))
        .success();
    assert!(
        r.stdout.contains("\"p.df:3:1\"  \"its length is public\""),
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        &prog(
            "n = declassify(len(p), \"its length is public\")",
            "deny \"declassified at ${at}: ${r}\" where declassified(at, r)\n",
        ),
    );
    let r = s.run(&common::on("p.df", &mock, &["plan"])).failure();
    assert!(
        r.stderr
            .contains("- declassified at p.df:3:1: its length is public\n"),
        "{}",
        r.stderr
    );
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("HUNTER"), "{out}");
    }
}

/// A stack whose secret outputs are an input's value (`token`) and a
/// resource's sensitive attribute (`pass`, a ref to what the provider holds).
const PRODUCER: &str = r#"edition 2026
input pw: secret(string)
stack prod {}
resource leaky.vault v {
  password = p
} where pw(p)
output token: secret(string) = pw
output pass: secret(string) = v.password
"#;

const PRODUCED: &str = "PRODUCED-SECRET-DO-NOT-STORE";

/// Every file under `dir` but the mock's worlds (the provider's own
/// storage, `remote.json`), with its path.
fn stored_files(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            stored_files(&p, out);
        } else if p.file_name().is_some_and(|n| n != "remote.json") {
            let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
            out.push((p.display().to_string(), text));
        }
    }
}

/// E DR-19: a secret output's bytes never enter the store. The state
/// records it by its label and the keyed digest of its value, and the
/// published outputs likewise, with where a provider holds it; on a
/// directory and in a bucket alike.
#[test]
fn a_secret_output_is_stored_by_label_and_digest_never_by_value() {
    let schema = schema();
    let set = format!("pw={PRODUCED}");
    let apply = ["dev", "--provider", &schema, "apply", "prod", "--set", &set];

    // A directory.
    let s = Scratch::project("secrets-output-local");
    s.write("stacks/prod.df", PRODUCER);
    s.run(&apply).success();
    let mut files = Vec::new();
    stored_files(&s.path("dform.state"), &mut files);
    for (path, text) in &files {
        assert!(!text.contains(PRODUCED), "{path}:\n{text}");
    }
    let state = s.read("dform.state/prod/state.json");
    assert!(state.contains("\"label\": \"output/#token\""), "{state}");
    assert!(state.contains("\"digest\": \"hmac-sha256:"), "{state}");
    let published = s.read("dform.state/prod/outputs.json");
    assert!(
        published.contains("\"label\": \"output/#pass\""),
        "{published}"
    );
    assert!(published.contains("\"path\": \"password\""), "{published}");

    // A bucket (the fake S3 server).
    let server = dform_s3::fake::Server::start();
    let s = Scratch::project("secrets-output-s3");
    s.write("stacks/prod.df", PRODUCER);
    s.write(
        "dform.toml",
        &format!(
            "[defaults]\nbackend = 's3(\"dform-test\", \"secrets/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n",
            server.endpoint
        ),
    );
    let spec = dform_core::store::S3Spec {
        bucket: "dform-test".into(),
        prefix: "secrets".into(),
        endpoint: Some(server.endpoint.clone()),
        region: Some("us-east-1".into()),
    };
    let bucket =
        dform_s3::S3Store::with_credentials(&spec, "", rusty_s3::Credentials::new("fake", "fake"))
            .unwrap();
    bucket.create_bucket().unwrap();
    let r = common::dform()
        .args(common::yes(&apply))
        .current_dir(&s.dir)
        .env("DFORM_S3_ACCESS_KEY_ID", "fake")
        .env("DFORM_S3_SECRET_ACCESS_KEY", "fake")
        .output()
        .unwrap();
    common::Run::from(r).success();
    use dform_core::store::Store;
    let keys = bucket.list("").unwrap();
    assert!(keys.iter().any(|k| k == "prod/state.json"), "{keys:?}");
    assert!(keys.iter().any(|k| k == "prod/outputs.json"), "{keys:?}");
    for k in &keys {
        let o = bucket.get(k).unwrap().expect("listed");
        let text = String::from_utf8_lossy(&o.bytes);
        assert!(!text.contains(PRODUCED), "{k}:\n{text}");
        if k == "prod/state.json" {
            assert!(text.contains("\"label\": \"output/#token\""), "{text}");
        }
    }
    let mut files = Vec::new();
    stored_files(&s.path("dform.state"), &mut files);
    for (path, text) in &files {
        assert!(!text.contains(PRODUCED), "{path}:\n{text}");
    }
}

/// A secret output crosses stacks as a reference (E DR-19): the reader's
/// provider reads it where the producer's provider holds it, inside Apply.
/// The reader's field applies, a changed secret updates it, and neither
/// run prints it. A secret the producer holds nowhere (an input's value)
/// cannot cross, and the plan says so.
#[test]
fn a_secret_output_reaches_a_sensitive_field_in_another_stack() {
    let schema = schema();
    let s = Scratch::project("secrets-output-cross");
    s.write("stacks/prod.df", PRODUCER);
    s.write(
        "stacks/app.df",
        "edition 2026\n\
         stack app {}\n\
         resource leaky.vault copy {\n\
           backup = p\n\
         } where stack_output(\"prod\", \"pass\", p)\n\
         ",
    );
    let dev = |args: &[&str]| {
        let mut a = vec!["dev", "--provider", schema.as_str()];
        a.extend_from_slice(args);
        s.run(&a)
    };
    let set = format!("pw={PRODUCED}");
    dev(&["apply", "prod", "--set", &set]).success();
    let materialized = |s: &Scratch| -> serde_json::Value {
        let w: serde_json::Value =
            serde_json::from_str(&s.read("dform.state/app/remote.json")).unwrap();
        w["resources"]["leaky.vault::copy"]["materialized"]["backup"].clone()
    };

    let r = dev(&["apply", "app"]).success();
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains(PRODUCED), "{out}");
    }
    assert!(
        r.stdout.contains(
            "+ leaky.vault[\"copy\"]\n  backup = (sensitive stack_output[\"prod\"].pass)\n"
        ),
        "{}",
        r.stdout
    );
    assert_eq!(materialized(&s), PRODUCED);
    let r = dev(&["plan", "app"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);

    // The producer's secret changes: the reader's field is updated.
    dev(&["apply", "prod", "--set", "pw=ROTATED-SECRET"]).success();
    let r = dev(&["apply", "app"]).success();
    assert!(r.stdout.contains("~ leaky.vault[\"copy\"]"), "{}", r.stdout);
    assert!(!r.stdout.contains("ROTATED"), "{}", r.stdout);
    assert_eq!(materialized(&s), "ROTATED-SECRET");

    // An input's value is held nowhere: no provider can read it.
    s.write(
        "stacks/app.df",
        &s.read("stacks/app.df").replace("\"pass\"", "\"token\""),
    );
    let r = dev(&["plan", "app"]).failure();
    assert!(
        r.stderr.contains(
            "plan leaky.vault[\"copy\"].backup: stack_output[\"prod\"].token is a secret output of prod \
             that no provider holds"
        ),
        "{}",
        r.stderr
    );
}

/// examples/crud-api's generated password (`extern random.password(+key,
/// -value: secret(string)) persist`): the random provider holds it, and
/// nothing dform keeps carries it (E DR-19): not state (its persisted
/// answer is the label, digest and where it is held), the in-flight record,
/// the audit log, the controller's memo, nor a plan file. The Secret and
/// the database user still get it: the mock reads it where it is held,
/// inside Apply.
#[test]
fn a_persisted_extern_secret_is_held_by_its_provider_never_stored() {
    const PASSWORD: &str = "mock-password-7f3c9a";
    let s = Scratch::new("secrets-extern");
    common::copy_dir(&repo().join("examples/crud-api"), &s.dir);
    let _ = std::fs::remove_dir_all(s.path("dform.state"));
    // A plan file taken before anything exists, applied; an apply that
    // fails midway keeps its in-flight record; the next one resumes it.
    s.run(&["plan", "--out", "first.json"]).success();
    let first = s.read("first.json");
    assert!(!first.contains(PASSWORD), "{first}");
    s.run(&[
        "dev",
        "apply",
        "first.json",
        "--chaos",
        "fail=k8s.job[\"migrate-v42\"]",
    ])
    .failure();
    let state = s.read("dform.state/shop.crud_api/state.json");
    assert!(state.contains("\"in_flight\""), "{state}");
    assert!(!state.contains(PASSWORD), "{state}");
    s.run(&["apply"]).success();
    let r = s.run(&["plan", "--out", "plan.json"]).success();
    assert_eq!(
        r.summary(),
        "stack shop.crud_api is undeformed",
        "{}",
        r.stdout
    );
    s.run(&["controller", "run", "shop.crud_api", "--once"])
        .success();
    let mut files = Vec::new();
    stored_files(&s.path("dform.state"), &mut files);
    files.push(("plan.json".into(), s.read("plan.json")));
    assert!(
        files.iter().any(|(p, _)| p.ends_with("controller.json")),
        "{:?}",
        files.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    for (path, text) in &files {
        assert!(!text.contains(PASSWORD), "{path}:\n{text}");
    }
    let state: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/shop.crud_api/state.json")).unwrap();
    let answer = &state["externs"][0];
    assert_eq!(
        answer["rows"][0][1]["v"]["label"], "random.password/crud-api-db#2",
        "{answer}"
    );
    let held = &answer["held"]["random.password/crud-api-db#2"];
    assert_eq!(held["type"], "random.password", "{answer}");
    assert!(
        held["digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("hmac-sha256:")),
        "{answer}"
    );

    // The provider's world: it keeps the value, and the objects that read
    // it have it.
    let world: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/shop.crud_api/remote.json")).unwrap();
    let r = &world["resources"];
    assert_eq!(
        r["k8s.secret::db_conn"]["materialized"]["stringData.PGPASSWORD"], PASSWORD,
        "{world}"
    );
    assert_eq!(
        r["google_sql_user::crud_user"]["materialized"]["password"], PASSWORD,
        "{world}"
    );
}

/// A world document dform keeps beyond a run, the in-flight record of an
/// interrupted apply and the controller's baseline, holds a sensitive leaf
/// as its keyed digest, never the value (here a secret input's, in the
/// world as the program set it). The resume and the controller compare
/// what they kept with the world the same way.
#[test]
fn kept_world_documents_hold_a_sensitive_leaf_by_its_digest() {
    let schema = schema();
    let s = Scratch::project("secrets-kept");
    s.write(
        "stacks/s.df",
        "edition 2026\n\
         input pw: secret(string)\n\
         stack s {}\n\
         resource leaky.vault v {\n\
           password = p\n\
         } where pw(p)\n\
         ",
    );
    let dev = |args: &[&str]| {
        let mut a = vec!["dev", "--provider", schema.as_str()];
        a.extend_from_slice(args);
        s.run(&a)
    };
    dev(&["apply", "s", "--set", "pw=FIRST-KEPT-SECRET"]).success();
    dev(&[
        "apply",
        "s",
        "--set",
        "pw=SECOND-KEPT-SECRET",
        "--chaos",
        "fail=leaky.vault[\"v\"]",
    ])
    .failure();
    let state = s.read("dform.state/s/state.json");
    assert!(state.contains("\"in_flight\""), "{state}");
    assert!(
        state.contains("\"password\": \"(sensitive hmac-sha256:"),
        "{state}"
    );
    assert!(!state.contains("KEPT-SECRET"), "{state}");
    let r = dev(&["apply", "s", "--set", "pw=SECOND-KEPT-SECRET"]).success();
    assert!(
        r.stdout
            .contains("resuming the apply interrupted at tick 1"),
        "{}",
        r.stdout
    );
    dev(&[
        "controller",
        "run",
        "s",
        "--once",
        "--set",
        "pw=SECOND-KEPT-SECRET",
    ])
    .success();
    let memo = s.read("dform.state/s/controller.json");
    assert!(
        memo.contains("\"password\": \"(sensitive hmac-sha256:"),
        "{memo}"
    );
    assert!(!memo.contains("KEPT-SECRET"), "{memo}");
}
