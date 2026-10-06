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
