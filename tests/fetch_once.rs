//! One fetch per call per run: within one run each call and its
//! arguments is fetched once, however many times the program writes it
//! and however many evaluation rounds ask it, and each object state maps
//! is read once between two writes; so a run sees one snapshot of the
//! outside world. Counted on the run's trace (`--trace FILE`), one span
//! per call it made.

use crate::common;
use common::{Run, Scratch};
use serde_json::Value;

/// The spans of the trace file at `path`: each one's name and attributes.
fn spans(path: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = Vec::new();
    for line in text.lines() {
        let req: Value = serde_json::from_str(line).unwrap();
        for rs in req["resourceSpans"].as_array().unwrap() {
            for ss in rs["scopeSpans"].as_array().unwrap() {
                out.extend(ss["spans"].as_array().unwrap().iter().cloned());
            }
        }
    }
    out
}

/// The span's attribute `k`, as text.
fn attr(s: &Value, k: &str) -> Option<String> {
    s["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == k)
        .and_then(|a| a["value"].as_object()?.values().next().cloned())
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
}

/// The spans named `name`, by their `addr`, sorted.
fn calls(spans: &[Value], name: &str) -> Vec<String> {
    let mut out: Vec<String> = spans
        .iter()
        .filter(|s| s["name"] == name)
        .map(|s| attr(s, "addr").unwrap_or_default())
        .collect();
    out.sort();
    out
}

/// `dform ARGS` in `dir` of `s`, its trace to `trace.jsonl` there.
fn traced(s: &Scratch, args: &[&str]) -> (Run, Vec<Value>) {
    let out = common::dform()
        .args(args)
        .args(["--trace", "trace.jsonl"])
        .env("NO_COLOR", "1")
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    let run = Run::from(out);
    let spans = spans(&s.path("trace.jsonl"));
    (run, spans)
}

/// The k3s shape on the mock (tests/deferred_refresh.rs): two servers of
/// a provider plugin, and a provider configured mid-plan from one of
/// them, serving three objects.
const MID_PLAN: &str = r#"
use fake { source = "prov" }
resource db.postgres server { name = "server" }
resource db.postgres other { name = "other" }
use k8s { kubeconfig = str.format("kc@%s", server.endpoint) }
resource k8s.namespace ns { metadata.name = "app" }
resource k8s.namespace web { metadata.name = "web" }
resource k8s.namespace cache { metadata.name = "cache" }
"#;

/// A plan reads each object state maps once: the provider configured
/// mid-plan adds its own three objects' Reads to the two servers', and
/// the servers are not read again.
#[test]
fn a_provider_configured_mid_plan_reads_only_its_own_objects() {
    let s = Scratch::new("fetch-once-refresh");
    s.write("p.df", MID_PLAN);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s.run(&["dev", "--world", "w.json", "apply", "--yes", "p.df"])
        .success();
    let (r, spans) = traced(&s, &["dev", "--world", "w.json", "plan", "p.df"]);
    let r = r.success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    assert_eq!(
        calls(&spans, "fakecloud Read"),
        [
            "db.postgres other",
            "db.postgres server",
            "k8s.namespace cache",
            "k8s.namespace ns",
            "k8s.namespace web",
        ],
        "each object read once"
    );
}

/// The same table call written in two files (a stack and the module it
/// uses) and asked again in a second evaluation round (its argument the
/// first answer) is asked of its provider once.
#[test]
fn an_extern_asked_from_two_files_and_two_rounds_is_asked_once() {
    let s = Scratch::project("fetch-once-extern");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\novh = \"providers/ovh\"\n",
    );
    let schema = common::repo().join("examples/k3s-platform/providers/ovh/schema.df");
    s.write(
        "providers/ovh/schema.df",
        &std::fs::read_to_string(schema).unwrap(),
    );
    s.write(
        "providers/ovh/externs.df",
        "ovh.zone(\"example.com\", \"example.com\", [\"ns1.example.net\"])\n",
    );
    s.write(
        "zones.df",
        "use ovh\noutput id: string = i where ovh.zone(\"example.com\", i, _)\n",
    );
    s.write(
        "stacks/app.df",
        r#"use ovh
use zones
output id: string = i where ovh.zone("example.com", i, _)
output again = n where ovh.zone("example.com", i, _), ovh.zone(i, _, n)
output theirs = zones.id
"#,
    );
    let (r, spans) = traced(&s, &["plan", "app"]);
    r.success();
    assert!(
        spans.iter().any(|s| s["name"] == "evaluation, round 2"),
        "a second round asks it again"
    );
    assert_eq!(calls(&spans, "fakecloud Query"), ["ovh.zone"]);
}

