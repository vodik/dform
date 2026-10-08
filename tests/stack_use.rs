//! A `use` of a stack binds its deployments' outputs, `platform[env].x`,
//! and nothing else (R-136): none of the used stack's rules, resources,
//! provider uses or reads, nor those of the modules it uses, run in the
//! stack that uses it, and `effects` lists none of them there. (A used
//! *module's* provider use and reads do run in its user: R-129,
//! tests/loader_in_module.rs.)

mod common;
use common::Scratch;

/// The platform: its own resource, and a module that configures a
/// provider and reads a file that is not there, which fails wherever it
/// runs.
const PLATFORM: &str = r#"
key env: enum("lab") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
use modules.k3s { host = edge.cidr }
output ip = edge.cidr
"#;

const K3S: &str = r#"
input host: string
use k8s { namespace = "edge" }
resource k8s.config_map "${d.metadata.name}" = d where {
  d in yaml.decode(io.read("vendor/kubeconfig.yml"))
}
"#;

const APPS: &str = r#"
key env: enum("lab") = "lab"
use fake
use stacks.platform
resource net.vpc rec { cidr = "10.1.0.0/16", name = platform[env].ip }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("modules/k3s.df", K3S);
    s.write("stacks/apps.df", APPS);
    s
}

#[test]
fn a_used_stacks_reads_run_in_it_not_in_its_user() {
    let s = project("stack-use-reads");
    // Where it belongs, the read runs (and fails: there is no file).
    let r = s.run(&["plan", "platform"]).failure();
    assert!(r.stderr.contains("vendor/kubeconfig.yml"), "{}", r.stderr);
    // The user reads the deployment's output, and runs none of it.
    let r = s.run(&["plan", "apps"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 create after platform[env=lab] is applied",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("waits on  stack platform[env=lab]"),
        "{}",
        r.stdout
    );
    for theirs in ["edge", "k8s", "config_map", "kubeconfig"] {
        assert!(!r.stdout.contains(theirs), "{theirs}:\n{}", r.stdout);
        assert!(!r.stderr.contains(theirs), "{theirs}:\n{}", r.stderr);
    }
}

#[test]
fn effects_lists_none_of_a_used_stacks_effects() {
    let s = project("stack-use-effects");
    let mine = s.run(&["dev", "effects", "stacks/apps.df"]).success();
    let theirs = s.run(&["dev", "effects", "stacks/platform.df"]).success();
    for row in [
        "k3s",
        "file:vendor/kubeconfig.yml",
        "k8s.config_map",
        "offers  ip",
    ] {
        assert!(theirs.stdout.contains(row), "{row}:\n{}", theirs.stdout);
        assert!(!mine.stdout.contains(row), "{row}:\n{}", mine.stdout);
    }
}
