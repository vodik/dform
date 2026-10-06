//! Parallelism with a limit: `apply --parallel N` walks the tick's DAG with
//! at most N Apply calls in flight. The mock's `latency=` knob puts each
//! call on the executor's clock and the world records its span. Over gRPC
//! the calls in flight really run at once; the direct and wire backends
//! answer them in simulated time (`plugin::queue`), so the schedule is
//! exact there.

mod common;
use common::{BACKENDS, Backend, Scratch};

const PROG: &str = r#"

resource net.vpc a { cidr = "10.0.0.0/16" }
resource net.vpc b { cidr = "10.1.0.0/16" }
resource net.subnet s { vpc_id = ref(net.vpc, "a", "id"), tier = "web" }
use fake
"#;

fn apply(backend: Backend, parallel: &str) -> (common::Run, Vec<(String, u64, u64)>) {
    let s = Scratch::new("parallel");
    s.write("p.df", PROG);
    let r = s
        .run_on(
            backend,
            &[
                "dev",
                "--world",
                "w.json",
                "--chaos",
                "latency=net.vpc[\"a\"]:100",
                "--chaos",
                "latency=net.vpc[\"b\"]:100",
                "--chaos",
                "latency=net.subnet[\"s\"]:50",
                "apply",
                "--parallel",
                parallel,
                "p.df",
            ],
        )
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
    for backend in BACKENDS {
        let (r, spans) = apply(backend, "1");
        one_at_a_time(&r, &spans);
    }
}

fn one_at_a_time(r: &common::Run, spans: &[(String, u64, u64)]) {
    assert_eq!(
        spans,
        [
            span(r#"net.vpc["a"]"#, 0, 100),
            span(r#"net.vpc["b"]"#, 100, 200),
            span(r#"net.subnet["s"]"#, 200, 250),
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
    for backend in [Backend::Direct, Backend::Wire] {
        independent_creates_overlap_on(backend);
    }
}

fn independent_creates_overlap_on(backend: Backend) {
    let (r, spans) = apply(backend, "2");
    assert_eq!(
        spans,
        [
            span(r#"net.vpc["a"]"#, 0, 100),
            span(r#"net.vpc["b"]"#, 0, 100),
            span(r#"net.subnet["s"]"#, 100, 150),
        ]
    );
    assert!(
        r.stdout.contains("chaos: simulated apply time: 150ms"),
        "{}",
        r.stdout
    );
    // Output order does not depend on the limit.
    let (one, _) = apply(backend, "1");
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
    for backend in BACKENDS {
        let (_, spans) = apply(backend, "8");
        let a = spans.iter().find(|x| x.0 == r#"net.vpc["a"]"#).unwrap();
        let s = spans.iter().find(|x| x.0 == r#"net.subnet["s"]"#).unwrap();
        assert!(s.1 >= a.2, "{backend:?}: {spans:?}");
    }
}

#[test]
fn parallel_must_be_at_least_one() {
    let s = Scratch::new("parallel-zero");
    s.write("p.df", PROG);
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "apply",
            "--parallel",
            "0",
            "p.df",
        ])
        .failure();
    assert!(r.stderr.contains("--parallel"), "{}", r.stderr);
}

/// A failure stops new calls; the one already in flight finishes and keeps
/// its identity. On the simulated clock the failure takes no time, so it is
/// taken before the vpc's answer and the subnet never starts.
#[test]
fn a_failure_stops_new_calls() {
    for backend in BACKENDS {
        let s = Scratch::new("parallel-fail");
        s.write("p.df", PROG);
        let r = s
            .run_on(
                backend,
                &[
                    "dev",
                    "--world",
                    "w.json",
                    "--chaos",
                    "fail=net.vpc[\"b\"]",
                    "apply",
                    "--parallel",
                    "2",
                    "p.df",
                ],
            )
            .failure();
        assert!(
            r.stderr.contains("apply net.vpc[\"b\"]: injected failure"),
            "{backend:?}: {}",
            r.stderr
        );
        let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
        let keys: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
        match backend {
            // The calls run at once over gRPC: whether the subnet started
            // before the failure was taken depends on which answer came
            // first. The vpc it waits for keeps its identity either way.
            Backend::Process => {
                assert!(keys.contains(&&"net.vpc::a".to_string()), "{keys:?}");
                assert!(!keys.contains(&&"net.vpc::b".to_string()), "{keys:?}");
            }
            Backend::Direct | Backend::Wire => assert_eq!(keys, ["net.vpc::a"], "{backend:?}"),
        }
    }
}

/// Whatever order the calls in flight answer in (a seed picks it), the
/// apply ends where a serial one does, and a second apply has nothing to
/// do.
#[test]
fn any_answer_order_ends_in_the_same_world() {
    let world = |seed: Option<u64>, parallel: &str| {
        let s = Scratch::new("parallel-seed");
        s.write("p.df", PROG);
        let mut c = Backend::Direct.command();
        if let Some(seed) = seed {
            c.env("DFORM_SEED", seed.to_string());
        }
        let args = [
            "dev",
            "--world",
            "w.json",
            "apply",
            "--yes",
            "p.df",
            "--parallel",
        ];
        let out = c
            .args(args)
            .arg(parallel)
            .current_dir(&s.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let r = s.run_on(
            Backend::Direct,
            &["dev", "--world", "w.json", "apply", "p.df"],
        );
        assert!(
            r.stdout.ends_with("apply: nothing to do\n"),
            "seed {seed:?}: {}",
            r.stdout
        );
        let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
        w.as_object_mut().unwrap().remove("timeline");
        (w, s.read("w.state.json"))
    };
    let serial = world(None, "1");
    for seed in 0..8 {
        assert_eq!(world(Some(seed), "3"), serial, "seed {seed}");
    }
}
