//! `stack list` over a backend that names the key (R-29 leftover): the
//! deployments are found under the backend's directory by its template,
//! whether or not the registry knows them.

mod common;
use common::Scratch;

const APP: &str = r#"
key env: enum("staging", "prod") = "staging"
provider fake
resource net.vpc main { cidr = "10.0.0.0/16", tags = { env } }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\nname = \"t\"\n",
    );
    s.write("stacks/app.df", APP);
    s
}

/// A backend that names the key: `stack list` finds every deployment under
/// its directory, also those the registry does not know.
#[test]
fn stack_list_finds_the_deployments_a_keyed_backend_holds() {
    let s = project("stack-list-keyed");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.app]\nbackend = 'local(\"state/{stack}-{env}\")'\n",
    );
    s.run(&["apply", "app", "env=prod"]).success();
    s.run(&["apply", "app", "env=staging"]).success();
    // Unregistered: found under the backend's directory alone.
    std::fs::remove_file(s.path("dform.state/stacks.json")).unwrap();
    let r = s.run(&["stack", "list"]).success();
    for d in ["app[env=prod]", "app[env=staging]"] {
        assert!(
            r.stdout.lines().any(|l| l.contains(d) && l.contains(" ok")),
            "{d}: {}",
            r.stdout
        );
    }
}
