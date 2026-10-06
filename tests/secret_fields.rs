//! A field an object type declares `secret(T)` is a declared place for a
//! secret (R-118): `output conn: conn = { .., password: random.password(..)
//! }` with `type conn = { .., password: secret(string) }` passes the
//! secrets pass, `conn.password` reads as a secret and `conn.host` as a
//! public value. A secret reaching a field whose type is not `secret(T)` is
//! still E0304, naming the field and its type. The same for an input of an
//! object type, a component's output and another stack's.

mod common;
use common::{Scratch, repo};

fn schema() -> String {
    repo()
        .join("tests/fixtures/providers/leaky/schema.df")
        .to_str()
        .unwrap()
        .to_string()
}

/// The shape of a real project: a component's output of a named object
/// type with one secret field, its copy made in the stack, and two
/// readers: a component given the password and the host, and the stack's
/// own resource given the host where the schema is public.
const DATABASES: &str = r#"
component postgres {
  input name: string
  type conn = { host: string, port: int, database: string, user: string, password: secret(string) }
  output conn: conn = {
    host: "${name}.databases.svc",
    port: 5432,
    database: name,
    user: name,
    password: random.password("${name}-password"),
  }
}
"#;

const SYNAPSE: &str = r#"
component synapse {
  input db_host: string
  input db_password: secret(string)
  resource leaky.vault secret { password = db_password }
  resource leaky.oops config { password = db_host }
}
"#;

const APPS: &str = r#"
use fake
resource databases.postgres synapse_db { name = "synapse" }
resource synapse.synapse matrix {
  db_host = synapse_db.conn.host
  db_password = synapse_db.conn.password
}
resource leaky.vault direct { password = synapse_db.conn.password }
resource leaky.oops plain { password = "${synapse_db.conn.host}:${synapse_db.conn.port}" }
let db = synapse_db.conn
resource leaky.vault rendered { password = yaml.encode({ host: db.host, password: db.password }) }
resource leaky.oops via_let { password = db.user }
"#;

fn project(name: &str, apps: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("databases.df", DATABASES);
    s.write("synapse.df", SYNAPSE);
    s.write("stacks/apps.df", apps);
    s
}

fn plan(s: &Scratch) -> common::Run {
    s.run(&["dev", "--provider", &schema(), "plan", "apps"])
}

/// The project's shape plans: the password goes where it is sensitive
/// (directly, through a component's input, through a `let` of the whole
/// object and an encoder), the host and user where they are public.
#[test]
fn a_secret_field_of_a_named_object_type_is_declared() {
    let s = project("secret-fields-named", APPS);
    let r = plan(&s).success();
    assert!(
        r.stdout
            .contains("password = \"synapse.databases.svc:5432\""),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("password = \"synapse.databases.svc\""),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("password = \"synapse\""), "{}", r.stdout);
    let secret = r.stdout.matches("password = (sensitive)").count();
    assert_eq!(secret, 3, "{}", r.stdout);
}

/// The same with the object type written inline.
#[test]
fn a_secret_field_of_an_inline_object_type_is_declared() {
    let s = project("secret-fields-inline", APPS);
    s.write(
        "databases.df",
        &DATABASES
            .replace("  type conn = { host: string, port: int, database: string, user: string, password: secret(string) }\n", "")
            .replace(
                "output conn: conn =",
                "output conn: { host: string, port: int, database: string, user: string, password: secret(string) } =",
            ),
    );
    let r = plan(&s).success();
    assert_eq!(
        r.stdout.matches("password = (sensitive)").count(),
        3,
        "{}",
        r.stdout
    );
}

/// A secret in a field the type does not declare secret is E0304, naming
/// the field and its type; and the secret field read into a public place
/// is E0304 at the reader.
#[test]
fn a_secret_in_a_public_field_is_still_refused() {
    let s = project("secret-fields-public", APPS);
    s.write(
        "databases.df",
        &DATABASES.replace("user: name,", "user: random.password(\"${name}-user\"),"),
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr.contains(
            "E0304: a secret reaches output conn.user, not declared secret(T): its type is string"
        ),
        "{}",
        r.stderr
    );

    let s = project(
        "secret-fields-reader",
        &format!("{APPS}resource leaky.oops leak {{ password = synapse_db.conn.password }}\n"),
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr
            .contains("E0304: a secret reaches leaky.oops .password, not marked sensitive"),
        "{}",
        r.stderr
    );

    // The whole object is secret where its secret field is: an output
    // that copies it must declare that field.
    let s = project(
        "secret-fields-copy",
        &format!("{APPS}output conn: {{ host: string, password: string }} = synapse_db.conn\n"),
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr.contains(
            "E0304: a secret reaches output conn.password, not declared secret(T): its type is string"
        ),
        "{}",
        r.stderr
    );
}

