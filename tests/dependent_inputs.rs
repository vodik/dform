//! Dependent inputs (R-104): `input gcp_project: string where cloud ==
//! "gcp"` is declared, read and required only where its clause holds;
//! `dform test` enumerates it only in the combinations that declare it; a
//! key is never under a clause; an input declared twice follows the rule
//! every guarded name does.

mod common;
use common::Scratch;

const P: &str = r#"
input cloud: enum("aws", "gcp") = "aws"
input gcp_zone: enum("a", "b") where cloud == "gcp"
input gcp_project: string where cloud == "gcp"
input region: string = "eu" where cloud == "aws"
input region: string = "europe-west1" where cloud == "gcp"
use fake
resource net.vpc v { name = "${gcp_project}-${gcp_zone}-${region}" } where cloud == "gcp"
resource net.vpc w { name = region } where cloud == "aws"
"#;

fn plan(s: &Scratch, sets: &[&str]) -> common::Run {
    let mut a = vec!["dev", "--world", "w.json"];
    for x in sets {
        a.extend(["--set", x]);
    }
    a.extend(["plan", "--why=none", "p.df"]);
    s.run(&a)
}

/// Where the clause does not hold the input is not declared: nothing asks
/// for it. Where it holds, one with no value is required, and each
/// declaration of `region` gives its own default.
#[test]
fn a_dependent_input_is_required_only_where_its_clause_holds() {
    let s = Scratch::new("dependent-required");
    s.write("p.df", P);
    let r = plan(&s, &[]).success();
    assert!(
        r.stdout.contains("+ net.vpc[\"w\"]\n  name = \"eu\"\n"),
        "{}",
        r.stdout
    );
    let r = plan(&s, &["cloud=gcp"]).failure();
    for want in [
        "input gcp_project is required and has no value: its clause holds in this deployment",
        "input gcp_zone is required and has no value: its clause holds in this deployment",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
    let r = plan(&s, &["cloud=gcp", "gcp_project=p", "gcp_zone=b"]).success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"v\"]\n  name = \"p-b-europe-west1\"\n"),
        "{}",
        r.stdout
    );
}

/// The test space enumerates a dependent enum only in the combinations
/// that declare it: three, not four.
#[test]
fn the_test_space_enumerates_only_the_combinations_that_declare_it() {
    let s = Scratch::new("dependent-space");
    s.write("p.df", P);
    let r = s
        .run(&["dev", "--set", "gcp_project=p", "test", "p.df"])
        .success();
    assert!(
        r.stdout.contains(
            "test p: 3 combinations of cloud, gcp_zone\n\
             gcp_project  cloud  gcp_zone  result\n\
             p            aws    -         ok\n\
             p            gcp    a         ok\n\
             p            gcp    b         ok\n"
        ),
        "{}",
        r.stdout
    );
}

/// A key names the deployment, so every deployment has it: it takes no
/// clause. An input declared twice without a clause on each is the
/// error; with another type on each, the error lists them.
#[test]
fn a_key_takes_no_clause_and_a_twice_declared_input_follows_the_rule() {
    let s = Scratch::new("dependent-key");
    s.write(
        "p.df",
        "\ninput cloud: enum(\"aws\", \"gcp\") = \"aws\"\nkey env: string where cloud == \"aws\"\n\
         use fake\n",
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr.contains(
            "key env has a clause: a key names the deployment, so every deployment has it"
        ),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &P.replace("= \"eu\" where cloud == \"aws\"", "= \"eu\""),
    );
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:6:1: `region` is declared twice; give each a `where`"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &P.replace("input region: string = \"eu\"", "input region: int = 1"),
    );
    let r = plan(&s, &[]).failure();
    for want in [
        "input region is declared with another type in each declaration",
        "region: int",
        "region: string",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
}

/// `where` that reads the input itself is a check misspelled, as before.
#[test]
fn a_clause_that_reads_the_input_itself_is_a_misspelled_check() {
    let e = common::error("input replicas: int = 2 where replicas >= 1\n");
    assert!(e.contains("a refinement is spelled `check` (R-1)"), "{e}");
}

/// A component's input may depend on another of its inputs.
#[test]
fn a_components_input_may_depend_on_another() {
    let s = Scratch::new("dependent-component");
    s.write(
        "p.df",
        r#"
component store {
  input cloud: enum("aws", "gcp")
  input project: string where cloud == "gcp"
  resource net.vpc v { name = project } where cloud == "gcp"
  resource net.vpc w { name = "aws" } where cloud == "aws"
}
resource store a { cloud = "aws" }
resource store g { cloud = "gcp", project = "p1" }
use fake
"#,
    );
    let r = plan(&s, &[]).success();
    for want in [
        "+ net.vpc[\"a.w\"]\n    name = \"aws\"\n",
        "+ net.vpc[\"g.v\"]\n    name = \"p1\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
}
