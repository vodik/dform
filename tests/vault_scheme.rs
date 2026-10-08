//! A secret manager as a location (R-172): the Vault provider
//! (`dform-provider-vault`) declares `vault://` and reads a KV v2 secret
//! with its version through the host, against a fake Vault
//! (`dform_provider_vault::fake`). A secret read into a `secret(T)` cell
//! configures a provider (Postgres, against its fake server), is recorded
//! in the plan file by its keyed digest and its version, and `apply PLAN`
//! refuses once the secret moved in Vault; `secrets list` says the secret
//! is `managed`, at its version. The token is a credential by name the
//! host applies; AppRole logs in.

mod common;
use common::{Run, Scratch};
use dform_provider_vault::fake::{ROLE_ID, SECRET_ID, Server, TOKEN};
use serde_json::json;

fn vault() -> String {
    common::exe("dform-provider-vault")
}

/// `dform ARGS` in `s`, its credentials directory the scratch's own.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("DFORM_CREDENTIALS", s.path("credentials"))
        .env("RANDOM_MASTER", "vault-scheme-master")
        .env("DFORM_TEST_NOW", "2026-10-07T09:00:00Z")
        .env_remove("VAULT_ADDR")
        .env_remove("VAULT_TOKEN");
    for k in ["PGHOST", "PGPORT", "PGUSER", "PGPASSWORD", "PGSSLMODE"] {
        c.env_remove(k);
    }
    Run::from(c.output().unwrap())
}

/// A project whose dform.toml names the provider (and `extra` providers)
/// and grants it the token, the token in the operator's credential file,
/// and `use vault`'s block pointing at `server` before `body`.
fn project(name: &str, server: &Server, extra: &str, body: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\n{extra}\n\
             [providers.vault]\npath = \"{}\"\ncredentials = [\"bearer:vault\"]\n",
            vault()
        ),
    );
    s.write("credentials/bearer/vault", TOKEN);
    s.write(
        "p.df",
        &format!("use vault {{ address = \"{}\" }}\n{body}", server.address()),
    );
    s
}

const SIGNING: &str = r#"use fake
let signing: secret(string) = io.read("vault://kv/synapse/signing#key")
resource net.vpc v { cidr_block = "10.0.0.0/16" }
output s: secret(string) = signing
"#;

/// The read, its record and its refusal: the plan file holds the
/// secret's keyed digest and the version Vault answered, never the
/// secret; a new version written in Vault after the plan makes `apply
/// PLAN` refuse, naming both versions; a plan made after applies.
#[test]
fn apply_refuses_a_plan_whose_secret_moved_in_vault() {
    let server = Server::start();
    server.put("kv", "synapse/signing", json!({"key": "ed25519 a_first"}));
    let s = project("vault-moved", &server, "", SIGNING);
    run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    let plan = s.read("plan.json");
    assert!(!plan.contains("a_first"), "{plan}");
    let answers = &s.json("plan.json")["inputs"]["answers"];
    assert_eq!(answers[0]["version"], json!("1"), "{plan}");
    assert!(answers[0]["digest"].is_string(), "{plan}");
    // The token went as the host applied it; the provider never had it.
    let seen = server.seen();
    assert!(
        seen.iter()
            .any(|r| r.path == "/v1/kv/data/synapse/signing" && r.token == TOKEN),
        "{seen:?}"
    );

    assert_eq!(
        server.put("kv", "synapse/signing", json!({"key": "ed25519 a_second"})),
        2
    );
    let r = run(&s, &["apply", "plan.json"]).failure();
    assert!(
        r.stderr.contains(
            "io.read(\"vault://kv/synapse/signing#key\"): version 1 in the plan, 2 now: it \
             moved in its secret manager since the plan"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("@document"), "{}", r.stderr);

    run(&s, &["plan", "--out", "plan2.json", "p.df"]).success();
    assert_eq!(
        s.json("plan2.json")["inputs"]["answers"][0]["version"],
        json!("2")
    );
    run(&s, &["apply", "plan2.json"]).success();
}

/// `secrets list` says a secret read from Vault is `managed`, at the
/// version read, and `secrets rotate` sends the operator to Vault.
#[test]
fn a_managed_secret_is_listed_with_its_version() {
    let server = Server::start();
    server.put("kv", "synapse/signing", json!({"key": "one"}));
    server.put("kv", "synapse/signing", json!({"key": "two"}));
    let s = project("vault-list", &server, "", SIGNING);
    run(&s, &["apply", "p.df"]).success();
    let r = run(&s, &["secrets", "list", "p.df"]).success();
    let row = r
        .stdout
        .lines()
        .find(|l| l.starts_with("vault://kv/synapse/signing#key"))
        .unwrap_or_else(|| panic!("{}", r.stdout));
    let cols: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(&cols[1..3], ["managed", "2"], "{}", r.stdout);
    assert!(!r.stdout.contains("two"), "{}", r.stdout);
    let j = run(&s, &["secrets", "list", "--json", "p.df"]).success();
    assert!(j.stdout.contains("\"generation\": \"2\""), "{}", j.stdout);
    let r = run(
        &s,
        &[
            "secrets",
            "rotate",
            "p.df",
            "vault://kv/synapse/signing#key",
        ],
    )
    .failure();
    assert!(
        r.stderr.contains(
            "is managed in p, and lives in vault://kv/synapse/signing#key, its secret \
             manager: rotate it there"
        ),
        "{}",
        r.stderr
    );
}

/// The Postgres provider configured from a password Vault keeps: the
/// read reaches its Configure (the fake server lets it in with that
/// password alone), and nothing of it reaches the plan file.
#[test]
fn a_provider_is_configured_from_a_vault_secret() {
    use dform_provider_postgres::fake::{ADMIN, ADMIN_PASSWORD};
    let pg = dform_provider_postgres::fake::Server::start();
    let server = Server::start();
    server.put(
        "kv",
        "db/admin",
        json!({"password": ADMIN_PASSWORD, "user": ADMIN}),
    );
    let s = project(
        "vault-postgres",
        &server,
        &format!(
            "postgres = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-postgres")
        ),
        &format!(
            r#"let admin_pw: secret(string) = io.read("vault://kv/db/admin#password")
use postgres {{
  host = "127.0.0.1"
  port = {}
  user = "{ADMIN}"
  password = admin_pw
  sslmode = "disable"
}}
resource postgres.role app {{
  name = "app"
  login = true
}}
"#,
            pg.port
        ),
    );
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    assert!(r.stdout.contains("+ postgres.role app"), "{}", r.stdout);
    assert!(!s.read("plan.json").contains(ADMIN_PASSWORD));
    run(&s, &["apply", "plan.json"]).success();
    assert!(pg.world().roles.contains_key("app"));
}

