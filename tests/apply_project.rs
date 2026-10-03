//! `dform apply` with no target in a project of several stacks (R-30
//! leftover): every stack, in dependency order, each a run of its own with
//! its own plan, confirmation and state.

mod common;
use common::Scratch;

const NET: &str = r#"edition 2026
provider fake
resource net.vpc main { cidr = "10.0.0.0/16" }
output cidr = main.cidr
"#;

const APP: &str = r#"edition 2026
provider fake
use stacks.net as network
resource net.subnet a {
  cidr = c
} where c = network.cidr
"#;

const SOLO: &str = r#"edition 2026
provider fake
resource net.vpc solo { cidr = "10.9.0.0/16" }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    // `app` sorts before `net`, which it reads.
    s.write("stacks/app.df", APP);
    s.write("stacks/net.df", NET);
    s.write("stacks/solo.df", SOLO);
    s
}

#[test]
fn apply_with_no_target_applies_every_stack_in_dependency_order() {
    let s = project("apply-project");
    let r = s.run(&["apply"]).success();
    assert!(
        r.stdout.starts_with(
            "apply: the project's 3 stacks in dependency order, each with its own plan, state \
             and confirmation: net, then app, then solo\n== net\n"
        ),
        "{}",
        r.stdout
    );
    for (stack, at) in [("net", "== net\n"), ("app", "== app\n"), ("solo", "== solo\n")] {
        assert!(r.stdout.contains(at), "{stack}: {}", r.stdout);
        assert!(
            s.path(&format!("dform.state/{stack}/state.json")).exists(),
            "{stack}"
        );
    }
    let app = s.read("dform.state/app/remote.json");
    assert!(app.contains("10.0.0.0/16"), "{app}");

    // Each run confirms on its own: without --yes, a declined first stack
    // stops the run before the others.
    let s = project("apply-project-confirm");
    let out = common::dform()
        .arg("apply")
        .current_dir(&s.dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!s.path("dform.state/app/state.json").exists());

    // A --set no stack declares is an error naming it.
    let r = s.run(&["apply", "--set", "nope=1"]).failure();
    assert!(
        r.stderr
            .contains("--set nope=1: no stack of the project declares input nope"),
        "{}",
        r.stderr
    );
}
