//! A tree's tick is the tree's: every deployment's calls whose
//! dependencies are met are in flight together, under one window, each
//! in its deployment's session of the one provider process, each answer
//! written to its own deployment's state.

use crate::common;
use common::{Scratch, attr, traced};
use serde_json::Value as Json;
use std::collections::BTreeSet;

/// How long each vpc's Apply is slowed (chaos `delay`): long enough that
/// two made one after the other cannot overlap.
const DELAY_MS: u64 = 1500;

/// Two deployments that read nothing of each other, each a vpc `main`
/// and a vpc `peer`, and a subnet in `main`.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    for (stack, at) in [("east", "10.0"), ("west", "10.1")] {
        s.write(
            &format!("stacks/{stack}.df"),
            &format!(
                "use fake\n\
                 resource net.vpc main {{ cidr = \"{at}.0.0/16\" }}\n\
                 resource net.vpc peer {{ cidr = \"{at}.128.0/17\" }}\n\
                 resource net.subnet a {{ vpc_id = ref(main), cidr = \"{at}.1.0/24\" }}\n"
            ),
        );
    }
    s
}

/// A deployment's state, as its apply left it.
fn state(s: &Scratch, stack: &str) -> Json {
    s.json(&format!("dform.state/stacks.{stack}/state.json"))
}

/// The keys of an object of state (`resources`, `uncertain`, ..).
fn keys(v: &Json) -> BTreeSet<String> {
    v.as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The objects a deployment's world holds (none before its world is
/// first written).
fn world(s: &Scratch, stack: &str) -> BTreeSet<String> {
    let path = format!("dform.state/stacks.{stack}/remote.json");
    match s.path(&path).exists() {
        true => keys(&s.json(&path)["resources"]),
        false => BTreeSet::new(),
    }
}

/// When an Apply span started and ended, in its deployment's session.
fn applies(spans: &[Json], addr: &str) -> Vec<(String, u128, u128)> {
    let at = |x: &Json, k: &str| x[k].as_str().unwrap().parse::<u128>().unwrap();
    spans
        .iter()
        .filter(|x| {
            common::is_call(x, "fakecloud", "Apply") && attr(x, "addr").as_deref() == Some(addr)
        })
        .map(|x| {
            let session = attr(x, "session").unwrap_or_default();
            (
                session,
                at(x, "startTimeUnixNano"),
                at(x, "endTimeUnixNano"),
            )
        })
        .collect()
}

/// The two deployments' tick-1 creates are in flight together: each
/// vpc's Apply, slowed, overlaps the other deployment's, each in its own
/// session of one process; each lands in its own deployment's world,
/// and the subnet that reads its vpc follows it.
#[test]
fn independent_deployments_calls_overlap_and_each_lands_in_its_world() {
    let s = project("tree-tick-overlap");
    let delay = format!("delay=net.vpc[\"main\"]:{DELAY_MS}");
    let (r, spans) = traced(&s, &["dev", "--chaos", &delay, "apply", "--yes"]);
    r.success();
    let made = applies(&spans, "net.vpc main");
    assert_eq!(made.len(), 2, "{made:?}");
    let ((sa, a0, a1), (sb, b0, b1)) = (&made[0], &made[1]);
    assert_ne!(sa, sb, "a session each: {made:?}");
    // Of one process: an apply shares the tree's links as a plan does.
    let handshakes = spans
        .iter()
        .filter(|x| attr(x, "provider.call").as_deref() == Some("Handshake"))
        .count();
    assert_eq!(handshakes, 1, "one process for the tree");
    assert!(a0 < b1 && b0 < a1, "the two creates overlap: {made:?}");
    for stack in ["east", "west"] {
        assert_eq!(
            world(&s, stack),
            BTreeSet::from(["net.subnet::a", "net.vpc::main", "net.vpc::peer"].map(String::from)),
            "{stack}"
        );
    }
    // A subnet's Apply starts after its own deployment's vpc answered.
    for (session, start, _) in applies(&spans, "net.subnet a") {
        let (_, _, vpc) = made.iter().find(|(s, ..)| *s == session).unwrap();
        assert!(start >= *vpc, "session {session}: {made:?}");
    }
}

/// A tick's deployments that read nothing of each other are planned at
/// once, each on its own thread: two readers of net's vpc, known after
/// net's tick 1, plan their subnets at tick 2, each Plan call waiting
/// 1.5 s on a server and each to a process of its own (two's provider is
/// the mock at a path); the two are in flight together.
#[test]
fn a_ticks_deployments_are_planned_at_once() {
    let s = Scratch::project("tree-tick-plans");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nt = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-fake")
        ),
    );
    s.write(
        "stacks/net.df",
        "use fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\noutput vpc = ref(main)\n",
    );
    for (stack, provider) in [("one", "fake"), ("two", "t")] {
        s.write(
            &format!("stacks/{stack}.df"),
            &format!(
                "use {provider}\nuse stacks.net as network\n\
                 resource net.subnet a {{ cidr = \"10.0.1.0/24\", name = network.vpc }}\n"
            ),
        );
    }
    let slow = std::time::Duration::from_millis(1500);
    let at = format!("127.0.0.1:{}", common::answering_after(slow, slow));
    let out = common::dform()
        .args(["apply", "--yes", "--trace", "trace.jsonl"])
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .env("DFORM_TEST_FAKE_PLAN_AWAITS", &at)
        .env_remove("DFORM_TRACE")
        .output()
        .unwrap();
    common::Run::from(out).success();
    let spans = common::spans(&s.path("trace.jsonl"));
    let ns = |x: &Json, k: &str| x[k].as_str().unwrap().parse::<u128>().unwrap();
    let tick = spans
        .iter()
        .find(|x| x["name"] == "tick 2")
        .expect("a tick 2");
    let mut ids = vec![tick["spanId"].clone()];
    let mut plans = Vec::new();
    while let Some(id) = ids.pop() {
        for x in spans.iter().filter(|x| x["parentSpanId"] == id) {
            ids.push(x["spanId"].clone());
            if x["name"] == "plan net.subnet a" {
                plans.push((ns(x, "startTimeUnixNano"), ns(x, "endTimeUnixNano")));
            }
        }
    }
    let [(a0, a1), (b0, b1)] = plans[..] else {
        panic!("a subnet's Plan call each at tick 2: {plans:?}");
    };
    assert!(a0 < b1 && b0 < a1, "planned one after the other: {plans:?}");
}

