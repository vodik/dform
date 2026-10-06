//! A read of another stack's output before that deployment is applied
//! (`platform[env].ingress_ip`) waits on it (R-121): the reader lists under
//! `later` as `waits on  stack platform[env=lab]`, its attributes as
//! written, and `why-not` says the deployment has not been applied. Once it
//! is, the reader plans with the value; a deployment that has published
//! and lacks the output is no wait.

mod common;
use common::Scratch;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
output ingress_ip = edge.cidr
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc rec {
  cidr = "10.1.0.0/16"
  name = platform[env].ingress_ip
}
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s
}

#[test]
fn a_read_of_a_deployment_not_applied_waits_on_it() {
    let s = project("cross-wait");
    let r = s.run(&["plan", "apps"]).success();
    assert_eq!(r.summary(), "plan: 0 changes, 1 later", "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "later   changes this plan cannot count yet\n  \
             waits on  stack platform[env=lab]  which this plan does not resolve\n  \
             + net.vpc rec  "
        ) && r
            .stdout
            .contains("      cidr = \"10.1.0.0/16\"\n      name = platform[env=lab].ingress_ip\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["why-not", "net.vpc rec", "apps"]).success();
    assert!(
        r.stdout
            .contains("waits on stack platform[env=lab], which has not been applied"),
        "{}",
        r.stdout
    );

    // Applied, the reader plans with its value.
    s.run(&["apply", "platform"]).success();
    let r = s.run(&["plan", "apps"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("name = \"10.0.0.0/16\""), "{}", r.stdout);
}

/// `apply apps` applies the deployment it reads first (R-30), and the
/// reader then applies with its value.
#[test]
fn apply_applies_the_deployment_it_waits_on_first() {
    let s = project("cross-wait-apply");
    let r = s.run(&["apply", "apps"]).success();
    assert!(
        r.stdout.contains("== platform[env=lab]") && r.stdout.contains("name = \"10.0.0.0/16\""),
        "{}",
        r.stdout
    );
}

/// A deployment that has published, without the output the reader reads
/// (applied before the output was added), is not waited on: the read finds
/// no row, and the reader is not planned, saying so.
#[test]
fn a_published_deployment_without_the_output_is_no_wait() {
    let s = project("cross-wait-published");
    s.write(
        "stacks/platform.df",
        &PLATFORM.replace(
            "output ingress_ip = edge.cidr\n",
            "output other = edge.cidr\n",
        ),
    );
    s.run(&["apply", "platform"]).success();
    s.write("stacks/platform.df", PLATFORM);
    let r = s.run(&["plan", "apps"]).success();
    assert!(!r.stdout.contains("waits on  stack"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("not planned   statements that derive no resource\n  net.vpc rec  "),
        "{}",
        r.stdout
    );
}
