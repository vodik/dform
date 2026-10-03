//! `dform test` (R-32): the program's denies are its tests. It evaluates
//! the program once per combination of its inputs (each enum input's
//! values, a bool both ways, a key's enum values; the rest their
//! defaults) against an empty mock world, and every deny must hold in
//! each. It prints a matrix, a row per combination, its inputs and its
//! result; a failure then as the command that plans it. `scenario` is
//! gone.

mod common;
use common::{Scratch, repo};

const P: &str = r#"edition 2026
input env: enum("dev", "prod")
input public: bool = false
input size: int = 1
provider fake
resource net.vpc main {
  cidr = "10.0.0.0/16"
  size
}
resource db.postgres main {
  multi_az = true
  public
} where env == "prod"
deny "prod has a database" where env == "prod", not main in db.postgres
deny "dev has no database" where env == "dev", _ in db.postgres
deny "a database is never public" where d in db.postgres, d.public
"#;

#[test]
fn test_runs_the_denies_over_every_combination() {
    let s = Scratch::new("test-space");
    s.write("p.df", P);
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "test p: 4 combinations of env, public\n\
             env   public  result\n\
             dev   false   ok\n\
             dev   true    ok\n\
             prod  false   ok\n\
             prod  true    denied\n\
             denied  dform plan p.df --set env=prod --set public=true\n  \
             - a database is never public\n\
             test p: 4 combinations, 1 failed\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr.contains("1 of 4 combinations failed"),
        "{}",
        r.stderr
    );
    // Nothing touched: no state, no world.
    assert!(!s.path("dform.state").exists());
}

/// `--set` pins an input: the rest of the space runs.
#[test]
fn a_set_pins_an_input() {
    let s = Scratch::new("test-pinned");
    s.write("p.df", P);
    let r = s.run(&["test", "p.df", "--set", "public=false"]).success();
    assert!(
        r.stdout.contains("test p: 2 combinations of env\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("public  env   result\nfalse   dev   ok\nfalse   prod  ok\n"),
        "{}",
        r.stdout
    );
}

/// The target pins a key; with none, each of the key's values runs.
#[test]
fn the_target_pins_a_key() {
    let s = Scratch::project("test-key");
    s.write(
        "stacks/app.df",
        &P.replace("input env: enum", "key env: enum"),
    );
    let r = s.run(&["test", "app"]).failure();
    assert!(
        r.stdout
            .contains("denied  dform plan app env=prod --set public=true\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["test", "app", "env=dev"]).success();
    assert!(
        r.stdout.contains(
            "test app: 2 combinations of public\n\
             env  public  result\n\
             dev  false   ok\n\
             dev  true    ok\n"
        ),
        "{}",
        r.stdout
    );
}

/// An input neither pinned nor bounded by its type, with no default, is
/// an error naming it.
#[test]
fn an_unbounded_input_is_an_error_naming_it() {
    let s = Scratch::new("test-unbounded");
    s.write("p.df", &P.replace("input size: int = 1", "input size: int"));
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("test: input size: int neither pinned nor bounded by its type"),
        "{}",
        r.stderr
    );
    s.run(&["test", "p.df", "--set", "size=3", "--set", "public=false"])
        .success();
}

/// `scenario` is gone, and says what replaces it.
#[test]
fn a_scenario_is_an_error_naming_test() {
    let s = Scratch::new("test-scenario");
    s.write(
        "p.df",
        &format!("{P}scenario prod {{\n  set env = \"prod\"\n}}\n"),
    );
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stderr.contains("`scenario` is gone (R-32)"),
        "{}",
        r.stderr
    );
}

#[test]
fn the_demo_denies_hold_in_every_env() {
    let s = Scratch::new("test-demo");
    common::copy_dir(&repo().join("examples/demo"), &s.dir);
    let r = s.run(&["test", "dform"]).success();
    assert!(
        r.stdout.contains(
            "test dform: 3 combinations of env\n\
             env      result\n\
             dev      ok\n\
             staging  ok\n\
             prod     ok\n\
             test dform: 3 combinations, 0 failed\n"
        ),
        "{}",
        r.stdout
    );
}

/// The input space is the stack's own inputs, each leaf of an object
/// input, and every used module's input the `use` block leaves to the
/// stack (R-54, R-55): each by the name `--set` gives it.
#[test]
fn the_space_is_every_input_the_stack_gives() {
    let s = Scratch::project("test-union");
    s.write(
        "pg.df",
        "edition 2026\ninput public: bool = false\ninput multi_az: bool = true\n\
         resource db.postgres main {\n  public\n  multi_az\n}\n",
    );
    s.write(
        "p.df",
        "edition 2026\ninput cluster {\n  tier: enum(\"a\", \"b\") = \"a\"\n  size: int = 1\n}\n\
         use pg { multi_az = true }\nprovider fake\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  tier = cluster.tier\n}\n\
         deny \"a database is never public\" where d in db.postgres, d.public\n",
    );
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "test p: 4 combinations of cluster.tier, pg.public\n\
             cluster.tier  pg.public  result\n\
             a             false      ok\n\
             a             true       denied\n\
             b             false      ok\n\
             b             true       denied\n\
             denied  dform plan p --set cluster.tier=a --set pg.public=true\n"
        ),
        "{}",
        r.stdout
    );
    // Pinned by its address, or by its object's.
    let r = s
        .run(&[
            "test",
            "p.df",
            "--set",
            "pg.public=false",
            "--set",
            "cluster.tier=b",
        ])
        .success();
    assert!(r.stdout.contains("test p: 1 combination"), "{}", r.stdout);
}

/// An input a `set .. where` gives is the program's to decide (R-38): no
/// axis, and the denies run with it applied.
#[test]
fn an_input_a_set_gives_is_no_axis() {
    let s = Scratch::new("test-set");
    s.write(
        "p.df",
        "edition 2026\ninput env: enum(\"dev\", \"prod\") = \"dev\"\n\
         input multi_az: bool = false\ninput public: bool = false\nprovider fake\n\
         set multi_az = true where env == \"prod\"\n\
         resource db.postgres main {\n  multi_az\n  public\n}\n\
         deny \"prod is multi_az\" where env == \"prod\", d in db.postgres, not d.multi_az\n",
    );
    let r = s.run(&["test", "p.df"]).success();
    assert!(
        r.stdout.contains("test p: 4 combinations of env, public\n"),
        "{}",
        r.stdout
    );
}

/// A deny's doc comment is its test's doc: `dform test` prints it beside
/// the deny that failed (R-30, with `scenario` gone).
#[test]
fn a_deny_prints_with_its_doc() {
    let s = Scratch::new("test-doc");
    s.write(
        "p.df",
        &P.replace(
            "deny \"a database is never public\"",
            "#| A public database is one leaked credential from a breach.\n\
             deny \"a database is never public\"",
        ),
    );
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains(
            "  - a database is never public   #| A public database is one leaked credential \
             from a breach.\n"
        ),
        "{}",
        r.stdout
    );
}