/// An input of an object type with a secret field: the field is a secret,
/// the others public.
#[test]
fn an_input_of_an_object_type_with_a_secret_field() {
    let s = Scratch::project("secret-fields-input");
    s.write(
        "stacks/app.df",
        "\ninput db: { host: string, password: secret(string) }\nuse fake\n\
         resource leaky.vault v { password = db.password }\n\
         resource leaky.oops o { password = db.host }\n",
    );
    let run = |s: &Scratch| {
        s.run(&[
            "dev",
            "--provider",
            &schema(),
            "plan",
            "app",
            "--set",
            "db.host=h.example",
            "--set",
            "db.password=HUNTER2-INPUT",
        ])
    };
    let r = run(&s).success();
    assert!(
        r.stdout.contains("password = \"h.example\""),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("HUNTER2"), "{}", r.stdout);
    s.write(
        "stacks/app.df",
        "\ninput db: { host: string, password: secret(string) }\nuse fake\n\
         resource leaky.oops o { password = db.password }\n",
    );
    let r = run(&s).failure();
    assert!(
        r.stderr
            .contains("E0304: a secret reaches leaky.oops .password, not marked sensitive"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("HUNTER2"), "{}", r.stderr);
}

/// Another stack's output of an object type with a secret field: the
/// producer stores the field by label and digest and publishes the rest;
/// the reader's `prod.conn.pass` is a secret its provider reads where the
/// producer's holds it, and `prod.conn.host` is a public value.
#[test]
fn another_stacks_output_with_a_secret_field() {
    const SECRET: &str = "FIELD-SECRET-DO-NOT-STORE";
    let s = Scratch::project("secret-fields-stacks");
    s.write(
        "stacks/prod.df",
        "\ninput pw: secret(string)\nuse fake\n\
         resource leaky.vault v { password = pw }\n\
         output conn: { host: string, pass: secret(string) } = { host: \"db.example\", pass: v.password }\n",
    );
    s.write(
        "stacks/app.df",
        "\nuse fake\nuse stacks.prod\n\
         resource leaky.vault copy { backup = prod.conn.pass }\n\
         resource leaky.oops host { password = prod.conn.host }\n",
    );
    let dev = |args: &[&str]| {
        let schema = schema();
        let mut a = vec!["dev", "--provider", schema.as_str()];
        a.extend_from_slice(args);
        s.run(&a)
    };
    let set = format!("pw={SECRET}");
    dev(&["apply", "prod", "--set", &set]).success();
    for f in [
        "dform.state/prod/state.json",
        "dform.state/prod/outputs.json",
    ] {
        let text = s.read(f);
        assert!(!text.contains(SECRET), "{f}:\n{text}");
        assert!(
            text.contains("\"label\": \"output/#conn.pass\""),
            "{f}:\n{text}"
        );
    }
    assert!(
        s.read("dform.state/prod/outputs.json")
            .contains("db.example")
    );

    let r = dev(&["apply", "app", "--set", &set, "--why=none"]).success();
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains(SECRET), "{out}");
    }
    assert!(
        r.stdout
            .contains("+ leaky.vault[\"copy\"]\n  backup = (sensitive prod.conn.pass)\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("password = \"db.example\""),
        "{}",
        r.stdout
    );
    let w: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/app/remote.json")).unwrap();
    assert_eq!(
        w["resources"]["leaky.vault::copy"]["materialized"]["backup"],
        SECRET
    );

    s.write(
        "stacks/app.df",
        "\nuse fake\nuse stacks.prod\nresource leaky.oops leak { password = prod.conn.pass }\n",
    );
    let r = dev(&["plan", "app"]).failure();
    assert!(
        r.stderr
            .contains("E0304: a secret reaches leaky.oops .password, not marked sensitive"),
        "{}",
        r.stderr
    );
}
