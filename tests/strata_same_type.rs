//! A resource may depend on another resource of its own type (R-107): a
//! server and the agents that join it by its address. The partition graph
//! splits the type by address when the graph by (type, path) has a
//! negative cycle through its resources, so an agent's `want` reads the
//! server's attribute only; a true cycle (a resource reading itself, two
//! reading each other) is still an error, and names the addresses.

mod common;
use common::Scratch;

/// The k3s shape of ~/src/ovh-infra: a module with one server and
/// `agents` agents of the same type, each agent's join line reading the
/// server's computed endpoint.
const K3S: &str = r#"
input name: string
input agents: int = 0
output ip = server.endpoint

resource db.postgres server { name = "${name}-server" }
resource db.postgres "${name}-agent-${i}" {
  name = "${name}-agent-${i}"
  join = "https://${server.endpoint}:6443"
} where i in 0..agents
"#;

fn project(stack: &str) -> Scratch {
    let s = Scratch::project("strata-same-type");
    s.write("k3s.df", K3S);
    s.write("stacks/k3s.df", stack);
    s
}

#[test]
fn agents_read_their_servers_address() {
    let s = project("use fake\nuse k3s { name = \"lab\", agents = 2 }\n");
    let r = s.run(&["plan", "k3s"]).success();
    assert_eq!(r.summary(), "plan: 3 changes (3 create) over 2 ticks");
    assert!(
        r.stdout.contains("waits on  k3s.server.endpoint"),
        "{}",
        r.stdout
    );
    s.run(&["apply", "k3s"]).success();
    let world = s.read("dform.state/k3s/remote.json");
    assert_eq!(
        world.matches("https://k3s.server.db.fake:6443").count(),
        2,
        "{world}"
    );
    let r = s.run(&["plan", "k3s"]).success();
    assert_eq!(r.summary(), "stack k3s is up to date", "{}", r.stdout);
}

/// The strata name the server's and the agents' nodes apart: the agents'
/// `want` sits above the server's endpoint.
#[test]
fn the_strata_name_the_addresses() {
    let s = project("use fake\nuse k3s { name = \"lab\", agents = 2 }\n");
    let r = s.run(&["dev", "strata", "k3s"]).success();
    let at = |node: &str| -> usize {
        let line = r
            .stdout
            .lines()
            .find(|l| l.split_once(' ').is_some_and(|(_, n)| n.trim() == node))
            .unwrap_or_else(|| panic!("no node {node}:\n{}", r.stdout));
        line.split_whitespace().next().unwrap().parse().unwrap()
    };
    let server = at("(attr, db.postgres[\"k3s.server\"], endpoint)");
    // An agent's name is a segment, quoted should a gap hold a dot (R-112).
    let agents = at(
        r#"(want, db.postgres["k3s.*-agent-*"|"*-agent-*"|"k3s.\"*-agent-*\""|"\"*-agent-*\""])"#,
    );
    assert!(server < agents, "{server} >= {agents}\n{}", r.stdout);
}

/// Without a module: a literal server and agents named by a format.
#[test]
fn a_stacks_own_agents_read_its_server() {
    let s = Scratch::project("strata-same-type-stack");
    s.write(
        "stacks/k3s.df",
        r#"
use fake
resource db.postgres server { name = "server" }
resource db.postgres "agent-${i}" { join = "${server.endpoint}:6443" } where i in 0..2
"#,
    );
    let r = s.run(&["plan", "k3s"]).success();
    assert_eq!(r.summary(), "plan: 3 changes (3 create) over 2 ticks");
}

#[test]
fn a_resource_reading_itself_is_a_cycle_through_its_address() {
    let s = Scratch::project("strata-self");
    s.write(
        "stacks/k3s.df",
        "use fake\nresource db.postgres a { x = \"${a.endpoint}\" }\n",
    );
    let r = s.run(&["plan", "k3s"]).failure();
    assert!(
        r.stderr.contains(
            "negative cycle through (arg, db.postgres[\"a\"], endpoint), \
             (attr, db.postgres[\"a\"], endpoint), (want, db.postgres[\"a\"])"
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn two_resources_reading_each_other_are_a_cycle_through_both() {
    let s = Scratch::project("strata-pair");
    s.write(
        "stacks/k3s.df",
        r#"
use fake
resource db.postgres a { x = "${b.endpoint}" }
resource db.postgres b { x = "${a.endpoint}" }
"#,
    );
    let r = s.run(&["plan", "k3s"]).failure();
    for edge in [
        "(attr, db.postgres[\"b\"], endpoint) -> (want, db.postgres[\"a\"])",
        "(attr, db.postgres[\"a\"], endpoint) -> (want, db.postgres[\"b\"])",
    ] {
        assert!(r.stderr.contains(edge), "{edge}\n{}", r.stderr);
    }
}
