//! The dependency graph keys an input's, a `let`'s and an output's cell
//! by the scope it belongs to (R-209), as the rule's text fixes it: the
//! copies a clause names (`resource node "agent-${i}"`) in the module
//! `k3s` are `k3s.agent-*`, never every scope, so a copy's `name` that
//! shadows its module's, or an output named as its user's, is no cycle.
//! On the mock.

mod common;
use common::Scratch;

/// The plan's lines `ATTR = ...`, in order.
fn values<'a>(stdout: &'a str, attr: &str) -> Vec<&'a str> {
    let prefix = format!("{attr} = ");
    stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .collect()
}

/// The reviewer's shape: a component's `input name` and `let tag` inside
/// a module with its own, copied by name and by a clause, the copy's
/// `name` given from the module's; a bare read is the copy's own,
/// `super.x` and the module's path read the module's, and nothing warns.
#[test]
fn a_copys_input_shadowing_its_modules_plans_the_inner_value() {
    let s = Scratch::project("scoped-shadow");
    s.write(
        "k3s.df",
        r#"input name: string
input agents: int = 2
let tag = "module"
component node {
  input name: string
  let tag = "copy"
  resource db.postgres db { name = "${name}-server" }
  resource net.vpc net { cidr = "${super.name} ${k3s.name} ${tag} ${super.tag}" }
}
resource node server { name = "${name}-s" }
resource node "agent-${i}" { name = "${name}-${i}" } where i in 0..agents
"#,
    );
    s.write("main.df", "use fake\nuse k3s { name = \"outer\" }\n");
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        values(&r.stdout, "name"),
        [
            "\"outer-0-server\"",
            "\"outer-1-server\"",
            "\"outer-s-server\""
        ],
        "{}",
        r.stdout
    );
    assert_eq!(
        values(&r.stdout, "cidr"),
        ["\"outer outer copy module\""; 3],
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("shadows"), "{}", r.stderr);
}

/// The private project's platform: the stack's key given to three
/// modules as their own `env`, two of which take config's `env` in turn;
/// the module's copies named by a clause; an output `ip` of the stack, of
/// the module and of each copy. Each scope's `env` and `ip` is its own.
#[test]
fn a_key_given_to_modules_that_use_each_other_is_no_cycle() {
    let s = Scratch::project("scoped-key");
    s.write(
        "config.df",
        "input env: enum(\"lab\", \"prod\")\nlet suffix: string = \"-${env}\"\n",
    );
    s.write(
        "policy.df",
        "input env: enum(\"lab\", \"prod\")\nuse config { env }\n\
         warn \"prod\" where env == \"prod\", config.suffix == \"-prod\"\n",
    );
    s.write(
        "k3s.df",
        r#"input env: enum("lab", "prod")
input name: string
input agents: int = 0
input disk: string = "50Gi"
use config { env }
output ip: string = server.ip
component node {
  input hostname: string
  output ip: string = vm.name
  resource compute.vm vm { name = hostname }
  resource db.postgres data { name = "${hostname}-${disk}${config.suffix}" }
}
resource node server { hostname = "${name}-server" }
resource node "agent-${i}" { hostname = "${name}-agent-${i}" } where i in 0..agents
"#,
    );
    s.write(
        "stacks/platform.df",
        r#"key env: enum("lab", "prod") = "lab"
input agents: int = 0
input disk: string = "50Gi"
output ip: string = k3s.ip
set { agents = 1, disk = "250Gi" } where env == "prod"
use fake
use config { env }
use policy { env }
use k3s { env, name = "k8s-${env}", agents, disk }
"#,
    );
    let r = s
        .run(&["plan", "--why=none", "platform", "env=prod"])
        .success();
    assert_eq!(
        values(&r.stdout, "name"),
        [
            "\"k8s-prod-agent-0\"",
            "\"k8s-prod-agent-0-250Gi-prod\"",
            "\"k8s-prod-server\"",
            "\"k8s-prod-server-250Gi-prod\""
        ],
        "{}",
        r.stdout
    );
}

