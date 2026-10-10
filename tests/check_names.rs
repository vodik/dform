//! A check's body is a clause like any other (After the input-check
//! landing): every name in scope reads as it would anywhere else, by the
//! lexical rule, so a check reads another input, an object field reads a
//! sibling field, and a check reads a `let`. Before this, a bare name in a
//! check that was not the input itself was the string of its name
//! (`agents <= max` compared 2 with "max" and failed as a type error).
//! Such a check reads more than the value given, so it is the
//! evaluation's: a deny (tests/input_check.rs).

mod common;
use common::Scratch;

fn project(name: &str, stack: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/app.df", stack);
    s
}

const RESOURCE: &str = "use fake\n\
                        resource net.vpc \"agent-${i}\" { cidr = \"10.0.0.0/16\" } where i in 0..agents\n";

/// `check agents <= max`: `max` is the sibling input, its value.
#[test]
fn a_check_reads_a_sibling_input() {
    let s = project(
        "check-names-sibling",
        &format!(
            "key env: enum(dev, prod)\n\
             input max: int = 3\n\
             input agents: int = 2 check agents <= max\n\
             {RESOURCE}"
        ),
    );
    s.run(&["plan", "app", "env=dev"]).success();
    let r = s
        .run(&["plan", "app", "env=dev", "--set", "agents=5"])
        .failure();
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains("  fails  input agents check ")
            && r.stdout
                .contains("  stacks/app.df:3  1 fails\n    value = 5\n"),
        "{}",
        r.stdout
    );
    s.run(&[
        "plan", "app", "env=dev", "--set", "agents=5", "--set", "max=9",
    ])
    .success();
}

/// An object input's field check reads a sibling field by the object's
/// path, as anywhere in the stack.
#[test]
fn an_object_fields_check_reads_a_sibling_field() {
    let s = project(
        "check-names-field",
        "key env: enum(dev, prod)\n\
         input db { lo: int = 1, hi: int = 4 check hi >= db.lo }\n\
         use fake\n\
         resource net.vpc \"main\" { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["plan", "app", "env=dev"]).success();
    let r = s
        .run(&["plan", "app", "env=dev", "--set", "db.lo=6"])
        .failure();
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains("  fails  input db.hi check ")
            && r.stdout
                .contains("  stacks/app.df:2  1 fails\n    value = 4\n"),
        "{}",
        r.stdout
    );
    s.run(&["plan", "app", "env=dev", "--set", "db.lo=4"])
        .success();
}

/// A check reads a `let` of the module.
#[test]
fn a_check_reads_a_let() {
    let s = project(
        "check-names-let",
        &format!(
            "key env: enum(dev, prod)\n\
             input agents: int = 2 check agents <= limit\n\
             let limit = 3\n\
             {RESOURCE}"
        ),
    );
    s.run(&["plan", "app", "env=dev"]).success();
    let r = s
        .run(&["plan", "app", "env=dev", "--set", "agents=5"])
        .failure();
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains("  fails  input agents check ")
            && r.stdout
                .contains("  stacks/app.df:2  1 fails\n    value = 5\n"),
        "{}",
        r.stdout
    );
    s.run(&["plan", "app", "env=dev", "--set", "agents=3"])
        .success();
}

/// A component's input check reads the component's sibling input of the
/// copy it is checked in, as the component's body does.
#[test]
fn a_components_check_reads_its_copys_sibling() {
    let s = project(
        "check-names-component",
        "key env: enum(dev, prod)\n\
         component pool {\n  input max: int = 3\n  input n: int check n <= max\n\
         \x20 resource net.vpc v { cidr = \"10.0.0.0/16\" }\n}\n\
         use fake\n\
         resource pool a { n = 2 }\n\
         resource pool b { n = 5 }\n\
         resource pool c { n = 5, max = 9 }\n",
    );
    let r = s.run(&["plan", "app", "env=dev"]).failure();
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(r.summary().ends_with("policy: 1 fails"), "{}", r.stdout);
    assert!(
        r.stdout.contains("  fails  input n check ")
            && r.stdout.contains("    of = \"b\", value = 5\n"),
        "{}",
        r.stdout
    );
}
