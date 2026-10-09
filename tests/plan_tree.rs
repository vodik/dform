//! The plan is the path tree printed (R-200): a thing's name is its path
//! from the project root, and lines that share a prefix group under one
//! header at every level (a deployment, a used module's instance, a copy
//! of a component), each line keeping its full name; a short name is legal
//! where it is unique.

mod common;
use common::Scratch;

const K3S: &str = r#"
component node {
  input index: int
  resource net.vpc vm { cidr = "10.${index}.0.0/16" }
}
resource node "agent-${i}" { index = i } where i in [0, 1]
resource net.subnet sub { cidr = "10.9.0.0/24" }
"#;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use k3s
resource net.vpc edge { cidr = "10.0.0.0/16" }
output cidr = k3s.sub.cidr
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc rec { cidr = "10.1.0.0/16", name = platform[env].cidr }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("k3s.df", K3S);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s.write(
        "project.df",
        "resource stacks.platform lab { env = \"lab\" }\nresource stacks.apps lab { env = \"lab\" }\n",
    );
    s
}

/// Three levels under a deployment: the module's instance, its copies,
/// their resources, each by its full name.
#[test]
fn a_deployment_a_module_and_a_copy_nest() {
    let s = project("tree-levels");
    let r = s.run(&["plan"]).success();
    assert!(
        r.stdout.starts_with(
            "plan: 5 changes (5 create)\n\n\
             + stacks.platform[env=lab]  project.df:1  never applied, 4 changes (4 create) over 1 tick\n  \
             tick 1  4 changes\n    \
             + module k3s\n      \
             + net.subnet k3s.sub        k3s.df:7\n          \
             cidr = \"10.9.0.0/24\"\n      \
             + k3s.node k3s.agent-0\n        \
             + net.vpc k3s.agent-0.vm  k3s.df:4\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("\n    + net.vpc edge                stacks/platform.df:5\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "\n+ stacks.apps[env=lab]      project.df:2  never applied, 1 change (1 create) over 1 \
             tick  after stacks.platform[env=lab]\n  tick 1  1 change\n    + net.vpc rec  \
             stacks/apps.df:5\n        cidr = \"10.1.0.0/16\"\n        name = \"10.9.0.0/24\"\n"
        ),
        "{}",
        r.stdout
    );
}

/// `why` takes a resource by its full name, or by a short one where that
/// is unique; a short name several end in is an error naming each.
#[test]
fn why_takes_a_short_name_where_it_is_unique() {
    let s = project("tree-why");
    let full = s.run(&["why", "net.subnet k3s.sub", "platform"]).success();
    assert!(
        full.stdout.starts_with("net.subnet k3s.sub  k3s.df:7\n"),
        "{}",
        full.stdout
    );
    for short in ["sub", "net.subnet sub", "k3s.sub"] {
        let r = s.run(&["why", short, "platform"]).success();
        assert_eq!(r.stdout, full.stdout, "{short}");
    }
    let r = s.run(&["why", "vm", "platform"]).failure();
    assert!(
        r.stderr.contains(
            "why vm: vm is the short name of net.vpc k3s.agent-0.vm and net.vpc \
             k3s.agent-1.vm; name one by its full name"
        ),
        "{}",
        r.stderr
    );
    // A value the stack declares is what its name reads.
    let r = s.run(&["why", "env", "platform"]).success();
    assert!(r.stdout.starts_with("key env = \"lab\""), "{}", r.stdout);
}

/// `moved` takes the old address by a short name where it is unique
/// (`"sub"` for state's `k3s.sub`), as `why` does; one several objects of
/// the type end in is an error naming each.
#[test]
fn moved_takes_a_short_old_name_where_it_is_unique() {
    let s = project("tree-moved");
    s.run(&["apply", "platform", "env=lab", "--yes"]).success();
    s.write(
        "k3s.df",
        &K3S.replace("net.subnet sub ", "net.subnet subnet "),
    );
    s.write(
        "stacks/platform.df",
        &PLATFORM.replace(
            "output cidr = k3s.sub.cidr",
            "moved(net.subnet, \"sub\", k3s.subnet)\noutput cidr = k3s.subnet.cidr",
        ),
    );
    let r = s.run(&["plan", "platform", "env=lab"]).success();
    assert!(
        r.stdout
            .contains("moved net.subnet[\"k3s.sub\"] -> net.subnet[\"k3s.subnet\"]\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("up to date"), "{}", r.stdout);
    s.write("k3s.df", K3S);
    s.write(
        "stacks/platform.df",
        &PLATFORM.replace(
            "output cidr",
            "resource net.vpc node { cidr = \"10.5.0.0/16\" }\nmoved(net.vpc, \"vm\", node)\noutput cidr",
        ),
    );
    let r = s.run(&["plan", "platform", "env=lab"]).failure();
    assert!(
        r.stderr.contains(
            "moved(net.vpc, \"vm\", ..): vm is the short name of net.vpc k3s.agent-0.vm and \
             net.vpc k3s.agent-1.vm; name one by its full name"
        ),
        "{}",
        r.stderr
    );
}

/// `why` resolves a used module's value by its short name (`why region`
/// for `k3s.region`) as it does a resource's; one several scopes declare
/// is an error naming each.
#[test]
fn why_takes_a_module_value_s_short_name() {
    let s = project("tree-why-value");
    s.write("k3s.df", &format!("{K3S}let region = \"bhs5\"\n"));
    let r = s.run(&["why", "region", "platform"]).success();
    assert!(
        r.stdout.starts_with("let k3s.region = \"bhs5\""),
        "{}",
        r.stdout
    );
    let r = s.run(&["why", "index", "platform"]).failure();
    assert!(
        r.stderr.contains(
            "why index: index is the short name of k3s.agent-0.index and k3s.agent-1.index"
        ),
        "{}",
        r.stderr
    );
}
