//! A copy of a component takes its name from its clause, as a provider
//! type's resource does (R-191): `resource node "agent-${i}" { .. } where i
//! in 0..agents` is one copy per row, `agent-0`, `agent-1`, each with its
//! own resources under it (`agent-0.instance`), read `node["agent-0"].instance` and
//! `node[_].instance`. A row that goes is a delete of its copy.

mod common;
use common::Scratch;

/// The reviewer's shape: a node is an instance and its volume, which
/// references it; one node per agent.
const NODES: &str = r#"input agents: int = 2

use fake

component node {
  input index: int
  output cidr = instance.cidr
  resource net.vpc instance { cidr = "10.${index}.0.0/16" }
  resource net.subnet volume {
    cidr = "10.${index}.1.0/24"
    vpc = instance
  }
}

resource node "agent-${i}" { index = i } where i in 0..agents
"#;

fn nodes(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("main.df", NODES);
    s
}

/// Each row of the clause is a copy, its resources under its name, the
/// volume's reference to the instance its own copy's.
#[test]
fn each_row_of_the_clause_is_a_copy() {
    let s = nodes("copies-rows");
    let r = s.run(&["plan", "main.df"]).success();
    for i in 0..2 {
        let want = format!(
            "  + node agent-{i}\n    + net.vpc agent-{i}.instance   main.df:8\n        \
             cidr = \"10.{i}.0.0/16\"\n    + net.subnet agent-{i}.volume  main.df:9\n        \
             cidr = \"10.{i}.1.0/24\"\n        vpc = agent-{i}.instance\n"
        );
        assert!(r.stdout.contains(&want), "{want}\n---\n{}", r.stdout);
    }
    let r = s.run(&["plan", "main.df", "--set", "agents=3"]).success();
    assert_eq!(r.summary(), "plan: 6 changes (6 create) over 1 tick");
}

/// `node["agent-1"].x` is a resource of that copy, `node[_].x` each
/// copy's; `has` tests a path through them; outputs read as before.
#[test]
fn its_resources_read_through_the_component() {
    let s = nodes("copies-reads");
    s.write(
        "main.df",
        &format!(
            "{NODES}one(c) where c = node[\"agent-1\"].instance.cidr\n\
             each(v) where v = node[_].volume.vpc\n\
             held(1) where has node[_].instance.cidr\n\
             out(n, c) where n in node, c = n.cidr\n"
        ),
    );
    let q = |pattern: &str| s.run(&["query", pattern, "main.df"]).success().stdout;
    assert!(q("one(c)").contains("\"10.1.0.0/16\""), "{}", q("one(c)"));
    let each = q("each(v)");
    assert!(
        each.contains("net.vpc agent-0.instance\n") && each.contains("net.vpc agent-1.instance\n"),
        "{each}"
    );
    assert!(q("held(x)").contains('1'), "{}", q("held(x)"));
    let out = q("out(n, c)");
    assert!(
        out.contains("\"agent-0\"  net.vpc agent-0.instance.cidr"),
        "{out}"
    );
}

/// `why` on a resource of a copy names the copy and the row of its
/// clause; its input reads as `agent-1.index`.
#[test]
fn why_names_the_row_of_the_clause() {
    let s = nodes("copies-why");
    let r = s.run(&["why", "agent-1.volume", "main.df"]).success();
    assert!(
        r.stdout.starts_with(
            "net.subnet agent-1.volume  main.df:9\n  in node agent-1  main.df:15  with i = 1, \
             agents = 2\n"
        ),
        "{}",
        r.stdout
    );
    let r = s
        .run(&["why", "node[\"agent-1\"].index", "main.df"])
        .success();
    assert!(
        r.stdout.starts_with("input agent-1.index = 1\n"),
        "{}",
        r.stdout
    );
}

/// Why a copy's resource is not planned names the copy its row named,
/// never the header's template or the compiler's names: two rows that
/// name one copy and disagree on its input leave it unset.
#[test]
fn why_not_names_the_copy_of_the_row() {
    let s = nodes("copies-why-not");
    s.write(
        "main.df",
        &NODES.replace("\"agent-${i}\"", "\"agent-${i % 1}\""),
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stdout
            .contains("net.vpc agent-0.instance   main.df:8  input agent-0.index is not set\n"),
        "{}",
        r.stdout
    );
    let r = s
        .run(&["why", "net.vpc agent-0.instance", "main.df"])
        .success();
    assert!(
        r.stdout
            .contains("    index(index) (in agent-0): not derived\n")
            && !r.stdout.contains("__")
            && !r.stdout.contains("::"),
        "{}",
        r.stdout
    );
}

/// A row that goes is a delete of its copy: its resources, the volume
/// before the instance it references.
#[test]
fn a_row_removed_deletes_its_copy() {
    let s = nodes("copies-removed");
    s.run(&["dev", "--world", "w.json", "apply", "main.df"])
        .success();
    let r = s
        .run(&[
            "dev", "--world", "w.json", "apply", "main.df", "--set", "agents=1",
        ])
        .success();
    let gone = |a: &str| r.stdout.find(&format!("  - {a}  "));
    let (volume, instance) = (
        gone("net.subnet agent-1.volume"),
        gone("net.vpc agent-1.instance"),
    );
    assert!(
        r.stdout.contains("  - node agent-1\n")
            && volume.is_some()
            && instance.is_some()
            && volume < instance,
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("agent-0"), "{}", r.stdout);
}

/// A copy named by its clause inside another, or inside a copy with a
/// name: each scoped under the one that makes it.
#[test]
fn a_copy_inside_a_copy_composes() {
    let s = Scratch::project("copies-nested");
    s.write(
        "main.df",
        r#"
use fake

component disk {
  input size: int
  resource net.vpc d { cidr = "10.${size}.0.0/16" }
}

component node {
  input index: int
  resource disk "d${j}" { size = index * 10 + j } where j in 0..2
}

component pool {
  input n: int
  resource node "agent-${i}" { index = i } where i in 0..n
}

resource pool main { n = 2 }
resource pool "p${k}" { n = k } where k in 1..2
"#,
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(r.summary(), "plan: 6 changes (6 create)", "{}", r.stdout);
    for (pool, i, j, size) in [("main", 1, 1, 11), ("p1", 0, 1, 1)] {
        let want = format!(
            "+ net.vpc[\"{pool}.agent-{i}.d{j}.d\"]\n        cidr = \"10.{size}.0.0/16\"\n"
        );
        assert!(r.stdout.contains(&want), "{want}\n---\n{}", r.stdout);
    }
}

/// A secret input of a copy named by its clause is each copy's secret:
/// written where the schema keeps no secret, it is refused as any
/// copy's is.
#[test]
fn a_copys_secret_input_stays_secret() {
    let s = Scratch::project("copies-secret");
    let program = |leak: &str| {
        format!(
            "input agents: int = 2\n\nuse fake\n\ncomponent node {{\n  input pw: \
             secret(string)\n  {leak}\n}}\n\nresource node \"agent-${{i}}\" {{ pw = \
             \"hunter2\" }} where i in 0..agents\n"
        )
    };
    s.write("main.df", &program(""));
    let r = s
        .run(&["query", "attr(input, s, \"pw\", v)", "main.df"])
        .success();
    assert!(
        r.stdout.contains("\"agent-1\"  secret(7 B)") && !r.stdout.contains("hunter2"),
        "{}",
        r.stdout
    );
    s.write("main.df", &program("resource net.vpc leak { cidr = pw }"));
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("a secret reaches net.vpc .cidr"),
        "{}",
        r.stderr
    );
}
