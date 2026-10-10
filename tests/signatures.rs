//! Component signatures (R-104): `type database = component { input ..
//! output .. }` names the inputs and outputs a component has, and
//! `component pg_aws: database { .. }` is checked against it, so the
//! guarded copies of one name are interchangeable by their output shape.

mod common;
use common::Scratch;

const SIG: &str = r#"
input cloud: enum("aws", "gcp") = "aws"
type conn = string

#| What every database component gives.
type database = component {
  input name: string
  output conn: conn
}

component pg_aws: database {
  input name: string
  input size: int = 1
  resource net.vpc v { size = size }
  output conn: conn = "aws:${name}"
}

component pg_gcp: database {
  input name: string
  output conn: conn = "gcp:${name}"
}

resource pg_aws store { name = "a" } where cloud == "aws"
resource pg_gcp store { name = "b" } where cloud == "gcp"
resource net.subnet s { size = 3, name = store.conn }
use fake
"#;

fn plan(s: &Scratch, sets: &[&str]) -> common::Run {
    let mut a = vec!["dev", "--world", "w.json"];
    for x in sets {
        a.extend(["--set", x]);
    }
    a.extend(["plan", "--why=none", "p.df"]);
    s.run(&a)
}

/// Two components of one signature under one guarded name: a consumer
/// reads `store.conn` across the pair, whichever holds.
#[test]
fn components_of_one_signature_are_interchangeable() {
    let s = Scratch::new("signatures-ok");
    s.write("p.df", SIG);
    let r = plan(&s, &[]).success();
    assert!(r.stdout.contains("name = \"aws:a\""), "{}", r.stdout);
    let r = plan(&s, &["cloud=gcp"]).success();
    assert!(r.stdout.contains("name = \"gcp:b\""), "{}", r.stdout);
    // `dform doc` prints the signature whole.
    let r = s.run(&["doc", "p.df"]).success();
    assert!(
        r.stdout.contains(
            "### signature `database`\n\n```dform\ntype database = component {\n  input name: \
             string\n  output conn: conn\n}\n```\n\nWhat every database component gives.\n"
        ),
        "{}",
        r.stdout
    );
}

/// What the signature declares and the component does not, or of
/// another type, is an error at the component, as is an input the
/// signature does not declare with no default.
#[test]
fn a_component_that_differs_from_its_signature_is_an_error() {
    let s = Scratch::new("signatures-mismatch");
    s.write(
        "p.df",
        &SIG.replace("output conn: conn = \"gcp:${name}\"", "input tier: string"),
    );
    let r = plan(&s, &[]).failure();
    for want in [
        "p.df:18:1: component pg_gcp has no output conn, which database declares (conn: string)",
        "p.df:18:1: input tier of component pg_gcp is not in database and has no default",
        "the signature database",
    ] {
        assert!(
            common::says_error(&r.stderr, want),
            "{want}\n---\n{}",
            r.stderr
        );
    }
    s.write(
        "p.df",
        &SIG.replace(
            "output conn: conn = \"gcp:${name}\"",
            "output conn: int = 1",
        ),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("output conn of component pg_gcp is int; database declares string"),
        "{}",
        r.stderr
    );
    s.write("p.df", &SIG.replace("pg_gcp: database", "pg_gcp: datebase"));
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr.contains("no component signature `datebase`"),
        "{}",
        r.stderr
    );
}

/// A signature is not instanced: the guarded concrete copies are.
#[test]
fn a_signature_is_not_instanced() {
    let s = Scratch::new("signatures-instance");
    s.write(
        "p.df",
        &SIG.replace(
            "resource pg_gcp store",
            "resource database other {}\nresource pg_gcp store",
        ),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("database is a component signature, which has no resources"),
        "{}",
        r.stderr
    );
}

/// A signature in another module, by the name its `use` binds.
#[test]
fn a_signature_is_read_through_its_module() {
    let s = Scratch::project("signatures-module");
    s.write(
        "kinds.df",
        "\n\ntype database = component {\n  output conn: string\n}\n",
    );
    s.write(
        "stacks/app.df",
        "\n\nuse fake\nuse kinds\n\ncomponent pg: kinds.database {\n  output conn: int = 1\n}\n",
    );
    let r = s.run(&["plan", "app"]).failure();
    assert!(
        r.stderr
            .contains("output conn of component pg is int; kinds.database declares string"),
        "{}",
        r.stderr
    );
}