/// AppRole: the provider logs in with the role id and the secret id the
/// program gives (the secret id a secret), and reads with the token Vault
/// answers; each run logs in again.
#[test]
fn approle_logs_in() {
    let server = Server::start();
    server.put("kv", "app/config", json!({"token": "t1", "n": 3}));
    let s = Scratch::project("vault-approle");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\n\n\
             [providers.vault]\npath = \"{}\"\n",
            vault()
        ),
    );
    s.write(
        "p.df",
        &format!(
            r#"input secret_id: secret(string) = "{SECRET_ID}"
use vault {{
  address = "{}"
  approle = {{ role_id: "{ROLE_ID}", secret_id: secret_id }}
}}
use fake
let config: secret(string) = io.read("vault://kv/app/config")
resource net.vpc v {{ cidr_block = "10.0.0.0/16" }}
output c: secret(string) = config
"#,
            server.address()
        ),
    );
    run(&s, &["plan", "p.df"]).success();
    assert_eq!(server.logins(), 1);
    let seen = server.seen();
    let read = seen
        .iter()
        .find(|r| r.path == "/v1/kv/data/app/config")
        .unwrap_or_else(|| panic!("{seen:?}"));
    assert_eq!(read.token, "hvs.approle-1");
    server.expire_tokens();
    run(&s, &["plan", "p.df"]).success();
    assert_eq!(server.logins(), 2);
}

/// What Vault says no to is said where it went wrong: a key the secret
/// does not have names the keys it has; a secret not written yet is waited
/// on; a version deleted is an error naming how it comes back; a token
/// Vault refuses names the policy to grant.
#[test]
fn vault_refusals_are_said_at_the_read() {
    let server = Server::start();
    server.put("kv", "app/a", json!({"user": "u", "password": "p"}));
    let s = project(
        "vault-refusals",
        &server,
        "",
        "use fake\nlet x: secret(string) = io.read(\"vault://kv/app/a#pasword\")\n\
         resource net.vpc v { cidr_block = \"10.0.0.0/16\" }\noutput o: secret(string) = x\n",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("the secret kv/app/a has no key \"pasword\": its keys are password, user"),
        "{}",
        r.stderr
    );

    // A sensitive attribute to wait in: the fake's schema and `db.secret`.
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df"))
            .unwrap()
            + "type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n"),
    );
    s.write(
        "dform.toml",
        &s.read("dform.toml")
            .replace("fake = \"fake\"", "fake = \"providers/fake\""),
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("vault://kv/app/a#pasword", "vault://kv/app/later#key")
            .replace(
                "output o: secret(string) = x",
                "resource db.secret w { password = x }",
            ),
    );
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("waits on  vault://kv/app/later#key"),
        "{}",
        r.stdout
    );

    server.delete("kv", "app/a", 1);
    s.write(
        "p.df",
        &s.read("p.df").replace(
            "vault://kv/app/later#key",
            "vault://kv/app/a?version=1#user",
        ),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("version 1 was deleted in Vault (`vault kv undelete` restores it)"),
        "{}",
        r.stderr
    );

    s.write("credentials/bearer/vault", "hvs.not-a-token");
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "Vault refused the read of /v1/kv/data/app/a?version=1 (403): the token's policy \
             needs `read` on kv/data/app/a"
        ),
        "{}",
        r.stderr
    );
}
