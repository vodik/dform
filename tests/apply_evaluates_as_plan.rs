//! An apply's evaluation is its plan's: a tree's apply plans its
//! deployments as `plan` does (`matrix::Tree`, the board), each under its
//! `deployment D` span with the plan's phases, its rounds as long, then
//! ticks.

use crate::common;
use common::{Scratch, traced};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// Platform, a network and its id as an output; apps, a subnet in it.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "stacks/platform.df",
        "use fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\noutput vpc = ref(main)\n\
         output region = \"bhs5\"\n",
    );
    s.write(
        "stacks/apps.df",
        "use fake\nuse stacks.platform\n\
         resource net.subnet a { vpc_id = platform.vpc, cidr = \"10.0.1.0/24\" }\n\
         resource net.subnet named { cidr = \"10.0.2.0/24\", name = platform.region }\n",
    );
    s
}

/// The spans of an apply's lease: its own, never a plan's.
const LEASE: [&str; 3] = ["lock taken", "lease renewed", "lease checked"];

/// The span `id`'s descendants, at any depth.
fn under<'a>(spans: &'a [Json], id: &Json) -> Vec<&'a Json> {
    let mut ids = vec![id.clone()];
    let mut out = Vec::new();
    while let Some(id) = ids.pop() {
        for x in spans.iter().filter(|x| x["parentSpanId"] == id) {
            ids.push(x["spanId"].clone());
            out.push(x);
        }
    }
    out
}

/// A span's length, in ns.
fn length(x: &Json) -> u128 {
    let at = |k: &str| x[k].as_str().unwrap().parse::<u128>().unwrap();
    at("endTimeUnixNano") - at("startTimeUnixNano")
}

/// Each deployment's spans by name, counted, and how long its
/// evaluation's rounds took. Left out: a wait, whose length is the
/// other's pace; the last apply's program evaluated to explain the
/// plan's text, which a plan prints under its deployment and an apply at
/// its tick 1; and the apply's own bookkeeping, its lease.
fn phases(spans: &[Json]) -> BTreeMap<String, (BTreeMap<String, usize>, u128)> {
    spans
        .iter()
        .filter_map(|d| {
            let name = d["name"].as_str()?.strip_prefix("deployment ")?;
            let mut names = BTreeMap::new();
            let mut rounds = 0;
            for x in under(spans, &d["spanId"]) {
                let n = x["name"].as_str().unwrap_or_default();
                if n.starts_with("waits on ")
                    || n.contains("last apply's program")
                    || LEASE.contains(&n)
                {
                    continue;
                }
                if n.starts_with("evaluation, round ") {
                    rounds += length(x);
                }
                *names.entry(n.to_string()).or_insert(0) += 1;
            }
            Some((name.to_string(), (names, rounds)))
        })
        .collect()
}

/// `apply apps` traces each deployment as `plan apps` does: the same
/// phases under `deployment D`, its rounds about as long; the apply's
/// ticks are not under it.
#[test]
fn a_trees_apply_plans_each_deployment_as_its_plan_does() {
    let s = project("apply-as-plan");
    let (plan, planned) = traced(&s, &["plan", "apps"]);
    plan.success();
    let (apply, applied) = traced(&s, &["apply", "apps", "--yes"]);
    apply.success();
    let (planned, applied) = (phases(&planned), phases(&applied));
    assert_eq!(
        planned.keys().collect::<Vec<_>>(),
        ["stacks.apps", "stacks.platform"],
        "{planned:?}"
    );
    for (d, (names, rounds)) in &planned {
        let (a_names, a_rounds) = &applied[d];
        assert_eq!(a_names, names, "{d}");
        assert!(names.contains_key("evaluated"), "{d}: {names:?}");
        assert!(!a_names.contains_key("tick 1"), "{d}: {a_names:?}");
        // Within noise: the same rounds, on a loaded machine.
        assert!(
            *a_rounds < rounds * 4 + 200_000_000,
            "{d}: rounds of {a_rounds} ns in the apply, {rounds} ns in the plan"
        );
    }
}
