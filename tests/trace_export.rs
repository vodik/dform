//! A long run's trace reaches its reader whole (src/trace.rs): every
//! span the run ended is in the trace file, and at the collector however
//! slowly it answers, the run's own span, which ends last, included. An
//! apply of three ticks, the second a wait: each tick under the run, its
//! provider calls under it.

use crate::common;
use common::Scratch;

/// The database's endpoint gates the vpc: tick 1 makes the database,
/// tick 2 waits for the world to have its endpoint, tick 3 makes the vpc.
const PROG: &str = r#"

use fake
resource db.postgres d { size = 1 }
resource net.vpc v { cidr = "10.0.0.0/16" } where d.endpoint == "d.db.fake"
"#;

/// The endpoint is the world's after this many Reads.
const NOT_READY: &str = r#"not-ready=db.postgres["d"].endpoint:3"#;

/// `dform dev --world w.json apply --yes`, its trace to `run.trace.jsonl`
/// and, given one, to the collector at `endpoint` too, with the exporter's
/// `OTEL_BSP_*` settings `bsp`.
fn apply(s: &Scratch, endpoint: Option<&str>, bsp: &[(&str, &str)]) -> common::Run {
    let mut c = common::dform();
    c.args([
        "dev", "--world", "w.json", "apply", "--yes", "--chaos", NOT_READY,
    ])
    .args(["--trace", "run.trace.jsonl", "p.df"])
    .current_dir(&s.dir)
    .env("DFORM_WAIT_POLL_MS", "50")
    .env_remove("DFORM_TRACE")
    .env_remove("DFORM_LOG")
    .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
    .envs(bsp.iter().copied());
    if let Some(e) = endpoint {
        c.env("OTEL_EXPORTER_OTLP_ENDPOINT", e);
    }
    common::Run::from(c.output().unwrap())
}

/// A span as a reader links it: its id, its parent's (empty for a root),
/// its name, and its resource's `service.name`.
#[derive(Debug, Clone, Default)]
struct Span {
    id: String,
    parent: String,
    name: String,
    service: String,
}

