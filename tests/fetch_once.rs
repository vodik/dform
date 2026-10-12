//! One fetch per call per run: within one run each call and its
//! arguments is fetched once, however many times the program writes it
//! and however many evaluation rounds ask it, and each object state maps
//! is read once between two writes; so a run sees one snapshot of the
//! outside world. Counted on the run's trace (`--trace FILE`), one span
//! per call it made.

use crate::common;
use common::{Run, Scratch, attr, calls, registry_answering_together, spans, traced};
use serde_json::Value;

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

/// A plan reads each of its state objects in the bucket once (the state,
/// the audit log and its segments, the master), however many parts of
/// the run ask for them.
#[test]
fn a_plan_reads_each_state_object_once() {
    let server = dform_s3::fake::Server::start();
    let s = Scratch::project("fetch-once-s3");
    common::copy_dir(&common::repo().join("tests/fixtures/demo"), &s.dir);
    let mut toml = std::fs::read_to_string(s.path("dform.toml")).unwrap();
    toml.push_str(&format!(
        "\n[defaults]\nbackend = 's3(\"dform-test\", \"once/{{stack}}\", \
         {{endpoint: \"{}\", region: \"us-east-1\"}})'\n\n\
         [secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
        server.endpoint
    ));
    s.write("dform.toml", &toml);
    let run = |args: &[&str]| {
        let out = common::dform()
            .args(common::yes(args))
            .current_dir(&s.dir)
            .env("NO_COLOR", "1")
            .env("DFORM_S3_ACCESS_KEY_ID", "fake")
            .env("DFORM_S3_SECRET_ACCESS_KEY", "fake")
            .env("DFORM_TEST_PASSPHRASE", "fetch once")
            .output()
            .unwrap();
        Run::from(out)
    };
    run(&["apply", "dform", "env=staging"]).success();
    let before = server.requests().len();
    let r = run(&["plan", "dform", "env=staging"]).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
    let gets: Vec<String> = server.requests()[before..]
        .iter()
        .filter_map(|q| q.strip_prefix("GET /dform-test/once/"))
        .map(str::to_string)
        .collect();
    let mut once = gets.clone();
    once.sort();
    once.dedup();
    assert_eq!(gets.len(), once.len(), "an object read twice: {gets:?}");
    for k in ["state.json", "state.master", "state.audit/000001.jsonl"] {
        assert!(gets.iter().any(|g| g.ends_with(k)), "{k} in {gets:?}");
    }
}
