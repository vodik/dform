//! A tree's plan runs its deployments at once, each waiting only on those
//! it reads (`board`): two that read nothing of each other are planned
//! together; a reader's evaluation starts once what it reads is
//! evaluated, before that deployment's Plan calls are answered; a reader
//! that read what an upstream's plan changes is planned again over the
//! plan's; a deployment whose plan is refused stops its readers, with its
//! own message.

use crate::common;
use common::{Run, Scratch, repo, spans};
use serde_json::Value as Json;

/// `dform ARGS` in `s`, its trace kept, with `env` set.
fn traced(s: &Scratch, args: &[&str], env: &[(&str, &str)]) -> (Run, Vec<Json>) {
    let out = common::dform()
        .args(args)
        .args(["--trace", "trace.jsonl"])
        .current_dir(&s.dir)
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    (Run::from(out), spans(&s.path("trace.jsonl")))
}

/// A span's start and end, in ns.
fn time(x: &Json) -> (u128, u128) {
    let at = |k: &str| x[k].as_str().unwrap().parse::<u128>().unwrap();
    (at("startTimeUnixNano"), at("endTimeUnixNano"))
}

/// The span named `name`.
fn named<'a>(spans: &'a [Json], name: &str) -> &'a Json {
    spans
        .iter()
        .find(|x| x["name"] == name)
        .unwrap_or_else(|| panic!("no span {name}"))
}

/// The spans named `name` under `top`, at any depth.
fn under<'a>(spans: &'a [Json], top: &Json, name: &str) -> Vec<&'a Json> {
    let mut ids = vec![top["spanId"].clone()];
    let mut out = Vec::new();
    while let Some(id) = ids.pop() {
        for x in spans.iter().filter(|x| x["parentSpanId"] == id) {
            ids.push(x["spanId"].clone());
            if x["name"] == name {
                out.push(x);
            }
        }
    }
    out
}

/// Two deployments that read nothing of each other (a stack's, at two
/// keys the project lists), each resolving its own tag at a registry that
/// answers only once both are asked at once: the plan succeeds only if
/// their evaluations run together, and their deployments' spans overlap.
#[test]
fn deployments_that_read_nothing_of_each_other_are_planned_together() {
    let port = common::registry_answering_together(2, std::time::Duration::from_secs(10));
    let s = Scratch::project("plan-concurrent-apart");
    s.write(
        "stacks/app.df",
        &format!(
            "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\n\
             resource compute.vm web {{ image = oci.resolve(\"127.0.0.1:{port}/acme/app:a-${{env}}\") }}\n"
        ),
    );
    s.write(
        "project.df",
        "resource stacks.app lab { env = \"lab\" }\nresource stacks.app prod { env = \"prod\" }\n",
    );
    std::fs::create_dir_all(s.path("cache")).unwrap();
    let cache = s.path("cache");
    let (r, spans) = traced(
        &s,
        &["plan"],
        &[("XDG_CACHE_HOME", cache.to_str().unwrap())],
    );
    let r = r.success();
    // Each pinned to the digest the registry answered (the plan elides
    // its middle).
    assert_eq!(r.stdout.matches("…aaaaaaaa").count(), 2, "{}", r.stdout);
    let (lab, prod) = (
        time(named(&spans, "deployment stacks.app[env=lab]")),
        time(named(&spans, "deployment stacks.app[env=prod]")),
    );
    assert!(lab.0 < prod.1 && prod.0 < lab.1, "apart: {lab:?} {prod:?}");
}

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
output cidr = edge.cidr
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc rec { cidr = "10.1.0.0/16", name = platform[env].cidr }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s
}

