//! Where a run's time goes (`dform_core::timing`, src/trace.rs): its
//! spans to a trace file, `--trace FILE`, in every build; over OTLP to a
//! collector's endpoint in a build with the `otel` feature. A run that
//! asks for no trace is the run it was.

use crate::common;
use common::{Scratch, copy_dir, repo};

/// A copy of examples/shop.
fn shop() -> Scratch {
    let s = Scratch::in_target("timing", "shop");
    copy_dir(&repo().join("examples/shop"), &s.dir);
    s
}

fn plan(s: &Scratch, more: &[&str]) -> common::Run {
    let mut c = common::dform();
    c.args(["plan", "shop", "env=prod"])
        .args(more)
        .current_dir(&s.dir)
        .env_remove("DFORM_TRACE")
        .env_remove("DFORM_LOG")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
    common::Run::from(c.output().unwrap())
}

/// Without the `otel` feature a collector's endpoint in the environment
/// (as for any other program) changes nothing: no subscriber, the plan
/// as it was.
#[cfg(not(feature = "otel"))]
#[test]
fn an_endpoint_without_the_otel_feature_changes_nothing() {
    let s = shop();
    let plain = plan(&s, &[]).success();
    let mut c = common::dform();
    c.args(["plan", "shop", "env=prod"])
        .current_dir(&s.dir)
        .env_remove("DFORM_TRACE")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9");
    let with_endpoint = common::Run::from(c.output().unwrap()).success();
    assert_eq!(with_endpoint.stdout, plain.stdout);
    assert_eq!(with_endpoint.stderr, plain.stderr);
}

/// With the `otel` feature the spans go to the endpoint too, over
/// OTLP/HTTP (protobuf to /v1/traces), the trace file written beside it,
/// and the plan is as it was.
#[cfg(feature = "otel")]
#[test]
fn a_plan_sends_its_spans_to_the_endpoint() {
    use std::io::{BufRead, BufReader, Read, Write};
    let s = shop();
    let plain = plan(&s, &[]).success();
    let sink = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = sink.local_addr().unwrap();
    let got = std::thread::spawn(move || {
        let (conn, _) = sink.accept().unwrap();
        let mut r = BufReader::new(conn.try_clone().unwrap());
        let mut head = Vec::new();
        let mut len = 0;
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            if let Some(n) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                len = n.trim().parse().unwrap();
            }
            if l.trim().is_empty() {
                break;
            }
            head.push(l.trim().to_string());
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        (&conn)
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .unwrap();
        (head, body.len())
    });
    let mut c = common::dform();
    c.args(["plan", "shop", "env=prod", "--trace", "run.trace.jsonl"])
        .current_dir(&s.dir)
        .env_remove("DFORM_TRACE")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{at}"));
    let sent = common::Run::from(c.output().unwrap()).success();
    assert_eq!(sent.stdout, plain.stdout);
    let file = std::fs::read_to_string(s.path("run.trace.jsonl")).unwrap();
    assert!(file.contains("\"name\":\"fakecloud Plan\""), "{file}");
    let (head, n) = got.join().unwrap();
    assert!(head[0].starts_with("POST /v1/traces "), "{head:?}");
    assert!(
        head.iter()
            .any(|h| h.eq_ignore_ascii_case("content-type: application/x-protobuf")),
        "{head:?}"
    );
    assert!(n > 0);
}

/// The trace file: OTLP JSON lines, one trace under `dform plan`; the
/// phases, each provider call a client span with its provider, kind and
/// address; the Plan calls sent together wait on one span, a block; and
/// no attribute but the names, kinds and counts the spans are made of.
/// The plan printed is the plan without a trace.
#[test]
fn a_plan_writes_its_spans_to_the_trace_file() {
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};

    let s = shop();
    let plain = plan(&s, &[]).success();
    let traced = plan(&s, &["--trace", "run.trace.jsonl"]).success();
    assert_eq!(traced.stdout, plain.stdout);
    assert_eq!(traced.stderr, plain.stderr);

    let text = std::fs::read_to_string(s.path("run.trace.jsonl")).unwrap();
    let mut spans: Vec<Value> = Vec::new();
    for line in text.lines() {
        let req: Value = serde_json::from_str(line).unwrap();
        for rs in req["resourceSpans"].as_array().unwrap() {
            for ss in rs["scopeSpans"].as_array().unwrap() {
                spans.extend(ss["spans"].as_array().unwrap().iter().cloned());
            }
        }
    }
    let attr = |s: &Value, k: &str| -> Option<String> {
        s["attributes"]
            .as_array()?
            .iter()
            .find(|a| a["key"] == k)
            .map(|a| {
                let v = &a["value"];
                v["stringValue"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| v["intValue"].as_str().map(String::from))
                    .unwrap_or_default()
            })
    };
    let traces: BTreeSet<&str> = spans.iter().filter_map(|s| s["traceId"].as_str()).collect();
    assert_eq!(traces.len(), 1, "one run, one trace");
    let names: BTreeSet<&str> = spans.iter().filter_map(|s| s["name"].as_str()).collect();
    for want in [
        "dform plan",
        "loaded and compiled",
        "located the deployment",
        "providers started and configured",
        "evaluated",
        "evaluation, round 1",
        "policy pass",
        "fakecloud Configure",
        "fakecloud Plan",
    ] {
        assert!(names.contains(want), "no span {want:?} in {names:?}");
    }
    // The attributes a span may carry: names, kinds, counts.
    let allowed: BTreeSet<&str> = [
        "seq",
        "waited_on",
        "provider",
        "kind",
        "addr",
        "tick",
        "count",
        "command",
    ]
    .into();
    for sp in &spans {
        for a in sp["attributes"].as_array().unwrap() {
            let k = a["key"].as_str().unwrap();
            assert!(allowed.contains(k), "attribute {k} on {}", sp["name"]);
        }
    }
    // Each Plan call a client span (3) of its provider, kind and address;
    // sent together, they wait on one span.
    let plans: Vec<&Value> = spans
        .iter()
        .filter(|s| s["name"] == "fakecloud Plan")
        .collect();
    assert_eq!(plans.len(), 12);
    let mut waited: BTreeMap<String, usize> = BTreeMap::new();
    for p in &plans {
        assert_eq!(p["kind"], 3, "a client span");
        assert_eq!(attr(p, "provider").as_deref(), Some("fakecloud"));
        assert_eq!(attr(p, "kind").as_deref(), Some("Plan"));
        assert!(attr(p, "addr").is_some_and(|a| a.starts_with("aws.")));
        *waited.entry(attr(p, "waited_on").unwrap()).or_default() += 1;
    }
    assert_eq!(waited.len(), 1, "the Plan calls are a block: {waited:?}");
    // What they waited on is a span of the run, ended before they began.
    let (w, _) = waited.into_iter().next().unwrap();
    let before = spans
        .iter()
        .find(|s| attr(s, "seq").as_deref() == Some(w.as_str()))
        .expect("the span waited on");
    let end: u128 = before["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    for p in &plans {
        let start: u128 = p["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert!(
            end <= start,
            "{} ended after a Plan call began",
            before["name"]
        );
    }
}
