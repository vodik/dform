//! An input typed by a resource type (`input subnet: net.subnet`) takes a
//! reference to a resource of it: it is `ref(T)`, as a provider's type is
//! written anywhere else a type is (docs/grammar.md "Inputs and outputs").

mod common;
use common::Scratch;

fn program(ty: &str, given: &str) -> String {
    format!(
        "use fake\n\
         component sub {{\n  input vpc: {ty}\n  \
         resource net.subnet s {{ cidr = \"10.0.1.0/24\", vpc_id = vpc }}\n}}\n\
         resource net.vpc main {{ cidr = \"10.0.0.0/16\" }}\n\
         resource net.subnet other {{ cidr = \"10.0.2.0/24\" }}\n\
         resource sub a {{ vpc = {given} }}\n"
    )
}

fn plan(name: &str, src: &str) -> common::Run {
    let s = Scratch::new(name);
    s.write("p.df", src);
    s.run(&["plan", "p.df"])
}

/// `input vpc: net.vpc` plans as `input vpc: ref(net.vpc)` does, and
/// refuses a resource of another type the same way.
#[test]
fn a_resource_type_is_a_reference_to_one() {
    let bare = plan("typed-bare", &program("net.vpc", "main")).success();
    let written = plan("typed-ref", &program("ref(net.vpc)", "main")).success();
    assert_eq!(bare.stdout, written.stdout);
    assert!(bare.stdout.contains("vpc_id = main"), "{}", bare.stdout);
    let r = plan("typed-other", &program("net.vpc", "other")).failure();
    assert!(
        r.stderr
            .contains("input vpc of component sub takes a ref(net.vpc), got net.subnet[\"other\"]"),
        "{}",
        r.stderr
    );
}

/// A stack's input takes one too; a type of a namespace the compiler knows
/// every type of must be one of them.
#[test]
fn a_stack_input_takes_a_provider_type() {
    let r = plan(
        "typed-stack",
        "input ns: k8s.namespace\noutput n = ns\nuse fake\n",
    )
    .failure();
    assert!(
        r.stderr.contains("input ns is required and has no value"),
        "{}",
        r.stderr
    );
    let r = plan("typed-typo", "input v: net.vcp\nuse fake\n").failure();
    assert!(
        r.stderr.contains("stack input v: unknown type net.vcp"),
        "{}",
        r.stderr
    );
}
