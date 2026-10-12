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
