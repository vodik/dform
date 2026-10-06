//! Timeouts and retries (R-81): every provider call has a timeout
//! (`[providers.NAME] timeout`, 60s by default), and a call that failed in
//! a way worth trying again is retried with backoff up to a budget, after
//! which the apply stops naming the resource and the last error.

mod common;
use common::{BACKENDS, Backend, Scratch};

const PROG: &str = r#"
provider fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(main), cidr = "10.0.1.0/24" }
"#;

/// A project whose mock answers within `timeout`, retried `retries`
/// times 10ms apart.
fn project(name: &str, timeout: &str, retries: u32) -> Scratch {
    let s = Scratch::new(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = {{ source = \"fake\", \
             timeout = \"{timeout}\", retries = {retries}, backoff = \"10ms\" }}\n"
        ),
    );
    s.write("p.df", PROG);
    s
}

fn apply_on(s: &Scratch, backend: Backend, chaos: &[&str]) -> common::Run {
    let mut mock = vec!["--world", "w.json"];
    for c in chaos {
        mock.extend(["--chaos", c]);
    }
    s.run_on(backend, &common::on("p.df", &mock, &["apply"]))
}

/// A call past `[providers.NAME] timeout` is taken as timed out, on every
/// backend: the message names the call and the timeout.
#[test]
fn a_call_past_its_timeout_times_out() {
    for backend in BACKENDS {
        let s = project("timeout", "300ms", 0);
        let r = apply_on(&s, backend, &["delay=net.subnet[\"a\"]:500"]).failure();
        assert!(
            r.stderr.contains(
                "Error: apply net.subnet[\"a\"]: the provider fakecloud did not answer the \
                 Apply net.subnet[\"a\"] call within 300ms (its timeout); the call may have \
                 taken effect\n"
            ),
            "{backend:?}: {}",
            r.stderr
        );
    }
}

/// The `retry` entries of the audit log beside `w.json`.
fn retries(s: &Scratch) -> Vec<serde_json::Value> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "retry")
        .collect()
}

/// A refusal that says it is transient (503) changed nothing: the Apply is
/// sent again after its backoff, each retry a progress line and an audit
/// entry, and the apply completes.
#[test]
fn a_transient_refusal_is_retried_with_backoff() {
    for backend in BACKENDS {
        let s = project("flaky", "60s", 3);
        let r = apply_on(&s, backend, &["flaky=net.subnet[\"a\"]:2"]).success();
        assert!(
            r.stderr
                .contains("retrying the Apply net.subnet[\"a\"] call in ")
                && r.stderr.contains(
                    "(retry 2 of 3): apply net.subnet[\"a\"]: Service Unavailable (503) \
                 (chaos flaky=net.subnet[\"a\"], 2 of 2)\n"
                ),
            "{backend:?}: {}",
            r.stderr
        );
        assert!(r.stdout.contains("apply: complete"), "{}", r.stdout);
        let logged = retries(&s);
        assert_eq!(logged.len(), 2, "{backend:?}: {logged:?}");
        assert_eq!(logged[1]["call"], "Apply net.subnet[\"a\"]");
        assert_eq!(logged[1]["attempt"], 2);
        assert_eq!(logged[1]["of"], 3);
        assert_eq!(logged[1]["provider"], "fakecloud");
        assert!(logged[0]["delay_ms"].as_u64().unwrap() <= 10, "{logged:?}");
    }
}

/// Past its budget the apply stops naming the resource and the last
/// error; what answered before it is in state.
#[test]
fn past_the_retry_budget_the_apply_stops_naming_the_resource() {
    let s = project("flaky-budget", "60s", 2);
    let r = apply_on(&s, Backend::Process, &["flaky=net.subnet[\"a\"]:5"]).failure();
    assert!(
        r.stderr.contains(
            "Error: apply net.subnet[\"a\"]: Service Unavailable (503) (chaos \
             flaky=net.subnet[\"a\"], 3 of 5) (gave up after 2 retries)\n"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(retries(&s).len(), 2);
    let state = s.json("w.state.json");
    assert!(state["resources"].get("net.vpc::main").is_some(), "{state}");
    assert!(state["resources"].get("net.subnet::a").is_none(), "{state}");
    // The next apply finishes it.
    apply_on(&s, Backend::Process, &[]).success();
}

/// A refusal that is not transient is not retried.
#[test]
fn a_refusal_that_is_not_transient_is_not_retried() {
    let s = project("fail-final", "60s", 3);
    apply_on(&s, Backend::Process, &["fail=net.subnet[\"a\"]"]).failure();
    assert!(retries(&s).is_empty());
}
