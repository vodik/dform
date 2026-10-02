//! `dform dev graph`: DOT for the resource DAG, the partition graph and any
//! binary relation, with deterministic node and edge order.

mod common;
mod inspection_common;
use common::Scratch;
use inspection_common::{dform, golden};

#[test]
fn the_resource_dag_follows_refs() {
    let out = dform("examples/demo/stacks/dform.df env=prod", &["graph"]);
    assert!(
        out.contains(
            r#""net.subnet[\"network.main::private-us-test-1a\"]" -> "net.vpc[\"network.main::vpc\"]";"#
        ),
        "{out}"
    );
    assert_eq!(
        out,
        dform("examples/demo/stacks/dform.df env=prod", &["graph"])
    );
    golden("graph_dform_prod_resources", &out);
}

/// A small program with its own (empty) schema, so the prelude adds no
/// nodes: strata are clusters, the negative edge is dashed.
#[test]
fn the_partition_graph_dashes_negative_edges() {
    let s = Scratch::new("graph-strata");
    s.write("schema.df", "edition 2026\n");
    s.write(
        "p.df",
        "edition 2026\np(1)\np(2)\nr(2)\nq(x) where p(x), not r(x)\ns(x) where q(x)",
    );
    let run = |prog: &str| s.run(&["dev", "--provider", "schema.df", "graph", "--strata", prog]);
    let out = run("p.df").success().stdout;
    assert!(out.contains(r#""r" -> "q" [style=dashed];"#), "{out}");
    assert!(out.contains(r#""p" -> "q";"#), "{out}");
    assert!(out.contains(r#"label="stratum 1";"#), "{out}");
    golden("graph_strata_negation", &out);

    // A negative cycle: the graph still prints, and the command fails with
    // the cycle.
    s.write(
        "bad.df",
        "edition 2026\np(1)\na(x) where p(x), not b(x)\nb(x) where p(x), not a(x)",
    );
    let bad = run("bad.df").failure();
    assert!(
        bad.stdout.contains(r#""a" -> "b" [style=dashed];"#),
        "{}",
        bad.stdout
    );
    assert!(!bad.stdout.contains("cluster"), "{}", bad.stdout);
}

#[test]
fn any_binary_relation_is_a_graph() {
    let out = dform(
        "examples/demo/stacks/dform.df",
        &["graph", "--relation", "vpc_peer/2"],
    );
    assert_eq!(
        out,
        dform(
            "examples/demo/stacks/dform.df",
            &["graph", "--relation", "vpc_peer"]
        )
    );
    golden("graph_dform_vpc_peer", &out);

    let s = Scratch::new("graph-arity");
    s.write("p.df", "edition 2026\nt(\"a\", \"b\", \"c\")");
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "graph",
            "--relation",
            "t",
            "p.df",
        ])
        .failure();
    assert!(
        r.stderr.contains("t is not a binary relation"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "graph",
            "--relation",
            "t/3",
            "p.df",
        ])
        .failure();
    assert!(r.stderr.contains("arity 3, want 2"), "{}", r.stderr);
}
