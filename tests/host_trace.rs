//! A wasm provider's trace (the `wasm` feature): its start is
//! `wasm.compile`, `wasm.link` and `wasm.instantiate` under the
//! provider's start, which ends once it is started; each call's export
//! runs in the call's `provider.call` span, no span of its own: the span
//! carries how long the call took in the provider (`provider.took`) and
//! a trap that stopped it (`error.type`); the requests the component
//! sends through the host are under its call.
//!
//! The components are built apart, as for tests/host_wasm.rs:
//!
//!   cargo xtask components
//!   cargo test --features wasm --test main -- host_trace::

#![cfg(feature = "wasm")]

use crate::common;
use common::Scratch;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

/// The component `file` under the wasip3 target's directory, release or
/// debug, else a panic naming the `build` command.
pub(crate) fn built(file: &str, build: &str) -> String {
    let target = std::path::Path::new(env!("CARGO_BIN_EXE_dform"))
        .parent()
        .and_then(|d| d.parent())
        .unwrap()
        .to_path_buf();
    for profile in ["release", "debug"] {
        let p = target.join(format!("wasm32-wasip3/{profile}/{file}"));
        if p.exists() {
            return p.display().to_string();
        }
    }
    panic!("the component {file} is not built: {build}");
}

/// The compiled components' cache tests/host_wasm.rs keeps: compiled once.
pub(crate) fn wasm_cache() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("host-wasm-cache")
}

/// `dform ARGS` on `p.df` against the world `w.json`, the mock the fake
/// component, its trace written to `run.jsonl`: its spans.
fn traced(s: &Scratch, args: &[&str]) -> (common::Run, Vec<Json>) {
    let fake = built(
        "dform_provider_fake.wasm",
        "cargo build -p dform-provider-fake --release --target wasm32-wasip3 --features component",
    );
    let out = common::dform()
        .args(common::on("p.df", &["--world", "w.json"], args))
        .env("DFORM_PROVIDER_FAKE", fake)
        .env("XDG_CACHE_HOME", wasm_cache())
        .env("DFORM_TRACE", s.path("run.jsonl"))
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    (common::Run::from(out), common::spans(&s.path("run.jsonl")))
}

/// A trace's spans by id, and each one's label: the span it is (a call
/// by its kind, `provider.call Read`; a request by its, `io.call http`),
/// else its name.
struct Tree<'a> {
    by_id: BTreeMap<&'a str, &'a Json>,
}

impl<'a> Tree<'a> {
    fn of(spans: &'a [Json]) -> Tree<'a> {
        Tree {
            by_id: spans
                .iter()
                .map(|s| (s["spanId"].as_str().unwrap(), s))
                .collect(),
        }
    }

    fn label(s: &Json) -> String {
        let name = s["name"].as_str().unwrap_or_default();
        let call = common::attr(s, "provider.call");
        let kind = common::attr(s, "kind").unwrap_or_default();
        match (s["kind"].as_u64(), call) {
            (Some(3), Some(call)) => format!("provider.call {call}"),
            (Some(3), None) => format!("io.call {kind}"),
            _ => name.to_string(),
        }
    }

    fn parent(&self, s: &Json) -> Option<&'a Json> {
        self.by_id.get(s["parentSpanId"].as_str()?).copied()
    }

    /// Each span's (its parent's label, its own), counted.
    fn edges(&self) -> BTreeMap<(String, String), usize> {
        let mut out = BTreeMap::new();
        for s in self.by_id.values() {
            let parent = self.parent(s).map_or("(root)".into(), Tree::label);
            *out.entry((parent, Tree::label(s))).or_default() += 1;
        }
        out
    }

    fn named(&self, name: &str) -> Vec<&'a Json> {
        self.by_id
            .values()
            .copied()
            .filter(|s| s["name"] == name)
            .collect()
    }
}

fn time(s: &Json, k: &str) -> u128 {
    s[k].as_str().unwrap().parse().unwrap()
}