/// The provider dies under the tree's tick (chaos `crash`): each
/// deployment's in-flight record holds its own creates whose answers were
/// lost, uncertain with their idempotency keys, and every object each
/// world has is its state's or uncertain; the next apply resumes both.
#[test]
fn a_crash_under_a_trees_tick_leaves_each_deployment_its_own_record() {
    let s = project("tree-tick-crash");
    let r = s.run(&[
        "dev",
        "--chaos",
        "crash=net.vpc[\"peer\"]",
        "apply",
        "--yes",
    ]);
    r.failure();
    let creates =
        BTreeSet::from(["net.vpc::main", "net.vpc::peer", "net.subnet::a"].map(String::from));
    for stack in ["east", "west"] {
        let st = state(&s, stack);
        let mapped = keys(&st["resources"]);
        let uncertain = keys(&st["uncertain"]);
        assert_eq!(uncertain, &creates - &mapped, "{stack}: {st}");
        assert!(!uncertain.is_empty(), "{stack}: {st}");
        assert_eq!(
            keys(&st["in_flight"]["remaining"]),
            uncertain,
            "{stack}: {st}"
        );
        for (a, u) in st["uncertain"].as_object().unwrap() {
            assert_eq!(u["op"], "create", "{stack} {a}: {st}");
        }
        assert!(world(&s, stack).is_subset(&creates), "{stack}");
    }
    s.run(&["apply", "--yes"]).success();
    for stack in ["east", "west"] {
        let st = state(&s, stack);
        assert_eq!(keys(&st["resources"]), creates, "{stack}: {st}");
        assert!(keys(&st["uncertain"]).is_empty(), "{stack}: {st}");
        assert!(st["in_flight"].is_null(), "{stack}: {st}");
        assert_eq!(world(&s, stack), creates, "{stack}");
    }
}

