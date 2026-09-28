//! Modules, instances, interfaces and grants (E DR-3): predicates are
//! private per instance, inputs and outputs are the data boundary, and a
//! policy pack writes only inside its grants.

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("lang-modules");
    s.write("p.df", src);
    s.run(&["--file", "p.df", "--world", "w.json", "plan"])
}

/// Two instances of one module each define `size/1`; privately, so neither
/// sees the other's. Were the relation global, each vpc would get both
/// sizes and conflict.
#[test]
fn a_module_predicate_is_private_to_its_instance() {
    let r = plan(
        r#"edition 2026
module m {
  input n: int
  size(n_) if n(n_)
  resource net.vpc vpc {
    for size(s)
    size = s
  }
}
instance m a { n = 1 }
instance m b { n = 2 }
"#,
    )
    .success();
    assert!(
        r.stdout.contains("+ net.vpc.m.a::vpc\n  size = 1\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ net.vpc.m.b::vpc\n  size = 2\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("conflicts:"), "{}", r.stdout);
}

#[test]
fn reading_a_private_predicate_is_an_error_naming_the_module() {
    let r = plan(
        r#"edition 2026
module m {
  size(1)
}
instance m a {}
big(s) if size(s)
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:6:11: size/1 is private to module m"),
        "{}",
        r.stderr
    );
}

/// `export p/N` is `m.i.p` outside; an `addr` output is the instance's
/// resource address, read with a variable instance segment.
#[test]
fn exports_and_outputs_are_the_interface() {
    let r = plan(
        r#"edition 2026
module m {
  input n: int
  output vpc: addr
  export size/1
  size(n_) if n(n_)
  resource net.vpc vpc {
    for size(s_)
    size = s_
  }
  output vpc = vpc
}
instance m a { n = 3 }
inst("a")
resource net.subnet s {
  for m.a.size(s_), inst(i), output(m[i], "vpc", v)
  size = s_
  vpc = v
}
"#,
    )
    .success();
    assert!(
        r.stdout
            .contains("+ net.subnet.s\n  size = 3\n  vpc = \"m.a::vpc\"\n"),
        "{}",
        r.stdout
    );
}

/// An input default is a `@default` contribution: the instance's value wins
/// where it sets one; a required input it does not set is a compile error.
#[test]
fn an_input_default_yields_to_the_instance() {
    let src = r#"edition 2026
module m {
  input n: int = 7
  resource net.vpc vpc {
    for n(n_)
    size = n_
  }
}
instance m a {}
instance m b { n = 1 }
"#;
    let r = plan(src).success();
    assert!(
        r.stdout.contains("+ net.vpc.m.a::vpc\n  size = 7\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ net.vpc.m.b::vpc\n  size = 1\n"),
        "{}",
        r.stdout
    );

    let r = plan(&src.replace("input n: int = 7", "input n: int")).failure();
    assert!(
        r.stderr
            .contains("p.df:9:1: instance m a does not set required input n"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_refinement_on_a_module_input_is_a_deny() {
    let r = plan(
        r#"edition 2026
module m {
  input n: int where n <= 5
  resource net.vpc vpc {
    for n(n_)
    size = n_
  }
}
instance m a { n = 9 }
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("input n of m.a fails its refinement: n <= 5 ctx={\"value\":9}"),
        "{}",
        r.stderr
    );
}

/// A policy pack's `arg` must fall in a grant, and a pack's own relations
/// are private unless granted.
#[test]
fn a_pack_writes_only_inside_its_grants() {
    let src = r#"edition 2026
resource net.vpc main { cidr = "10.0.0.0/16" }
policy tags {
  contributes _.tags
  arg(t, a, .tags, { team: "x" }) if want(t, a)
  arg(net.vpc, a, .cidr, "10.9.0.0/16") @override if want(net.vpc, a)
}
apply tags
"#;
    let r = plan(src).failure();
    assert!(
        r.stderr
            .contains("p.df:6:3: policy tags writes .cidr of net.vpc outside its grants"),
        "{}",
        r.stderr
    );
    let r = plan(&src.replace(
        "contributes _.tags",
        "contributes _.tags\n  contributes net.vpc.cidr",
    ))
    .success();
    assert!(r.stdout.contains("cidr = \"10.9.0.0/16\""), "{}", r.stdout);
}

/// A stack input passed to a module input of the same name: the
/// instance's input cell is its own node (partitioned by the instance's
/// scope), not the stack's, so `replicas = replicas` is not a read of the
/// aggregate it feeds. It was a negative cycle.
#[test]
fn a_stack_input_passed_to_a_module_input_of_the_same_name_stratifies() {
    let s = Scratch::new("lang-modules");
    s.write(
        "p.df",
        r#"edition 2026
input replicas: int = 2
module app {
  input replicas: int
  resource compute.vm vm {
    count = replicas
  }
}
instance app blue { replicas = replicas }
instance app green { replicas = 7 }
"#,
    );
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--world",
            "w.json",
            "plan",
            "--set",
            "replicas=3",
        ])
        .success();
    assert!(
        r.stdout
            .contains("+ compute.vm.app.blue::vm\n  count = 3\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ compute.vm.app.green::vm\n  count = 7\n"),
        "{}",
        r.stdout
    );
}
