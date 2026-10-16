//! `set` through a relation (the decision of 2026-10-16): a policy's
//! floors are one `set` per thing, its target placed by a relation the
//! program's rules define, `pod(w, p)` over every workload kind. The `set`
//! is the `set` once per rule of the relation, the rule's clause in its
//! place; a rule's `p = w.spec.template` makes `p` that place. What cannot
//! be unfolded is refused where it is written.

use crate::common;
use common::{Scratch, says_error};
use std::collections::{BTreeMap, BTreeSet};

/// One of each workload kind the k8s mock serves, each with a container;
/// the deployment's own `securityContext` and a second container.
const WORKLOADS: &str = r#"
use k8s

resource k8s.deployment web {
  metadata.name = "web"
  spec.selector.matchLabels = { app = "web" }
  spec.template.spec.containers = [
    { name = "api", image = "api:1", securityContext = { privileged = true } },
    { name = "side", image = "side:1" },
  ]
}

resource k8s.stateful_set db {
  metadata.name = "db"
  spec.serviceName = "db"
  spec.selector.matchLabels = { app = "db" }
  spec.template.spec.containers = [{ name = "pg", image = "pg:16" }]
}

resource k8s.daemon_set agent {
  metadata.name = "agent"
  spec.selector.matchLabels = { app = "agent" }
  spec.template.spec.containers = [{ name = "agent", image = "agent:1" }]
}

resource k8s.job migrate {
  metadata.name = "migrate"
  spec.template.spec.containers = [{ name = "migrate", image = "migrate:1" }]
}

resource k8s.cron_job nightly {
  metadata.name = "nightly"
  spec.schedule = "0 3 * * *"
  spec.jobTemplate.spec.template.spec.containers = [{ name = "backup", image = "backup:1" }]
}

pod(w, p) where w in k8s.deployment, p = w.spec.template
pod(w, p) where w in k8s.stateful_set, p = w.spec.template
pod(w, p) where w in k8s.daemon_set, p = w.spec.template
pod(w, p) where w in k8s.job, p = w.spec.template
pod(w, p) where w in k8s.cron_job, p = w.spec.jobTemplate.spec.template
"#;

/// Each workload's pod template, by its address in the plan.
const PODS: [(&str, &str); 5] = [
    ("k8s.deployment[\"web\"]", "spec.template"),
    ("k8s.stateful_set[\"db\"]", "spec.template"),
    ("k8s.daemon_set[\"agent\"]", "spec.template"),
    ("k8s.job[\"migrate\"]", "spec.template"),
    (
        "k8s.cron_job[\"nightly\"]",
        "spec.jobTemplate.spec.template",
    ),
];

/// Where the line holding `text` of `s`'s `main.df` is, as an error
/// names it.
fn at(s: &Scratch, text: &str) -> String {
    let src = s.read("main.df");
    let line = src.lines().position(|l| l.contains(text)).unwrap() + 1;
    format!("main.df:{line}")
}

fn project(name: &str, policy: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("main.df", &format!("{WORKLOADS}\n{policy}\n"));
    s
}

/// The plan's changes, `--json`: each resource's paths and the values
/// they get, and how many times the plan names each resource.
fn plan(s: &Scratch) -> (BTreeMap<String, BTreeMap<String, String>>, Vec<String>) {
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--json", "main.df"])
        .success();
    let plan: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let (mut out, mut named) = (BTreeMap::new(), Vec::new());
    for tick in plan["ticks"].as_array().unwrap() {
        for c in tick["changes"].as_array().unwrap() {
            let addr = c["address"].as_str().unwrap().to_string();
            named.push(addr.clone());
            let paths: &mut BTreeMap<String, String> = out.entry(addr).or_default();
            for leaf in c["changes"].as_array().unwrap() {
                let path = leaf["path"].as_str().unwrap().to_string();
                paths.insert(path, leaf["after"].to_string());
            }
        }
    }
    (out, named)
}

/// The user's policy: one `pod` relation over the five workload kinds,
/// one `set` per thing through it: requests at `@default` in every
/// container, a `securityContext` written whole over a container's own,
/// a label on every pod. Each resource is planned once, each with the
/// writes at its own template's path; and the same `set` with the place
/// in its own clause (`p = w.spec.template`) writes the same.
#[test]
fn a_set_through_pod_writes_every_workload_kind() {
    let policy = r#"
set c.resources.requests = { cpu = 100m, memory = 128Mi } @default
  where pod(_, p), c in p.spec.containers
set c.securityContext := { runAsNonRoot = true } @override
  where pod(_, p), c in p.spec.containers
set p.metadata.labels.team = "platform" where pod(_, p)
set p.metadata.labels.app = w.metadata.name where w in k8s.deployment, p = w.spec.template
"#;
    let (plan, named) = plan(&project("set-through-pod", policy));
    let once: BTreeSet<&String> = named.iter().collect();
    assert_eq!(once.len(), named.len(), "{named:?}");
    assert_eq!(named.len(), PODS.len(), "{named:?}");
    for (addr, pod) in PODS {
        let paths = &plan[addr];
        let containers: Vec<&String> = paths.keys().filter(|k| k.ends_with("].image")).collect();
        assert!(!containers.is_empty(), "{addr}: {paths:#?}");
        for image in containers {
            let c = image.trim_end_matches(".image");
            let want = [
                (format!("{c}.resources.requests.cpu"), "\"100m\""),
                (format!("{c}.resources.requests.memory"), "\"128Mi\""),
                (format!("{c}.securityContext.runAsNonRoot"), "true"),
            ];
            for (path, value) in want {
                assert_eq!(
                    paths.get(&path).map(String::as_str),
                    Some(value),
                    "{addr} {path}"
                );
            }
            assert!(
                c.starts_with(&format!("{pod}.spec.containers[")),
                "{addr}: {c}"
            );
        }
        let team = format!("{pod}.metadata.labels.team");
        assert_eq!(paths[&team], "\"platform\"", "{addr}");
    }
    let web = &plan["k8s.deployment[\"web\"]"];
    assert_eq!(web["spec.template.metadata.labels.app"], "\"web\"");
    let privileged = "spec.template.spec.containers[name=api].securityContext.privileged";
    assert!(!web.contains_key(privileged), "{web:#?}");
}

