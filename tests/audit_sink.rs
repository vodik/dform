//! The audit sink is bounded (R-141): it runs in a process group of its
//! own, gets `[defaults] audit_sink_timeout` per entry, and one that takes
//! longer is killed with everything `sh` started; the apply goes on with
//! a warning, its lock never held by a sink.

mod common;
use common::{Scratch, mock};

const PROG: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }
use fake
"#;

/// A sink that hangs (`sleep 30` started by `sh`, its pid written down)
/// with a 1s budget: the apply finishes in well under 30s, warns, and no
/// `sleep` is left running.
#[test]
fn a_sink_that_hangs_is_stopped_with_its_group() {
    let s = Scratch::project("audit-sink-hang");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[defaults]\naudit_sink_timeout = \"1s\"\n",
    );
    s.write("p.df", PROG);
    let sink = format!("sleep 30 & echo $! >> {}; wait", s.path("pids").display());
    let start = std::time::Instant::now();
    let r = mock(&s, &["--audit-sink", &sink, "apply"]).success();
    let took = start.elapsed();
    assert!(
        r.stderr
            .contains("did not finish in 1s; it was stopped; the local log has the entry"),
        "{}",
        r.stderr
    );
    let pids = s.read("pids");
    let n = pids.lines().count();
    assert!(n >= 1, "{pids}");
    // One budget per entry: well under one sleep, let alone one per entry.
    assert!(took.as_secs() < 30, "the apply took {took:?}");
    for pid in pids.lines() {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Gone, or a zombie nobody reaps (its parent `sh` was killed too).
        assert!(
            stat.is_empty() || stat.split(' ').nth(2) == Some("Z"),
            "sleep {pid} is still running: {stat}"
        );
    }
    mock(&s, &["log", "verify"]).success();
}