/// A plan on the fake component: its start is compiled (or taken from
/// the cache), linked and instantiated under the provider's start, which
/// ends before the deployment is evaluated (it was the run's whole length
/// while the instance's thread stayed in it); every call is a client
/// span and no span is a server's: the call carries how long it took in
/// the provider, its status the answer's.
#[test]
fn a_plan_on_the_fake_component_traces_its_start_and_each_call() {
    let s = Scratch::new("host-trace-plan");
    s.write("p.df", PROG);
    let (r, spans) = traced(&s, &["plan"]);
    r.success();
    let tree = Tree::of(&spans);
    let edges = tree.edges();
    let start = "provider mock started";
    for (parent, child) in [
        (start, "wasm.compile"),
        (start, "wasm.link"),
        (start, "wasm.instantiate"),
        (start, "provider.call Handshake"),
        (
            "providers started and configured",
            "provider.call Configure",
        ),
        ("providers' schema loaded", "provider.call Schema"),
        ("Plan calls", "provider.call Plan"),
    ] {
        assert!(
            edges.contains_key(&(parent.to_string(), child.to_string())),
            "{parent} > {child} in\n{edges:#?}"
        );
    }
    for wasm in ["wasm.compile", "wasm.link", "wasm.instantiate"] {
        let under: BTreeSet<_> = edges
            .iter()
            .filter(|((_, c), _)| c == wasm)
            .map(|((p, _), n)| (p.as_str(), *n))
            .collect();
        assert_eq!(under, BTreeSet::from([(start, 1)]), "{wasm}");
    }
    let compiled = &tree.named("wasm.compile")[0];
    assert!(
        matches!(
            common::attr(compiled, "cached").as_deref(),
            Some("true" | "false")
        ),
        "{compiled}"
    );
    // Every call carries its time in the provider, answered; none is a
    // server span.
    assert!(spans.iter().all(|s| s["kind"] != 2), "a server span");
    let calls: Vec<&Json> = spans
        .iter()
        .filter(|s| Tree::label(s).starts_with("provider.call"))
        .collect();
    assert!(calls.len() >= 4, "{calls:#?}");
    for c in calls {
        let took = common::attr(c, "provider.took").map(|t| t.parse::<u64>());
        assert!(matches!(took, Some(Ok(_))), "{c}");
        assert_eq!(c["status"]["code"], 1, "{c}");
    }
    let started = &tree.named(start)[0];
    let evaluated = tree.named("evaluated");
    assert!(
        evaluated
            .iter()
            .all(|e| time(started, "endTimeUnixNano") <= time(e, "startTimeUnixNano")),
        "the provider's start ends once it is started"
    );
}

/// The component exits as it is called to Apply `compute.vm app`: that
/// call is an error whose type is the exit.
#[test]
fn a_trap_is_its_calls_error() {
    let s = Scratch::new("host-trace-crash");
    s.write("p.df", PROG);
    let (r, spans) = traced(&s, &["apply", "--chaos", "crash=compute.vm[\"app\"]"]);
    r.failure();
    let crashed: Vec<&Json> = spans
        .iter()
        .filter(|s| common::attr(s, "error.type").is_some())
        .collect();
    assert_eq!(crashed.len(), 1, "{crashed:#?}");
    let call = crashed[0];
    assert_eq!(common::attr(call, "error.type").as_deref(), Some("exit"));
    assert_eq!(
        common::attr(call, "addr").as_deref(),
        Some("compute.vm app")
    );
    assert_eq!(call["status"]["code"], 2, "{call}");
}

/// The OVH provider as a component: each request it sends through the
/// host is under its call (and so is its signer's fetch of the API's
/// clock, made as the Configure opens the keys), so a trace reads as the
/// native provider's (tests/host_wasm.rs's parity with the native build).
#[test]
fn the_ovh_components_requests_are_under_its_calls() {
    use dform_provider_ovh::fake::{self, Server};
    let wasm = built(
        "dform_provider_ovh.wasm",
        "cargo build -p dform-provider-ovh --release --target wasm32-wasip3 --features component",
    );
    let server = Server::start();
    let s = Scratch::project("host-trace-ovh");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{wasm}\", allow = \
             [\"wasi:filesystem\"], credentials = [\"signed:environment\"] }}\n"
        ),
    );
    s.write(
        "main.df",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"{}\" }}\n\n\
             resource ovh.ssh_key admin {{ name = \"lab-admin\", public_key = \"ssh-ed25519 \
             AAAAC3Nz lab\" }}\n",
            server.endpoint,
            fake::DESCRIPTION,
        ),
    );
    let mut plan = common::dform();
    plan.args(["plan", "main.df", "--trace", "run.jsonl"])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env("XDG_CACHE_HOME", wasm_cache())
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
    // The keys its signer signs with, granted from the environment: its
    // signer fetches the API's clock as the Configure opens them.
    for (k, v) in server.env() {
        plan.env(k, v);
    }
    let out = plan.output().unwrap();
    common::Run::from(out).success();
    let spans = common::spans(&s.path("run.jsonl"));
    let tree = Tree::of(&spans);
    let requests: Vec<&Json> = spans
        .iter()
        .filter(|s| common::attr(s, "kind").as_deref() == Some("http"))
        .collect();
    assert!(
        requests
            .iter()
            .any(|r| common::attr(r, "addr").is_some_and(|a| a.ends_with("/auth/time"))),
        "the signer's fetch of the API's clock: {requests:#?}"
    );
    for r in requests {
        let call = tree.parent(r).unwrap();
        assert_eq!(call["kind"], 3, "{r} under {call}");
        assert!(
            Tree::label(call).starts_with("provider.call"),
            "{r} under {call}"
        );
    }
    let edges = tree.edges();
    assert!(
        edges.contains_key(&("provider.call Configure".into(), "io.call http".into())),
        "{edges:#?}"
    );
}
