//! Parallelism with a limit: `apply --parallel N` walks the tick's DAG with
//! at most N Apply calls in flight. The mock's `latency=` knob puts each
//! call on a simulated clock and the world records its span.

mod common;
use common::Scratch;

const PROG: &str = r#"edition 2026

resource net.vpc a { cidr = "10.0.0.0/16" }
resource net.vpc b { cidr = "10.1.0.0/16" }
resource net.subnet s { vpc_id = ref(net.vpc, "a", "id"), tier = "web" }
"#;

fn apply(parallel: &str) -> (common::Run, Vec<(String, u64, u64)>) {
    let s = Scratch::new("parallel");
    s.write("p.df", PROG);
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--world",
            "w.json",
            "apply",
            "--parallel",
            parallel,
            "--chaos",
            "latency=net.vpc/a:100",
            "--chaos",
            "latency=net.vpc/b:100",
            "--chaos",
            "latency=net.subnet/s:50",
        ])
        .success();
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let spans = w["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["addr"].as_str().unwrap().to_string(),
                x["start_ms"].as_u64().unwrap(),
                x["end_ms"].as_u64().unwrap(),
            )
        })
        .collect();
    (r, spans)
}

fn span(a: &str, start: u64, end: u64) -> (String, u64, u64) {
    (a.to_string(), start, end)
}

#[test]
fn one_at_a_time_by_default() {
    let (r, spans) = apply("1");
    assert_eq!(
        spans,
        [
            span("net.vpc/a", 0, 100),
            span("net.vpc/b", 100, 200),
            span("net.subnet/s", 200, 250),
        ]
    );
    assert!(
        r.stdout.contains("chaos: simulated apply time: 250ms"),
        "{}",
        r.stdout
    );
}

/// The two vpcs overlap; the subnet waits for its vpc, not for the other.
#[test]
fn independent_creates_overlap() {
    let (r, spans) = apply("2");
    assert_eq!(
        spans,
        [
            span("net.vpc/a", 0, 100),
            span("net.vpc/b", 0, 100),
            span("net.subnet/s", 100, 150),
        ]
    );
    assert!(
        r.stdout.contains("chaos: simulated apply time: 150ms"),
        "{}",
        r.stdout
    );
    // Output order does not depend on the limit.
    let (one, _) = apply("1");
    let strip = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("chaos: simulated"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&r.stdout), strip(&one.stdout));
}

/// A dependency is never overlapped, whatever the limit.
#[test]
fn a_dependent_waits_for_its_dependency() {
    let (_, spans) = apply("8");
    let a = spans.iter().find(|x| x.0 == "net.vpc/a").unwrap();
    let s = spans.iter().find(|x| x.0 == "net.subnet/s").unwrap();
    assert!(s.1 >= a.2, "{spans:?}");
}

#[test]
fn parallel_must_be_at_least_one() {
    let s = Scratch::new("parallel-zero");
    s.write("p.df", PROG);
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--world",
            "w.json",
            "apply",
            "--parallel",
            "0",
        ])
        .failure();
    assert!(r.stderr.contains("--parallel"), "{}", r.stderr);
}

/// A failure stops new calls; the one already in flight finishes and keeps
/// its identity.
#[test]
fn a_failure_stops_new_calls() {
    let s = Scratch::new("parallel-fail");
    s.write("p.df", PROG);
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--world",
            "w.json",
            "apply",
            "--parallel",
            "2",
            "--chaos",
            "fail=net.vpc/b",
        ])
        .failure();
    assert!(
        r.stderr.contains("apply net.vpc/b: injected failure"),
        "{}",
        r.stderr
    );
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    assert_eq!(
        st["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["net.vpc::a"]
    );
}