/// A relation through another: `container(w, c)` over `pod`, the `set`
/// unfolded through both.
#[test]
fn a_set_through_two_relations() {
    let policy = r#"
container(w, c) where pod(w, p), c in p.spec.containers
set c.resources.limits.memory = 256Mi @default where container(_, c)
"#;
    let (plan, _) = plan(&project("set-through-two", policy));
    for (addr, pod) in PODS {
        let limits = plan[addr]
            .iter()
            .filter(|(k, v)| {
                k.starts_with(pod) && k.ends_with("limits.memory") && *v == "\"256Mi\""
            })
            .count();
        assert!(limits > 0, "{addr}: {:#?}", plan[addr]);
    }
}

/// What cannot be unfolded is refused at its site, in the program's
/// words: a relation that places through itself, one a fact gives rows,
/// a rule that does not say where its column is, a value where the `set`
/// joins by a variable.
#[test]
fn what_cannot_be_unfolded_is_refused_where_it_is() {
    let set = "set q.metadata.labels.team = \"x\" where placed(_, q)";
    for (rule, site, want) in [
        (
            "placed(w, p) where placed(w, q), p = q.spec",
            "placed(w, q)",
            "`set` writes through `placed`, and a rule of `placed` places what it writes \
             through `placed` again",
        ),
        (
            "placed(\"a\", \"b\")",
            "where placed(_, q)",
            "`set` writes through `placed`, which a fact gives rows to",
        ),
        (
            "placed(w, p) where w in k8s.job, p = {}",
            "k8s.job, p = {}",
            "`set` writes through `placed`, and this rule does not say where `p` is",
        ),
    ] {
        let rules = format!("placed(w, p) where pod(w, p)\n{rule}");
        let s = project("set-through-refused", &format!("{rules}\n{set}"));
        let r = s
            .run(&["dev", "--world", "w.json", "plan", "main.df"])
            .failure();
        let want = format!("{}:1: {want}", at(&s, site));
        assert!(says_error(&r.stderr, &want), "{want}:\n{}", r.stderr);
    }
    let s = project(
        "set-through-value",
        "set p.metadata.labels.team = \"x\" where pod(\"web\", p)",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "main.df"])
        .failure();
    let want =
        "`set` writes through `pod`, which joins its rules by variables: `\"web\"` is a value";
    assert!(says_error(&r.stderr, want), "{}", r.stderr);
}

/// The loop trap: a rule that chooses its place by what the `set` writes
/// there is the cycle any write that reads itself is, and the error shows
/// the rule the `set` went through.
#[test]
fn a_rule_choosing_its_place_by_what_the_set_writes_is_the_loop() {
    let policy = r#"
picked(w, p) where w in k8s.deployment, p = w.spec.template, p.metadata.labels.tier == "web"
set p.metadata.labels.team = "x" where picked(_, p)
"#;
    let s = project("set-through-loop", policy);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "main.df"])
        .failure();
    for want in [
        "k8s.deployment.spec waits on itself".to_string(),
        format!(
            "through {}  picked(w, p) where w in k8s.deployment, p = w.spec.template, \
             p.metadata.labels.tier == \"web\"",
            at(&s, "picked(w, p) where")
        ),
    ] {
        assert!(r.stderr.contains(&want), "{want}:\n{}", r.stderr);
    }
}

/// Positions a `set` does not yet write through, each refused today.
#[test]
#[ignore = "not yet: a used module's relation (`m.pod(w, p)`) is read, never unfolded; \
            step (b) brings std's `pod`"]
fn a_set_through_a_used_modules_relation() {
    let s = project(
        "set-through-module",
        "use pods\nset p.metadata.labels.team = \"x\" where pods.pod(_, p)",
    );
    s.write(
        "pods.df",
        "\nuse k8s\npod(w, p) where w in k8s.deployment, p = w.spec.template\n",
    );
    s.run(&["dev", "--world", "w.json", "plan", "main.df"])
        .success();
}
