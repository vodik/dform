//! R-74: a typed `let` (`let k: T = t`) checks its value as any typed
//! position does and types the columns its reads reach; a bare name two
//! resources share is the one of a typed `let`'s resource type.

mod common;
use common::{Scratch, error, mock};
use dform::parser::parse_file;
use dform::transform;

/// A vpc and a subnet both named `main`, then `rest`.
fn two_mains(rest: &str) -> String {
    format!(
        "provider fake\n\n\
         resource net.vpc main {{ cidr = \"10.0.0.0/16\" }}\n\
         resource net.subnet main {{\n  vpc = net.vpc[\"main\"]\n  cidr = \"10.0.1.0/24\"\n}}\n{rest}"
    )
}

fn plan(name: &str, src: &str) -> common::Run {
    let s = Scratch::new(name);
    s.write("p.df", src);
    mock(&s, &["plan"])
}

/// A literal is read as the declared type; the plan gives the value.
#[test]
fn a_typed_let_reads_its_literal_as_its_type() {
    let r = plan(
        "typed-let-literal",
        &two_mains(
            "let region: enum(\"eu\", \"us\") = \"eu\"\n\
             let block: inet = \"10.2.0.0/16\"\n\
             resource net.vpc other {\n  cidr = \"${block}\"\n  tags = { region }\n}\n",
        ),
    )
    .success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"other\"]\n  cidr = \"10.2.0.0/16\"\n  tags.region = \"eu\"\n"),
        "{}",
        r.stdout
    );
}

/// What does not fit the type is an error at the value, naming the let.
#[test]
fn a_typed_let_checks_its_value() {
    for (rest, want) in [
        (
            "let region: enum(\"eu\", \"us\") = \"ca\"",
            "let region is enum(\"eu\", \"us\"): \"ca\" is not one of its members",
        ),
        (
            "let block: inet = \"10.2\"",
            "let block is an inet: \"10.2\" is not a network",
        ),
        (
            "let v: net.subnet = net.vpc[\"other\"]",
            "let v takes a ref(net.subnet), got net.vpc[\"other\"]",
        ),
        (
            "let v: net.vpc = \"main\"",
            "let v takes a ref(net.vpc): a resource",
        ),
        (
            "resource compute.vm solo { size = \"s\" }\nlet n: string = solo",
            "let n is string, not a reference: compute.vm[\"solo\"] is no string",
        ),
        (
            "let n: int = 1\nlet n: string = \"a\"",
            "`let n` is declared `int` in one row and `string` in another",
        ),
    ] {
        let e = error(&two_mains(rest));
        assert!(e.contains(want), "{rest}\n{e}");
    }
}

/// `let k: T = t` with a resource type: a bare name two resources share
/// is the one of type `T`, the let holds the reference, and a dot reads
/// through it. Untyped, the name is the error listing both.
#[test]
fn a_typed_let_picks_the_resource_of_its_type() {
    let r = plan(
        "typed-let-ref",
        &two_mains(
            "let v: net.vpc = main\nlet s: ref(net.subnet) = main\n\
             resource net.subnet other {\n  vpc = v\n  cidr = s.cidr\n}\n",
        ),
    )
    .success();
    assert!(
        r.stdout.contains(
            "+ net.subnet[\"other\"]\n  cidr = \"10.0.1.0/24\"\n  vpc = ?net.vpc[\"main\"]\n"
        ),
        "{}",
        r.stdout
    );
    let e = error(&two_mains("let v = main\n"));
    assert!(
        e.contains(
            "`main` names 2 resources: write one of net.vpc[\"main\"], net.subnet[\"main\"]"
        ),
        "{e}"
    );
}

/// The declared type is the column of the let's reads (R-34): a variable
/// bound to it is that type, though nothing else gives it one.
#[test]
fn a_typed_let_types_the_columns_it_reaches() {
    let sigs = |src: &str| -> Vec<String> {
        let p = parse_file("t.df", &format!("\n{src}")).unwrap_or_else(|e| panic!("{e:#}"));
        let l = transform::lower(&p).unwrap_or_else(|e| panic!("{e:#}"));
        l.signatures.values().map(|s| s.to_string()).collect()
    };
    let typed = sigs("decl src(v: any)\nlet host: string = x where src(x)\nq(h) where h = host\n");
    assert!(typed.contains(&"q(h: string)".to_string()), "{typed:?}");
    let untyped = sigs("decl src(v: any)\nlet host = x where src(x)\nq(h) where h = host\n");
    assert!(untyped.contains(&"q(h: any)".to_string()), "{untyped:?}");
}