/// A reader's call that reads a secret its upstream holds reads it where
/// it is held, inside its Apply: in a tree's tick it waits for the
/// upstream's call that changes it (slowed, chaos `delay`), so a rotated
/// secret reaches the reader in the same apply.
#[test]
fn a_readers_call_waits_for_the_call_that_changes_the_secret_it_reads() {
    let s = Scratch::project("tree-tick-held");
    s.write(
        "stacks/prod.df",
        "input pw: secret(string)\n\
         use fake\n\
         resource leaky.vault v {\n  password = p\n} where pw(p)\n\
         output pass: secret(string) = v.password\n",
    );
    // App's `v` is there so that chaos may name `v` (it names only a
    // deployment's own resources): it does not change, and the call
    // slowed is prod's alone.
    s.write(
        "stacks/app.df",
        "use fake\nuse stacks.prod\n\
         resource leaky.vault v { backup = \"plain\" }\n\
         resource leaky.vault copy { backup = prod.pass }\n",
    );
    let schema = common::repo().join("tests/fixtures/providers/leaky/schema.df");
    let schema = schema.to_str().unwrap();
    let delay = format!("delay=leaky.vault[\"v\"]:{}", DELAY_MS / 3);
    let apply = |target: &str, pw: &str, chaos: &[&str]| {
        let set = format!("pw={pw}");
        let mut a = vec!["dev", "--provider", schema];
        for c in chaos {
            a.extend(["--chaos", c]);
        }
        a.extend(["apply", target, "--set", &set, "--yes"]);
        traced(&s, &a)
    };
    // Applied once, prod's world is where the reader reads its secret.
    apply("prod", "FIRST", &[]).0.success();
    apply("app", "FIRST", &[]).0.success();
    let (r, spans) = apply("app", "ROTATED", &[&delay]);
    r.success();
    let (v, copy) = (
        applies(&spans, "leaky.vault v"),
        applies(&spans, "leaky.vault copy"),
    );
    let ([(_, _, v_end)], [(_, copy_start, _)]) = (v.as_slice(), copy.as_slice()) else {
        panic!("one call each: {v:?} {copy:?}");
    };
    assert!(copy_start >= v_end, "copy after v: {v:?} {copy:?}");
    let w = s.json("dform.state/stacks.app/remote.json");
    assert_eq!(
        w["resources"]["leaky.vault::copy"]["materialized"]["backup"], "ROTATED",
        "{w}"
    );
}

