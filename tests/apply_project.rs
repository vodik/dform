//! `dform apply` with no target in a project of several stacks (R-30
//! leftover): every stack, one apply whose ticks are the tree's, each its
//! own plan, state and audit log. What one stack makes that another reads
//! is made a tick before the reader's change; nothing else orders them.

use crate::common;
use common::Scratch;
use std::collections::{BTreeMap, BTreeSet};

const NET: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
output cidr = main.cidr
output vpc = ref(main)
"#;

/// A subnet named for the vpc `net` makes: known after net's tick 1.
const APP: &str = r#"
use fake
use stacks.net as network
resource net.subnet a {
  cidr = c
  name = network.vpc
} where c = network.cidr
"#;

const SOLO: &str = r#"
use fake
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

/// The ticks each stack's audit log says its creates were made at, by
/// address.
fn made_at(s: &Scratch, stack: &str) -> BTreeMap<String, u64> {
    s.read(&format!("dform.state/stacks.{stack}/state.audit.jsonl"))
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "action" && e["result"] == "ok")
        .map(|e| {
            let addr = e["address"].as_str().unwrap().to_string();
            (addr, e["tick"].as_u64().unwrap())
        })
        .collect()
}

#[test]
fn apply_with_no_target_applies_every_stack_in_dependency_order() {
    let s = project("apply-project");
    let r = s.run(&["apply"]).success();
    // The stacks the line names, as a set, net before app, which reads it.
    let line = r.stdout.lines().next().unwrap_or_default();
    let named: Vec<&str> = line
        .strip_prefix("stacks: the project's 3, ")
        .and_then(|l| l.strip_suffix(": one apply, its ticks the tree's"))
        .unwrap_or_default()
        .split(", ")
        .collect();
    let set: BTreeSet<&str> = named.iter().copied().collect();
    assert_eq!(
        set,
        BTreeSet::from(["stacks.app", "stacks.net", "stacks.solo"]),
        "{}",
        r.stdout
    );
    let at = |name: &str| named.iter().position(|n| *n == name);
    assert!(at("stacks.net") < at("stacks.app"), "{line}");
    for stack in ["net", "app", "solo"] {
        assert!(
            r.stdout.contains(&format!("== stacks.{stack}\n")),
            "{stack}: {}",
            r.stdout
        );
        assert!(
            s.path(&format!("dform.state/stacks.{stack}/state.json"))
                .exists(),
            "{stack}"
        );
    }
    // Tick 1 makes what reads nothing not yet known, of every stack;
    // app's subnet, named for net's vpc, the tick after it.
    let (net, app, solo) = (made_at(&s, "net"), made_at(&s, "app"), made_at(&s, "solo"));
    assert_eq!(net.get("net.vpc[\"main\"]"), Some(&1), "{net:?}");
    assert_eq!(solo.get("net.vpc[\"solo\"]"), Some(&1), "{solo:?}");
    assert_eq!(app.get("net.subnet[\"a\"]"), Some(&2), "{app:?}");
    let app = s.read("dform.state/stacks.app/remote.json");
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
    assert!(!s.path("dform.state/stacks.app/state.json").exists());

    // A --set no stack declares is an error naming it.
    let r = s.run(&["apply", "--set", "nope=1"]).failure();
    assert!(
        r.stderr
            .contains("--set nope=1: no stack of the project declares input nope"),
        "{}",
        r.stderr
    );
}

/// A deployment that publishes nothing (app) is applied all the same:
/// the tree's plan after the apply says it is up to date, never `never
/// applied`.
#[test]
fn a_deployment_with_no_outputs_is_up_to_date_after_its_apply() {
    let s = project("apply-project-unpublished");
    s.run(&["apply", "--yes"]).success();
    let r = s.run(&["plan", "app"]).success();
    for stack in ["app", "net"] {
        assert!(
            r.stdout
                .lines()
                .any(|l| l.starts_with(&format!("= stacks.{stack} ")) && l.ends_with("up to date")),
            "{stack}: {}",
            r.stdout
        );
    }
}

/// A world fixture in a tree (`dev --world W`): each deployment has its
/// own world beside W, named for it, and its own state and lease beside
/// that, as a tree's deployments have without one; the project's apply
/// and the project module's alike. One shared file had every deployment
/// lease one state.
#[test]
fn a_world_fixture_in_a_tree_is_each_deployments_own() {
    let world = |s: &Scratch, full: &str| -> BTreeSet<String> {
        let w = s.json(&format!("w.{full}.json"));
        w["resources"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let module = project("apply-project-world-module");
    module.write(
        "project.df",
        "resource stacks.net net {}\nresource stacks.app app {}\nresource stacks.solo solo {}\n",
    );
    for s in [project("apply-project-world"), module] {
        s.run(&["dev", "--world", "w.json", "apply", "--yes"])
            .success();
        assert_eq!(
            world(&s, "stacks.net"),
            BTreeSet::from(["net.vpc::main".into()])
        );
        assert_eq!(
            world(&s, "stacks.solo"),
            BTreeSet::from(["net.vpc::solo".into()])
        );
        assert_eq!(world(&s, "stacks.app").len(), 1, "app's subnet");
        for full in ["stacks.net", "stacks.app", "stacks.solo"] {
            let st = s.json(&format!("w.{full}.state.json"));
            assert_eq!(st["resources"].as_object().unwrap().len(), 1, "{full}");
        }
        // The next apply finds each deployment's objects where it left them.
        let r = s
            .run(&["dev", "--world", "w.json", "apply", "--yes"])
            .success();
        assert!(!r.stdout.contains("+ net."), "{}", r.stdout);
    }
}