/// The spans of the trace file.
fn file_spans(s: &Scratch) -> Vec<Span> {
    let mut out = Vec::new();
    for line in s.read("run.trace.jsonl").lines() {
        let req: serde_json::Value = serde_json::from_str(line).unwrap();
        for rs in req["resourceSpans"].as_array().unwrap() {
            let service = rs["resource"]["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "service.name")
                .and_then(|a| a["value"]["stringValue"].as_str())
                .unwrap_or_default();
            for ss in rs["scopeSpans"].as_array().unwrap() {
                for sp in ss["spans"].as_array().unwrap() {
                    out.push(Span {
                        id: sp["spanId"].as_str().unwrap().into(),
                        parent: sp["parentSpanId"].as_str().unwrap().into(),
                        name: sp["name"].as_str().unwrap().into(),
                        service: service.into(),
                    });
                }
            }
        }
    }
    out
}

/// The trace `spans` (from `from`) is whole: dform's, one root, the
/// run's span, and every other span's parent in it; ticks 1 to 3 under
/// the run, the first and the last with a provider's Apply under them,
/// the second with its wait.
fn assert_whole(spans: &[Span], from: &str) {
    assert!(
        spans.iter().all(|s| s.service == "dform"),
        "{from}: {spans:?}"
    );
    let by_id: std::collections::HashMap<&str, &Span> =
        spans.iter().map(|s| (s.id.as_str(), s)).collect();
    let orphans: Vec<&Span> = spans
        .iter()
        .filter(|s| !s.parent.is_empty() && !by_id.contains_key(s.parent.as_str()))
        .collect();
    assert!(
        orphans.is_empty(),
        "{from}: {} of {} spans without their parent: {orphans:?}",
        orphans.len(),
        spans.len()
    );
    let roots: Vec<&str> = spans
        .iter()
        .filter(|s| s.parent.is_empty())
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(roots, ["dform dev"], "{from}: the run's span, the one root");
    let under = |s: &Span, name: &str| {
        let mut at = s;
        while let Some(p) = by_id.get(at.parent.as_str()) {
            if p.name == name {
                return true;
            }
            at = p;
        }
        false
    };
    for (tick, call) in [
        ("tick 1", "fakecloud Apply"),
        ("tick 2", "wait, tick 2"),
        ("tick 3", "fakecloud Apply"),
    ] {
        let ticks: Vec<&Span> = spans.iter().filter(|s| s.name == tick).collect();
        assert_eq!(ticks.len(), 1, "{from}: {tick}");
        assert!(under(ticks[0], "dform dev"), "{from}: {tick} under the run");
        assert!(
            spans.iter().any(|s| s.name == call && under(s, tick)),
            "{from}: no {call:?} under {tick}"
        );
    }
}

/// The trace file holds every span of a three-tick apply, each under its
/// parent, the run's last.
#[test]
fn a_long_applys_trace_file_holds_every_span() {
    let s = Scratch::new("trace-file-whole");
    s.write("p.df", PROG);
    let r = apply(&s, None, &[]).success();
    assert!(r.stdout.contains("\ntick 3  1 change\n"), "{}", r.stdout);
    assert_whole(&file_spans(&s), "the trace file");
}

/// A collector that answers each export a second late gets the run's
/// last batch too: the run's span, its ticks and what is under them,
/// which end last and are sent as the run ends, in more exports than the
/// exporter's shutdown alone would wait for (eight spans an export,
/// none sent before the end). The trace file has them all as well.
#[cfg(feature = "otel")]
#[test]
fn a_slow_collector_gets_the_runs_last_batch() {
    let collector = otlp::Collector::start(std::time::Duration::from_secs(1));
    let s = Scratch::new("trace-collector-whole");
    s.write("p.df", PROG);
    let r = apply(
        &s,
        Some(&collector.endpoint()),
        &[
            ("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", "8"),
            ("OTEL_BSP_SCHEDULE_DELAY", "600000"),
        ],
    )
    .success();
    assert!(r.stdout.contains("\ntick 3  1 change\n"), "{}", r.stdout);
    let file = file_spans(&s);
    assert_whole(&file, "the trace file");
    let sent = collector.spans();
    assert_whole(&sent, "the collector");
    assert_eq!(sent.len(), file.len(), "the collector's spans, the file's");
    assert!(!r.stderr.contains("warning: the trace"), "{}", r.stderr);
}

/// A collector of OTLP/HTTP (protobuf, `POST /v1/traces`), which answers
/// each export late.
#[cfg(feature = "otel")]
mod otlp {
    use super::Span;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub struct Collector {
        at: std::net::SocketAddr,
        got: Arc<Mutex<Vec<Span>>>,
    }

    impl Collector {
        /// Listening, each export's spans kept as it arrives and answered
        /// `late`.
        pub fn start(late: Duration) -> Collector {
            let sink = TcpListener::bind("127.0.0.1:0").unwrap();
            let at = sink.local_addr().unwrap();
            let got = Arc::new(Mutex::new(Vec::new()));
            let kept = got.clone();
            std::thread::spawn(move || {
                for conn in sink.incoming().flatten() {
                    let kept = kept.clone();
                    std::thread::spawn(move || serve(conn, late, &kept));
                }
            });
            Collector { at, got }
        }

        pub fn endpoint(&self) -> String {
            format!("http://{}", self.at)
        }

        /// The spans received so far.
        pub fn spans(&self) -> Vec<Span> {
            self.got.lock().unwrap().clone()
        }
    }

    /// The exports of one connection (kept alive), until it closes.
    fn serve(conn: TcpStream, late: Duration, kept: &Mutex<Vec<Span>>) {
        let mut r = BufReader::new(conn.try_clone().unwrap());
        loop {
            let mut len = 0;
            loop {
                let mut l = String::new();
                if r.read_line(&mut l).unwrap_or(0) == 0 {
                    return;
                }
                if let Some(n) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = n.trim().parse().unwrap();
                }
                if l.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0; len];
            if r.read_exact(&mut body).is_err() {
                return;
            }
            kept.lock().unwrap().extend(spans(&body));
            std::thread::sleep(late);
            let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n";
            if (&conn).write_all(ok).is_err() {
                return;
            }
        }
    }

    /// An `ExportTraceServiceRequest`'s spans: `resource_spans` (1),
    /// each its `resource` (1) and their `scope_spans` (2), their `spans`
    /// (2), each its `span_id` (2), `parent_span_id` (4) and `name` (5).
    fn spans(body: &[u8]) -> Vec<Span> {
        let mut out = Vec::new();
        for rs in fields(body).filter(|(f, _)| *f == 1) {
            let service = fields(rs.1)
                .filter(|(f, _)| *f == 1)
                .find_map(|(_, r)| service(r))
                .unwrap_or_default();
            for ss in fields(rs.1).filter(|(f, _)| *f == 2) {
                for sp in fields(ss.1).filter(|(f, _)| *f == 2) {
                    let mut span = Span {
                        service: service.clone(),
                        ..Span::default()
                    };
                    for (f, v) in fields(sp.1) {
                        match f {
                            2 => span.id = hex(v),
                            4 => span.parent = hex(v),
                            5 => span.name = String::from_utf8(v.to_vec()).unwrap(),
                            _ => {}
                        }
                    }
                    out.push(span);
                }
            }
        }
        out
    }

    /// A `Resource`'s `service.name`: of its `attributes` (1), the
    /// `KeyValue` whose `key` (1) it is, its `value` (2)'s `string_value`
    /// (1).
    fn service(resource: &[u8]) -> Option<String> {
        fields(resource)
            .filter(|(f, _)| *f == 1)
            .find_map(|(_, kv)| {
                let mut kv = fields(kv);
                let key = kv.find(|(f, _)| *f == 1)?.1;
                let value = kv.find(|(f, _)| *f == 2)?.1;
                let s = fields(value).find(|(f, _)| *f == 1)?.1;
                (key == b"service.name").then(|| String::from_utf8(s.to_vec()).unwrap())
            })
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A protobuf message's length-delimited fields, by number; the
    /// others skipped.
    fn fields(mut b: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
        std::iter::from_fn(move || {
            loop {
                if b.is_empty() {
                    return None;
                }
                let key = varint(&mut b);
                let len = match key & 7 {
                    0 => {
                        varint(&mut b);
                        continue;
                    }
                    1 => 8,
                    5 => 4,
                    2 => varint(&mut b) as usize,
                    w => panic!("protobuf wire type {w}"),
                };
                let (v, rest) = b.split_at(len);
                b = rest;
                if key & 7 == 2 {
                    return Some((key >> 3, v));
                }
            }
        })
    }

    fn varint(b: &mut &[u8]) -> u64 {
        let mut v = 0;
        for shift in (0..64).step_by(7) {
            let byte = b[0];
            *b = &b[1..];
            v |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                break;
            }
        }
        v
    }
}