/// The private project's apps: a module given the stack's key and a
/// resource of the provider together, the provider configured from
/// another deployment's output under that key. The module's `env` is
/// not the stack's, so the provider's configuration reads no resource it
/// serves.
#[test]
fn a_module_given_the_key_and_a_resource_is_no_provider_cycle() {
    let s = Scratch::project("scoped-provider");
    s.write(
        "config.df",
        "input env: enum(\"lab\", \"prod\")\nlet suffix: string = \"-${env}\"\n",
    );
    s.write(
        "m.df",
        r#"input env: enum("lab", "prod")
input vpc: ref(net.vpc)
use config { env }
resource db.postgres d { name = "${env}-${vpc.cidr}${config.suffix}" }
"#,
    );
    s.write(
        "stacks/platform.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\noutput zone: string = \"z-${env}\"\nuse fake\n",
    );
    s.write(
        "stacks/app.df",
        r#"key env: enum("lab", "prod") = "lab"
use config { env }
use stacks.platform
use fake { zone = platform[env].zone }
resource net.vpc vpc { cidr = "10.0.0.0/16" }
use m { env, vpc }
"#,
    );
    let r = s.run(&["plan", "--why=none", "stacks/app.df"]).success();
    assert!(
        r.stdout
            .contains("pending on ?provider fake  zone = platform[env].zone"),
        "{}",
        r.stdout
    );
}

/// A real cycle through a copy's input names each cell by its scope, as
/// the program names it.
#[test]
fn a_cycle_through_a_copys_input_names_its_scope() {
    let s = Scratch::project("scoped-cycle");
    s.write(
        "k3s.df",
        r#"input agents: int = 1
component node {
  input hostname: string
  output name: string = hostname
}
resource node "agent-${i}" { hostname = "h" } where i in 0..agents
resource node "agent-${i}" { hostname = n } where i in 0..agents, output(_, "name", n)
"#,
    );
    s.write("main.df", "use fake\nuse k3s\n");
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("input k3s.agent-*.hostname"),
        "{}",
        r.stderr
    );
}

/// A scope that reads its copies' output `ip` (`node[_].ip`) and has an
/// output `ip` of its own: the copies a variable ranges over are the
/// copies the scope makes, by name or by a clause, never the scope
/// itself. In the stack and in a module.
#[test]
fn a_scopes_output_read_from_its_copies_outputs_is_no_cycle() {
    let s = Scratch::project("scoped-copies");
    let node = "component node {\n  input n: string\n  output ip: string = \"ip-${n}\"\n}\n";
    s.write(
        "k3s.df",
        &format!(
            "{node}resource node \"agent-${{i}}\" {{ n = \"${{i}}\" }} where i in 0..2\n\
             let ips = collect_list(x) where x = node[_].ip\n\
             output ip: string = list.join(ips, \",\")\n"
        ),
    );
    s.write(
        "main.df",
        &format!(
            "use fake\nuse k3s\n{node}resource node a {{ n = \"a\" }}\n\
             let ips = collect_list(x) where x = node[_].ip\n\
             output ip: string = list.join(ips, \",\")\n\
             resource net.vpc v {{ cidr = \"${{list.join(ips, \",\")}} ${{k3s.ip}}\" }}\n"
        ),
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        values(&r.stdout, "cidr"),
        ["\"ip-a ip-0,ip-1\""],
        "{}",
        r.stdout
    );
}

/// A copy by name given an output of a copy a clause names: inside the
/// clause's copies their own input is theirs (`k3s.agent-*`, read off
/// the copy's gate), never the named copy's, so the server's `hostname`
/// from `agent-0`'s `ip` is no cycle through its own.
#[test]
fn a_named_copy_given_a_clause_copys_output_is_no_cycle() {
    let s = Scratch::project("scoped-gate");
    s.write(
        "k3s.df",
        r#"component node {
  input hostname: string
  output ip: string = "ip-${hostname}"
  resource db.postgres db { name = hostname }
}
resource node "agent-${i}" { hostname = "a${i}" } where i in 0..2
resource node server { hostname = "s-${node["agent-0"].ip}" }
"#,
    );
    s.write("main.df", "use fake\nuse k3s\n");
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        values(&r.stdout, "name"),
        ["\"a0\"", "\"a1\"", "\"s-ip-a0\""],
        "{}",
        r.stdout
    );
}
