//! Modules, instances and interfaces (E DR-3, DESIGN.org R-5): predicates
//! are private per instance, inputs and outputs are the data boundary, and
//! a policy pack writes without a grant.

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("lang-modules");
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
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
  size(n_) where n(n_)
  resource net.vpc vpc {
    size = s
  } where size(s)
}
instance m a { n = 1 }
instance m b { n = 2 }
"#,
    )
    .success();
    assert!(
        r.stdout.contains("+ net.vpc[\"m.a::vpc\"]\n  size = 1\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ net.vpc[\"m.b::vpc\"]\n  size = 2\n"),
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
big(s) where size(s)
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:6:14: size/1 is private to module m"),
        "{}",
        r.stderr
    );
}

/// An output is `m.i.k` outside; an `addr` output is the instance's
/// resource address, read with a variable instance segment.
#[test]
fn outputs_are_the_interface() {
    let r = plan(
        r#"edition 2026
module m {
  input n: int
  size(n_) where n(n_)
  resource net.vpc vpc {
    size = s_
  } where size(s_)
  output size: int = s_ where size(s_)
  output vpc: addr = vpc
}
instance m a { n = 3 }
inst("a")
resource net.subnet s {
  size = s_
  vpc = v
} where s_ = m.a.size, inst(i), output(m[i], "vpc", v)
"#,
    )
    .success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"s\"]\n  size = 3\n  vpc = \"m.a::vpc\"\n"),
        "{}",
        r.stdout
    );
}

/// `export p` and `contributes` are gone (DESIGN.org R-5): each is an
/// error naming what to write instead.
#[test]
fn export_of_a_relation_and_contributes_are_errors() {
    let r = plan(
        r#"edition 2026
module m {
  export size
  contributes need
  size(1)
}
instance m a {}
"#,
    )
    .failure();
    assert!(
        r.stderr.contains("p.df:3:10: expected `type`, found `size`"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("`export p` is gone: a module's relations are private to each instance"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("p.df:4:3: expected a statement, found `contributes`"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("`contributes` is gone (R-5): a write needs no grant"),
        "{}",
        r.stderr
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
    size = n_
  } where n(n_)
}
instance m a {}
instance m b { n = 1 }
"#;
    let r = plan(src).success();
    assert!(
        r.stdout.contains("+ net.vpc[\"m.a::vpc\"]\n  size = 7\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ net.vpc[\"m.b::vpc\"]\n  size = 1\n"),
        "{}",
        r.stdout
    );

    let r = plan(&src.replace("input n: int = 7", "input n: int")).failure();
    assert!(
        r.stderr
            .contains("p.df:8:1: instance m a does not set required input n"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_refinement_on_a_module_input_is_a_deny() {
    let r = plan(
        r#"edition 2026
module m {
  input n: int check n <= 5
  resource net.vpc vpc {
    size = n_
  } where n(n_)
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

/// A policy pack writes any attribute without a grant (DESIGN.org R-5):
/// ranks decide, and the stratifier partitions the write by its head. A
/// pack's own relations stay private.
#[test]
fn a_pack_writes_without_a_grant_and_its_relations_are_private() {
    let src = r#"edition 2026
resource net.vpc main { cidr = "10.0.0.0/16" }
policy tags {
  team("x")
  arg(t, a, "tags", { team: v }) where want(t, a), team(v)
  set a.cidr = "10.9.0.0/16" @override where a in net.vpc
}
use tags
"#;
    let r = plan(src).success();
    assert!(r.stdout.contains("cidr = \"10.9.0.0/16\""), "{}", r.stdout);
    assert!(r.stdout.contains("team = \"x\""), "{}", r.stdout);
    let r = plan(&format!("{src}seen(v) where team(v)\n")).failure();
    assert!(
        r.stderr.contains("p.df:9:15: team/1 is private to policy tags"),
        "{}",
        r.stderr
    );
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
            "dev",
            "--world",
            "w.json",
            "plan",
            "--set",
            "replicas=3",
            "p.df",
        ])
        .success();
    assert!(
        r.stdout
            .contains("+ compute.vm[\"app.blue::vm\"]\n  count = 3\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ compute.vm[\"app.green::vm\"]\n  count = 7\n"),
        "{}",
        r.stdout
    );
}

/// dform.df's peering rule joins each `vpc_peer_inst` edge to its own
/// instances' vpcs: with a second edge (to a third network) there are two
/// peerings, each named for its edge and holding its own pair. Unjoined, each
/// peering got both accepters and conflicted.
#[test]
fn dform_df_peers_each_edge_with_its_own_pair() {
    let s = Scratch::new("lang-modules-peering");
    common::copy_dir(&common::repo().join("examples/demo"), &s.dir);
    let third = r#"
instance network third {
  vpc_net = inet("10.70.0.0/16")
}
vpc_peer_inst("main", "third")
"#;
    s.write(
        "stacks/dform.df",
        &format!("{}{third}", s.read("stacks/dform.df")),
    );
    let r = s.run(&["plan", "dform"]).success();
    assert_eq!(
        r.stdout.matches("\n+ net.vpc_peering[").count(),
        2,
        "{}",
        r.stdout
    );
    for (name, accepter) in [("peer-main-peer", "peer"), ("peer-main-third", "third")] {
        let want = format!(
            "+ net.vpc_peering[\"{name}\"]\n  accepter_vpc_id = ?net.vpc[\"network.{accepter}::vpc\"].id\n  requester_vpc_id = ?net.vpc[\"network.main::vpc\"].id\n"
        );
        assert!(r.stdout.contains(&want), "{want}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("conflict"), "{}", r.stdout);
}
