//! The Postgres provider (`dform-provider-postgres`, R-159) against a fake
//! server (`dform_provider_postgres::fake`), named in dform.toml by
//! `path =`: the conformance suite, and a program whose role's password is
//! derived (`random.password`) planned, applied, planned clean, rotated
//! (`secrets rotate`: an update of the role, a new verifier) and planned
//! clean again; the provider's own role refused with its location; a
//! connection in the clear warned of.

mod common;
use common::{Run, Scratch};
use dform_provider_postgres::fake::{ADMIN, ADMIN_PASSWORD, Server};

fn postgres() -> String {
    common::exe("dform-provider-postgres")
}

/// `dform ARGS` in `s`, as alice, at `now`.
fn run(s: &Scratch, now: &str, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("RANDOM_MASTER", "provider-postgres-master")
        .env("DFORM_ACTOR", "alice")
        .env("DFORM_TEST_NOW", now);
    for k in [
        "PGHOST",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGSSLMODE",
        "PGDATABASE",
    ] {
        c.env_remove(k);
    }
    Run::from(c.output().unwrap())
}

const NOW: &str = "2026-10-07T09:00:00Z";

fn project(name: &str, server: &Server, body: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\npostgres = {{ path = \"{}\" }}\n",
            postgres()
        ),
    );
    s.write(
        "stacks/p.df",
        &format!(
            r#"input admin_pw: secret(string) = "{ADMIN_PASSWORD}"
use postgres {{
  host = "127.0.0.1"
  port = {}
  user = "{ADMIN}"
  password = admin_pw
  sslmode = "disable"
}}
{body}"#,
            server.port
        ),
    );
    s
}

/// The Synapse shape: a login role whose password is derived, and its
/// database, owned by it, with the locale Synapse wants.
const SYNAPSE: &str = r#"
resource postgres.role synapse {
  name = "synapse"
  login = true
  password = random.password("synapse-db")
}

resource postgres.database synapse {
  name = "synapse"
  owner = synapse
  encoding = "UTF8"
  lc_collate = "C"
  lc_ctype = "C"
}
"#;

#[test]
fn provider_check_conforms_against_the_fake_server() {
    let server = Server::start();
    let s = Scratch::new("postgres-check");
    let mut c = common::dform();
    c.args(["provider", "check", &postgres()])
        .current_dir(&s.dir);
    for (k, v) in server.env(ADMIN, ADMIN_PASSWORD, "disable") {
        c.env(k, v);
    }
    let r = Run::from(c.output().unwrap()).success();
    assert!(!r.stdout.contains("FAIL"), "{}", r.stdout);
    for check in [
        "ok    Schema serves its own types, with examples",
        "ok    Schema's types are named under the provider's name, postgres",
        "ok    Plan refuses a document without a required attribute",
        "ok    Plan marks a sensitive attribute sensitive",
        "ok    Apply CREATE returns the object with its computed values",
        "ok    Read returns what Apply created",
        "ok    Apply CREATE again with the same idempotency key answers the object it made",
        "ok    Apply UPDATE changes the object in place",
        "ok    Apply REPLACE makes a new object",
        "ok    Apply DELETE removes the object",
    ] {
        assert!(r.stdout.contains(check), "{check}\n{}", r.stdout);
    }
    let roles = server.world().roles;
    assert!(!roles.contains_key("dform_check"), "{roles:?}");
}

/// The password rotation, end to end: the role and its database applied,
/// a plan clean, `secrets rotate` changing the password alone as an
/// update of the role (a new verifier, the role logging in with the new
/// password, the plaintext in no statement), a plan clean after.
#[test]
fn a_rotation_is_an_update_of_the_role() {
    let server = Server::start();
    let s = project("postgres-rotate", &server, SYNAPSE);
    let plan = run(&s, NOW, &["plan", "p"]).success();
    for line in [
        "+ postgres.role synapse",
        "+ postgres.database synapse",
        "password = (sensitive",
        "owner = synapse",
    ] {
        assert!(plan.stdout.contains(line), "{line}\n{}", plan.stdout);
    }
    // In the clear, as written: said.
    assert!(
        plan.stderr.contains(
            "provider postgres: warning: sslmode=disable: the connection to dform_admin@127.0.0.1"
        ),
        "{}",
        plan.stderr
    );
    run(&s, NOW, &["apply", "p"]).success();
    let w = server.world();
    assert!(w.roles["synapse"].login);
    let first = w.roles["synapse"].verifier.clone().unwrap();
    assert!(first.starts_with("SCRAM-SHA-256$4096:"), "{first}");
    let db = &w.databases["synapse"];
    assert_eq!((db.owner.as_str(), db.collate.as_str()), ("synapse", "C"));
    // State keeps the password's keyed digest, never it.
    let st = s.json("dform.state/p/state.json");
    let written = &st["resources"]["postgres.role::synapse"]["written"]["password"];
    assert!(
        written
            .as_str()
            .is_some_and(|d| d.starts_with("hmac-sha256:")),
        "{st}"
    );

    let again = run(&s, NOW, &["plan", "p"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);

    run(
        &s,
        "2026-10-08T09:00:00Z",
        &["secrets", "rotate", "p", "synapse-db"],
    )
    .success();
    let r = run(&s, NOW, &["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("~ postgres.role synapse"), "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "password = (sensitive) → (sensitive, generation 2, rotated 2026-10-08 by alice)"
        ),
        "{}",
        r.stdout
    );
    let before = server.queries().len();
    run(&s, NOW, &["apply", "p"]).success();
    let second = server.world().roles["synapse"].verifier.clone().unwrap();
    assert_ne!(first, second);
    let altered: Vec<String> = server.queries()[before..]
        .iter()
        .filter(|q| !q.starts_with("SELECT"))
        .cloned()
        .collect();
    assert_eq!(altered.len(), 1, "{altered:?}");
    assert!(
        altered[0].starts_with("ALTER ROLE \"synapse\" PASSWORD 'SCRAM-SHA-256$4096:"),
        "{altered:?}"
    );
    let after = run(&s, NOW, &["plan", "p"]).success();
    assert!(after.stdout.contains("is up to date"), "{}", after.stdout);
    // The database was never touched: same owner, no DROP.
    assert!(!server.queries().iter().any(|q| q.contains("DROP")));
}

/// A program that manages the role the provider connects as is refused at
/// plan, the error naming the resource and a separate admin role.
#[test]
fn the_admin_role_is_refused_with_its_location() {
    let server = Server::start();
    let s = project(
        "postgres-admin",
        &server,
        &format!(
            "\nresource postgres.role admin {{\n  name = \"{ADMIN}\"\n  password = random.password(\"admin\")\n}}\n"
        ),
    );
    let r = run(&s, NOW, &["plan", "p"]);
    assert!(!r.ok, "{}", r.stdout);
    let all = format!("{}{}", r.stdout, r.stderr);
    assert!(all.contains("postgres.role"), "{all}");
    assert!(
        all.contains("\"dform_admin\" is the role provider postgres connects as"),
        "{all}"
    );
    assert!(all.contains("separate admin role"), "{all}");
    assert!(server.accepts(ADMIN, ADMIN_PASSWORD));
}
