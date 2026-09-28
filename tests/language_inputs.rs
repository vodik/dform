//! Typed stack inputs: a default is a `@default` contribution, `--set` and
//! `--input-file` normal ones; types, required inputs and refinements are
//! checked.

mod common;
use common::Scratch;

const P: &str = r#"edition 2026.
input env: enum(dev, staging, prod) = staging.
input replicas: int = 2 where 1 <= replicas, replicas <= 5.
input nets: list(inet) = [].
input owner: string.
resource net.vpc main { env = E, replicas = R, owner = O } :- env(E), replicas(R), owner(O).
resource net.subnet S { cidr = N } :- nets(Ns), member(Ns, I, N), S = format("s%s", I).
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("lang-inputs");
    s.write("p.df", P);
    s
}

fn plan(s: &Scratch, extra: &[&str]) -> common::Run {
    let mut a = vec!["--file", "p.df", "--world", "w.json"];
    a.extend(extra);
    a.push("plan");
    s.run(&a)
}

#[test]
fn a_default_yields_to_set() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops"]).success();
    assert!(r.stdout.contains("env = \"staging\""), "{}", r.stdout);
    assert!(r.stdout.contains("replicas = 2"), "{}", r.stdout);
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=prod"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
}

#[test]
fn a_required_input_with_no_value_is_an_error_naming_it() {
    let s = scratch();
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:5:1: input owner is required and has no value"),
        "{}",
        r.stderr
    );
}

#[test]
fn set_is_checked_against_the_declaration() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=qa"]).failure();
    assert!(
        r.stderr
            .contains("--set env=qa: input env is enum(dev, staging, prod)"),
        "{}",
        r.stderr
    );
    let r = plan(&s, &["--set", "owner=ops", "--set", "region=x"]).failure();
    assert!(
        r.stderr
            .contains("--set region: the program declares no input region"),
        "{}",
        r.stderr
    );
    // An int where a string is declared is its text.
    let r = plan(&s, &["--set", "owner=42"]).success();
    assert!(r.stdout.contains("owner = \"42\""), "{}", r.stdout);
}

#[test]
fn a_refinement_on_an_input_is_a_deny() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "replicas=9"]).failure();
    assert!(
        r.stderr.contains(
            "input replicas fails its refinement: 1 <= replicas, replicas <= 5 ctx={\"value\":9}"
        ),
        "{}",
        r.stderr
    );
}

/// An input file holds one fact per input; its values are program terms,
/// so lists and `inet(...)` work, and a value of the wrong type is a
/// violation naming the input.
#[test]
fn an_input_file_gives_inputs_as_facts() {
    let s = scratch();
    s.write(
        "prod.df",
        "edition 2026.\nenv(prod).\nowner(\"ops\").\nnets([inet(\"10.0.0.0/24\"), inet(\"10.0.1.0/24\")]).\n",
    );
    let r = plan(&s, &["--input-file", "prod.df"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("+ net.subnet.s1\n  cidr = \"10.0.1.0/24\""),
        "{}",
        r.stdout
    );

    s.write("bad.df", "edition 2026.\nowner(\"ops\").\nnets([\"x\"]).\n");
    let r = plan(&s, &["--input-file", "bad.df"]).failure();
    assert!(
        r.stderr.contains("input nets: [\"x\"] is not list(inet)"),
        "{}",
        r.stderr
    );

    s.write("stray.df", "edition 2026.\nowner(\"ops\").\nregion(x).\n");
    let r = plan(&s, &["--input-file", "stray.df"]).failure();
    assert!(
        r.stderr
            .contains("stray.df:3:1: region/1 is not an input of the program"),
        "{}",
        r.stderr
    );
}

/// A value a rule computes is checked after evaluation.
#[test]
fn a_module_input_of_the_wrong_type_is_a_violation() {
    let s = Scratch::new("lang-inputs-module");
    s.write(
        "p.df",
        "edition 2026.\nmodule m {\n  input n: int.\n  resource net.vpc v { n = N } :- n(N).\n}.\ninstance m a { n = \"three\" }.\n",
    );
    let r = s
        .run(&["--file", "p.df", "--world", "w.json", "plan"])
        .failure();
    assert!(
        r.stderr.contains("input n of m.a: three is not int"),
        "{}",
        r.stderr
    );
}

/// The plan file records each input file's digest: `apply PLAN` reads the
/// same file, and refuses when it changed.
#[test]
fn the_plan_file_records_input_files() {
    let s = scratch();
    s.write("prod.df", "edition 2026.\nenv(prod).\nowner(\"ops\").\n");
    s.run(&[
        "--file",
        "p.df",
        "--world",
        "w.json",
        "--input-file",
        "prod.df",
        "plan",
        "--out",
        "plan.json",
    ])
    .success();
    assert!(s.read("plan.json").contains("\"input_files\""));
    s.run(&["apply", "plan.json"]).success();
    s.write("prod.df", "edition 2026.\nenv(dev).\nowner(\"ops\").\n");
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("--input-file prod.df: changed since the plan"),
        "{}",
        r.stderr
    );
}