/// What apps reads of platform is known once platform is evaluated:
/// apps' evaluation starts after platform's ends, and runs while
/// platform's Plan call is in flight. Platform's Plan call and apps' tag
/// are asked of a registry that answers once both are in flight
/// together: the plan succeeds only if apps' evaluation does not wait for
/// platform's Plan calls. Apps' provider is a process of its own (the
/// Kubernetes provider, offline): a call of its to platform's would wait
/// behind that Plan call in the provider's one thread.
#[test]
fn a_reader_is_evaluated_while_its_upstream_s_plan_calls_are_in_flight() {
    let port = common::registry_answering_together(2, std::time::Duration::from_secs(10));
    let s = project("plan-concurrent-reader");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "stacks/apps.df",
        &format!(
            r#"key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s {{ source = "./providers/k8s" }}
resource k8s.namespace apps {{
  metadata.name = "apps-${{env}}"
  metadata.annotations.cidr = platform[env].cidr
  metadata.annotations.image = oci.resolve("127.0.0.1:{port}/acme/apps:a1")
}}
"#
        ),
    );
    std::fs::create_dir_all(s.path("cache")).unwrap();
    let cache = s.path("cache");
    let at = format!("127.0.0.1:{port}");
    let (r, spans) = traced(
        &s,
        &["plan", "apps"],
        &[
            ("DFORM_TEST_FAKE_PLAN_AWAITS", at.as_str()),
            ("DFORM_K8S_OFFLINE", "1"),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
    );
    let r = r.success();
    assert!(
        r.stdout.contains("/acme/apps:a1@s…aaaaaaaa"),
        "{}",
        r.stdout
    );
    let platform = named(&spans, "deployment stacks.platform[env=lab]");
    let apps = named(&spans, "deployment stacks.apps[env=lab]");
    let planned = time(under(&spans, platform, "planned (refresh, Plan calls, policy)")[0]);
    let evaluated = under(&spans, platform, "evaluated")
        .into_iter()
        .map(time)
        .filter(|t| t.0 < planned.0)
        .map(|t| t.1)
        .max()
        .unwrap();
    let reader = under(&spans, apps, "evaluated")
        .into_iter()
        .map(|x| time(x).0)
        .min()
        .unwrap();
    assert!(evaluated <= reader, "apps evaluated before platform was");
}

/// A reader of an upstream that has published starts at once, over what
/// it published: apps is evaluated while platform's evaluation is still
/// in flight. Each resolves a tag of a registry that answers only once
/// both are asked at once: the plan succeeds only if apps does not wait
/// for platform's evaluation.
#[test]
fn a_reader_of_what_its_upstream_published_is_evaluated_beside_it() {
    let s = project("plan-concurrent-published");
    s.run(&["apply", "apps", "--yes"]).success();
    let port = common::registry_answering_together(2, std::time::Duration::from_secs(10));
    let resolve = |name: &str, tag: &str| {
        format!(
            "resource compute.vm {name} {{ image = oci.resolve(\"127.0.0.1:{port}/acme/{name}:{tag}\") }}\n"
        )
    };
    s.write(
        "stacks/platform.df",
        &(PLATFORM.to_string() + &resolve("build", "a1")),
    );
    s.write(
        "stacks/apps.df",
        &(APPS.to_string() + &resolve("web", "a1")),
    );
    std::fs::create_dir_all(s.path("cache")).unwrap();
    let cache = s.path("cache");
    let (r, spans) = traced(
        &s,
        &["plan", "apps"],
        &[("XDG_CACHE_HOME", cache.to_str().unwrap())],
    );
    let r = r.success();
    assert_eq!(r.stdout.matches("…aaaaaaaa").count(), 2, "{}", r.stdout);
    let first = |d: &str, name: &str| {
        let d = named(&spans, &format!("deployment stacks.{d}[env=lab]"));
        under(&spans, d, name).into_iter().map(time).min().unwrap()
    };
    let (platform, apps) = (first("platform", "evaluated"), first("apps", "evaluated"));
    assert!(
        apps.0 < platform.1,
        "apps after platform's evaluation: {apps:?} {platform:?}"
    );
}