/// A registry that answers a manifest's HEAD only once `n` are asked at
/// once (within `budget`; else 503): each tag's digest is `sha256:` and
/// its first letter, 64 times.
fn registry_answering_together(n: usize, budget: std::time::Duration) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Condvar, Mutex};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let open = Arc::new((Mutex::new(0usize), Condvar::new()));
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            let open = open.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut first = String::new();
                r.read_line(&mut first).unwrap_or(0);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let tag = first
                    .split_whitespace()
                    .nth(1)
                    .and_then(|t| t.rsplit_once("/manifests/"))
                    .map(|(_, t)| t.to_string())
                    .unwrap_or_default();
                let (count, cv) = &*open;
                let mut count = count.lock().unwrap();
                *count += 1;
                cv.notify_all();
                let (count, _) = cv.wait_timeout_while(count, budget, |c| *c < n).unwrap();
                let together = *count >= n;
                drop(count);
                let c = tag.chars().next().unwrap_or('0');
                let reply = match together {
                    true => format!(
                        "HTTP/1.1 200 OK\r\ndocker-content-digest: sha256:{}\r\n\
                         content-length: 0\r\nconnection: close\r\n\r\n",
                        c.to_string().repeat(64)
                    ),
                    false => "HTTP/1.1 503 alone\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string(),
                };
                let mut conn = conn;
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
    port
}

/// The calls of one round are independent, so they are asked together:
/// four image tags resolved at a registry that answers only when all four
/// are asked at once. In the trace they are one block (one `waited_on`),
/// each asked once.
#[test]
fn a_rounds_calls_are_asked_together() {
    let port = registry_answering_together(4, std::time::Duration::from_secs(5));
    let s = Scratch::project("fetch-together");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    let containers: Vec<String> = ["a1", "b2", "c3", "d4"]
        .iter()
        .map(|t| {
            format!(r#"{{ name: "c{t}", image: oci.resolve("127.0.0.1:{port}/acme/app:{t}") }}"#)
        })
        .collect();
    s.write(
        "p.df",
        &format!(
            r#"use k8s
resource k8s.deployment app {{
  metadata.name = "app"
  metadata.namespace = "default"
  spec.selector.matchLabels = {{ app: "app" }}
  spec.template.metadata.labels = {{ app: "app" }}
  spec.template.spec.containers = [{}]
}}
"#,
            containers.join(", ")
        ),
    );
    std::fs::create_dir_all(s.path("cache")).unwrap();
    let out = common::dform()
        .args(["plan", "p.df", "--trace", "trace.jsonl"])
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", s.path("cache"))
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    let r = Run::from(out).success();
    for t in ["a1", "b2", "c3", "d4"] {
        let c = t.chars().next().unwrap().to_string().repeat(64);
        let pinned = format!("127.0.0.1:{port}/acme/app:{t}@sha256:{c}");
        assert!(r.stdout.contains(&pinned), "{pinned}\n{}", r.stdout);
    }
    let spans = spans(&s.path("trace.jsonl"));
    let resolves: Vec<&Value> = spans
        .iter()
        .filter(|s| s["name"] == "oci.resolve")
        .collect();
    assert_eq!(resolves.len(), 4, "each tag asked once");
    let waited: std::collections::BTreeSet<Option<String>> =
        resolves.iter().map(|s| attr(s, "waited_on")).collect();
    assert_eq!(waited.len(), 1, "one block: {waited:?}");
}
