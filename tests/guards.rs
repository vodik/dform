//! Guarded declarations (R-104): a named statement may be declared more
//! than once in a scope when every declaration of the name has a clause;
//! one with none is the only one allowed. Two that both hold at evaluation
//! are a deny naming both sites, and a read of the name is the one that
//! holds.

mod common;
use common::Scratch;

fn plan(s: &Scratch, args: &[&str]) -> common::Run {
    let mut a = vec!["dev", "--world", "w.json"];
    a.extend_from_slice(args);
    a.extend(["plan", "--why=none", "p.df"]);
    s.run(&a)
}

const PAIR: &str = r#"
input cloud: enum("aws", "gcp") = "aws"
type conn = string
component pg_aws {
  input name: string
  resource net.vpc v { size = 1 }
  output conn: conn = "aws:${name}"
}
component pg_gcp {
  input name: string
  resource net.vpc v { size = 2 }
  output conn: conn = "gcp:${name}"
}
instance pg_aws store { name = "a" } where cloud == "aws"
instance pg_gcp store { name = "b" } where cloud == "gcp"
resource net.subnet s { size = 3, name = store.conn }
use fake
"#;

/// Two copies of one name, each under its clause: the plan has the copy
/// that holds, and a read of `store.conn` is its output.
#[test]
fn a_guarded_pair_is_the_copy_that_holds() {
    let s = Scratch::new("guards-pair");
    s.write("p.df", PAIR);
    let r = plan(&s, &[]).success();
    for want in [
        "+ net.subnet[\"s\"]\n  name = \"aws:a\"\n",
        "+ pg_aws[\"store\"]\n  + net.vpc[\"store.v\"]\n    size = 1\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("pg_gcp"), "{}", r.stdout);
    let r = plan(&s, &["--set", "cloud=gcp"]).success();
    for want in [
        "+ net.subnet[\"s\"]\n  name = \"gcp:b\"\n",
        "+ pg_gcp[\"store\"]\n  + net.vpc[\"store.v\"]\n    size = 2\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    // `dform test` runs both members of the enum: each picks one.
    let r = s.run(&["test", "p.df"]).success();
    assert!(r.stdout.contains("2 combinations"), "{}", r.stdout);
}

/// The compiler does not prove the clauses exclusive; the evaluation
/// does: two that both hold are a deny naming both sites.
#[test]
fn two_that_both_hold_are_denied_naming_both() {
    let s = Scratch::new("guards-both");
    s.write(
        "p.df",
        &PAIR.replace("where cloud == \"gcp\"", "where cloud != \"gcp\""),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr.contains(
            "`store` is declared twice and both declarations hold: `instance pg_aws store` at \
             p.df:14:1 and `instance pg_gcp store` at p.df:15:1"
        ),
        "{}",
        r.stderr
    );
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout
            .contains("denied  dform plan p.df --set cloud=aws\n  - `store` is declared twice"),
        "{}",
        r.stdout
    );
}

/// A name declared twice where one declaration has no clause is the
/// compile error at the second, naming the first.
#[test]
fn an_unguarded_duplicate_is_an_error() {
    let s = Scratch::new("guards-unguarded");
    s.write("p.df", &PAIR.replace(" where cloud == \"gcp\"", ""));
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:15:1: `store` is declared twice; give each a `where`"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("first here"), "{}", r.stderr);
}

/// A read of a guarded name whose declarations differ is an error that
/// names every declaration and says a signature unifies them; one that
/// lacks the item is one too.
#[test]
fn a_read_across_declarations_of_other_types_is_an_error() {
    let s = Scratch::new("guards-types");
    s.write(
        "p.df",
        &PAIR.replace(
            "output conn: conn = \"gcp:${name}\"",
            "output conn: int = 2",
        ),
    );
    let r = plan(&s, &[]).failure();
    for want in [
        "`store.conn` has another type in each declaration of `store`",
        "instance pg_aws store: output conn: conn",
        "instance pg_gcp store: output conn: int",
        "a component signature unifies them",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
    s.write(
        "p.df",
        &PAIR.replace(
            "output conn: conn = \"gcp:${name}\"",
            "output other: int = 2",
        ),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("`store.conn`: `instance pg_gcp store` has no `conn`"),
        "{}",
        r.stderr
    );
}

/// `use` is a named statement like `instance`: two modules under one name,
/// each under its clause, and `m.x` is the one that holds.
#[test]
fn a_guarded_use_pair_reads_the_one_that_holds() {
    let s = Scratch::project("guards-use");
    s.write(
        "db_aws.df",
        "\n\nlet engine = \"aurora\"\nresource db.postgres main { public = false }\n",
    );
    s.write(
        "db_gcp.df",
        "\n\nlet engine = \"cloudsql\"\nresource db.postgres main { public = false }\n",
    );
    s.write(
        "stacks/app.df",
        r#"

input cloud: enum("aws", "gcp") = "aws"
use fake
use db_aws as store where cloud == "aws"
use db_gcp as store where cloud == "gcp"
resource net.vpc v { name = store.engine }
"#,
    );
    let r = s.run(&["plan", "--why=none", "app"]).success();
    assert!(r.stdout.contains("name = \"aurora\""), "{}", r.stdout);
    assert!(
        r.stdout.contains("+ db.postgres[\"store.main\"]"),
        "{}",
        r.stdout
    );
    let r = s
        .run(&["plan", "--why=none", "app", "--set", "cloud=gcp"])
        .success();
    assert!(r.stdout.contains("name = \"cloudsql\""), "{}", r.stdout);
}

const PROVIDERS: &str = r#"
input cloud: enum("aws", "gcp") = "aws"
use fake { region = "eu-west-1" } where cloud == "aws"
use fake { region = "us-east-1" } where cloud == "gcp"
use gke where cloud == "gcp"
resource net.vpc v { size = 1 }
"#;

/// A provider under a clause is configured, and serves, only where its
/// clause holds: its `provider_config` is derived under the clause (with
/// no settings too), so the provider waits for it (`configure_from`).
/// `effects` says where each starts.
#[test]
fn a_guarded_provider_is_configured_where_its_clause_holds() {
    let s = Scratch::new("guards-providers");
    s.write("p.df", PROVIDERS);
    let config = |set: &str| {
        s.run(&["dev", "--set", set, "query", "provider_config", "p.df"])
            .success()
            .stdout
    };
    let aws = config("cloud=aws");
    assert!(aws.contains("\"fake\"  {region: \"eu-west-1\"}"), "{aws}");
    assert!(!aws.contains("us-east-1") && !aws.contains("gke"), "{aws}");
    let gcp = config("cloud=gcp");
    assert!(gcp.contains("\"fake\"  {region: \"us-east-1\"}"), "{gcp}");
    assert!(gcp.contains("\"gke\"   {}"), "{gcp}");
    let r = s.run(&["dev", "effects", "p.df"]).success();
    for want in [
        "stack  uses    provider fake when cloud == \"aws\"\n",
        "stack  uses    provider fake when cloud == \"gcp\"\n",
        "stack  uses    provider gke when cloud == \"gcp\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    // A provider the clause leaves out serves nothing: the vpc it would
    // serve is planned only where it holds.
    s.write(
        "q.df",
        "\ninput cloud: enum(\"aws\", \"gcp\") = \"aws\"\nuse fake where cloud == \"gcp\"\n\
         resource net.vpc v { size = 1 } where cloud == \"gcp\"\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "q.df"])
        .success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "--set",
            "cloud=gcp",
            "plan",
            "q.df",
        ])
        .success();
    assert!(r.stdout.contains("+ net.vpc v"), "{}", r.stdout);
}

/// Two declarations of a provider that both hold are the deny; one with
/// no clause beside another is the compile error.
#[test]
fn guarded_providers_follow_the_rule() {
    let s = Scratch::new("guards-providers-rule");
    s.write(
        "p.df",
        &PROVIDERS.replace(
            "where cloud == \"gcp\"\nuse gke",
            "where cloud != \"gcp\"\nuse gke",
        ),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr.contains(
            "`use fake` is declared twice and both declarations hold: `use fake` at \
             p.df:3:1 and `use fake` at p.df:4:1"
        ),
        "{}",
        r.stderr
    );
    s.write("p.df", &PROVIDERS.replace(" where cloud == \"aws\"", ""));
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:4:1: `fake` is declared twice; give each a `where`"),
        "{}",
        r.stderr
    );
}

/// Over enum inputs the clauses are decided at compile time, over the
/// product `dform test` enumerates: a combination where no declaration
/// holds, and one where two do, are warnings naming the sites.
#[test]
fn the_enum_lints_name_a_gap_and_an_overlap() {
    let s = Scratch::new("guards-lints");
    s.write(
        "p.df",
        &PAIR
            .replace(
                "enum(\"aws\", \"gcp\")",
                "enum(\"aws\", \"gcp\", \"azure\")",
            )
            .replace("where cloud == \"gcp\"", "where cloud != \"gcp\""),
    );
    let r = plan(&s, &[]).failure();
    for want in [
        "warning: `store` has no declaration when cloud == \"gcp\" (declared at p.df:14:1, \
         p.df:15:1)",
        "warning: `store`: the clauses of two declarations both hold when cloud == \"aws\": \
         p.df:14:1 and p.df:15:1",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
    // Exclusive and exhaustive: no warning.
    s.write("p.df", PAIR);
    let r = plan(&s, &[]).success();
    assert!(!r.stderr.contains("warning"), "{}", r.stderr);
}

/// The plan file and `plan --json` list every name declared more than
/// once, so a review sees a pair a refactor enabled; the text does not.
#[test]
fn the_plan_file_lists_the_guarded_groups() {
    let s = Scratch::new("guards-plan-file");
    s.write("p.df", PAIR);
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    let f = s.json("plan.json");
    assert_eq!(
        f["guarded"],
        serde_json::json!([{"name": "store", "declarations": 2}]),
        "{f}"
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--json", "p.df"])
        .success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        j["guarded"],
        serde_json::json!([{"name": "store", "declarations": 2}])
    );
    let r = plan(&s, &[]).success();
    assert!(!r.stdout.contains("declarations"), "{}", r.stdout);
}