/// Platform's plan replaces the database its output is made of: what its
/// evaluation knew of the endpoint (the old one's) is known only after
/// tick 1 in its plan. Apps, which read the old one, is planned again
/// over the plan's: its change waits on that tick.
#[test]
fn a_reader_of_what_its_upstream_s_plan_changes_is_planned_again() {
    let s = Scratch::project("plan-concurrent-again");
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(\"db.postgres\", \"name\", \"string\", [\"force_new\"])\n"),
    );
    let platform = |name: &str| {
        format!(
            "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\n\
             resource db.postgres db {{ name = \"{name}-${{env}}\" }}\n\
             output endpoint = db.endpoint\n"
        )
    };
    s.write("stacks/platform.df", &platform("db"));
    s.write(
        "stacks/apps.df",
        &APPS.replace("platform[env].cidr", "platform[env].endpoint"),
    );
    s.run(&["apply", "apps", "--yes"]).success();
    s.write("stacks/platform.df", &platform("db2"));
    let r = s.run(&["plan", "apps"]).success();
    assert!(
        r.stdout.contains(
            "  tick 2  1 change\n    waits on  stacks.platform[env=lab].endpoint\n    ~ net.vpc rec"
        ),
        "{}",
        r.stdout
    );
}

/// A deny refuses platform's plan after its evaluation made its outputs
/// known: apps is not planned, and the tree's plan is refused with the
/// refusal platform's own plan says.
#[test]
fn a_refused_upstream_stops_its_reader_with_its_own_message() {
    let s = project("plan-concurrent-refused");
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}deny \"no platform in lab\" where env == \"lab\"\n"),
    );
    let alone = s.run(&["plan", "platform"]);
    assert_eq!(alone.code, Some(4), "{}", alone.stderr);
    let r = s.run(&["plan", "apps"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout
            .contains("stacks/apps.df  not planned: stacks.platform[env=lab] refused"),
        "{}",
        r.stdout
    );
    let said = |stderr: &str| -> Vec<String> {
        stderr
            .lines()
            .filter(|l| l.starts_with("refused"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(said(&r.stderr), said(&alone.stderr));
}

/// A server that answers each request 200 after `delay`: what platform's
/// Plan call waits on (`DFORM_TEST_FAKE_PLAN_AWAITS`).
fn answering_after(delay: std::time::Duration) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in l.incoming().flatten() {
            let mut r = BufReader::new(conn.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            std::thread::sleep(delay);
            let _ = conn
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    port
}

/// Apps waits for platform's plan, whose Plan call takes 5 s, before
/// it says what it read: the wait is a span of apps' deployment, named
/// for the one it waits on and what for (`until`), never a gap in the
/// trace.
#[test]
fn a_reader_s_wait_on_its_upstream_is_a_span_of_its_own() {
    let port = answering_after(std::time::Duration::from_secs(5));
    let s = project("plan-concurrent-wait");
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    s.write(
        "stacks/apps.df",
        r#"key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s { source = "./providers/k8s" }
resource k8s.namespace apps {
  metadata.name = "apps-${env}"
  metadata.annotations.cidr = platform[env].cidr
}
"#,
    );
    let at = format!("127.0.0.1:{port}");
    let (r, spans) = traced(
        &s,
        &["plan", "apps"],
        &[
            ("DFORM_TEST_FAKE_PLAN_AWAITS", at.as_str()),
            ("DFORM_K8S_OFFLINE", "1"),
        ],
    );
    r.success();
    let apps = named(&spans, "deployment stacks.apps[env=lab]");
    let waits = under(&spans, apps, "waits on stacks.platform[env=lab]");
    let wait = waits
        .iter()
        .find(|w| common::attr(w, "until").as_deref() == Some("planned"))
        .expect("no wait for platform's plan under apps");
    assert_eq!(
        common::attr(wait, "upstream").as_deref(),
        Some("stacks.platform[env=lab]")
    );
    let (start, end) = time(wait);
    assert!(end - start > 500_000_000, "a wait of {} ns", end - start);
    let platform = named(&spans, "deployment stacks.platform[env=lab]");
    assert!(under(&spans, platform, "waits on stacks.platform[env=lab]").is_empty());
}