/// A reader's provider configured from an output its upstream's plan
/// replaces (the server a kubeconfig names, gone): the reader configures
/// it at once from what the upstream published, provisionally, while the
/// upstream's plan goes (its Plan call, the first, slowed); the old server is tried
/// once, within the first contact's timeout (its 1 s), and never again.
/// The upstream's plan replaces it, so the reader is planned again over
/// the plan's, its rows waiting on the provider, and its plan says what it
/// holds and on what. Once the tick replaces the server the provider is
/// configured from the new one.
#[test]
fn a_provider_configured_from_an_output_its_upstream_replaces_tries_the_old_one_once() {
    let s = Scratch::project("tree-tick-stale");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nt = \"providers/t\"\n\
         k8s = { source = \"k8s\", timeout = \"1s\" }\n",
    );
    s.write(
        "providers/t/schema.df",
        "type_provider(\"t.server\", \"tcloud\")\n\
         type_attr(\"t.server\", \"id\", \"string\", [\"computed\", \"id\"])\n\
         type_mint(\"t.server\", \"id\", \"{hash}\")\n\
         type_attr(\"t.server\", \"zone\", \"string\", [\"required\", \"force_new\"])\n\
         type_attr(\"t.server\", \"address\", \"string\", [\"computed\"])\n\
         type_mint(\"t.server\", \"address\", \"https://{doc:zone}.example\")\n",
    );
    let platform = |zone: &str| {
        format!(
            "use t\nresource t.server main {{ zone = \"{zone}\" }}\n\
             output kubeconfig = main.address\n"
        )
    };
    s.write("stacks/platform.df", &platform("a"));
    s.write(
        "stacks/apps.df",
        "use stacks.platform\nuse k8s { kubeconfig = platform.kubeconfig }\n\
         resource k8s.namespace apps { metadata.name = \"apps\" }\noutput ns = \"apps\"\n",
    );
    let recorded = s.path("settings.jsonl");
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let out = common::dform()
            .args(args)
            .current_dir(&s.dir)
            .env("NO_COLOR", "1")
            .env("DFORM_TEST_RECORD_SETTINGS", &recorded)
            .envs(env.iter().copied())
            .env_remove("DFORM_TRACE")
            .output()
            .unwrap();
        common::Run::from(out).success()
    };
    // Apps' lines in the record since the last taken: each kubeconfig its
    // k8s was configured with, and each call to the server that is gone.
    let take = || -> (Vec<String>, Vec<String>) {
        let lines = s.read("settings.jsonl");
        std::fs::remove_file(&recorded).unwrap();
        let apps: Vec<Json> = lines
            .lines()
            .map(|l| serde_json::from_str::<Json>(l).unwrap())
            .filter(|l| l["stack"] == "apps")
            .collect();
        let of = |k: &str| {
            apps.iter()
                .filter_map(|l| l.pointer(k)?.as_str().map(str::to_string))
                .collect()
        };
        (of("/settings/kubeconfig"), of("/dead"))
    };
    run(&["apply", "apps", "--yes"], &[]);
    std::fs::remove_file(&recorded).unwrap();
    s.write("stacks/platform.df", &platform("b"));
    // Platform's Plan call, the first, slowed: apps configures k8s while
    // platform plans.
    let slowed = || {
        let port =
            common::answering_after(std::time::Duration::from_secs(3), std::time::Duration::ZERO);
        format!("127.0.0.1:{port}")
    };
    let r = run(
        &["plan", "apps", "--trace", "trace.jsonl"],
        &[
            ("DFORM_TEST_FAKE_PLAN_AWAITS", &slowed()),
            ("DFORM_TEST_DEAD_ENDPOINT", "https://a.example"),
        ],
    );
    assert!(
        r.stdout.contains(
            "~ stacks.apps      stacks/apps.df  up to date but for 1 held, waiting on provider \
             k8s  kubeconfig = platform.kubeconfig\n"
        ),
        "{}",
        r.stdout
    );
    let (kubeconfigs, dead) = take();
    assert_eq!(
        kubeconfigs,
        ["https://a.example"],
        "configured at once from what platform published"
    );
    assert_eq!(dead, ["Configure"], "the server that is gone, once");
    // Its Configure gave up after the first contact's timeout, not the
    // server's 5 s.
    let spans = common::spans(&s.path("trace.jsonl"));
    let ns = |x: &Json, k: &str| x[k].as_str().unwrap().parse::<u128>().unwrap();
    let first = spans
        .iter()
        .filter(|x| x["name"] == "deployment stacks.apps")
        .min_by_key(|x| ns(x, "startTimeUnixNano"))
        .expect("apps' deployment");
    let mut ids = vec![first["spanId"].clone()];
    let mut longest = 0;
    while let Some(id) = ids.pop() {
        for x in spans.iter().filter(|x| x["parentSpanId"] == id) {
            ids.push(x["spanId"].clone());
            if x["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("configure"))
            {
                longest = longest.max(ns(x, "endTimeUnixNano") - ns(x, "startTimeUnixNano"));
            }
        }
    }
    assert!(
        (900_000_000..3_000_000_000).contains(&longest),
        "apps' longest configure: {longest} ns"
    );
    run(
        &["apply", "apps", "--yes"],
        &[("DFORM_TEST_FAKE_PLAN_AWAITS", &slowed())],
    );
    assert_eq!(
        take().0,
        ["https://a.example", "https://b.example"],
        "the old server's while platform plans, then the new one's"
    );
}
