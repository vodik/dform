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

/// The kinds of the entries a `cat >> sink.jsonl` sink got.
fn sunk(s: &Scratch) -> std::collections::BTreeSet<String> {
    s.read("sink.jsonl")
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// The sink gets the entries it did before the log became the state's
/// (R-146): not `state` nor `lease`, one more per Apply call, unless
/// `[defaults] audit_sink_entries = "all"`.
#[test]
fn a_sink_gets_the_states_entries_only_when_asked() {
    let s = Scratch::project("audit-sink-entries");
    s.write("p.df", PROG);
    let sink = format!("cat >> {}", s.path("sink.jsonl").display());
    mock(&s, &["--audit-sink", &sink, "apply"]).success();
    let kinds = sunk(&s);
    assert!(
        kinds.contains("action") && kinds.contains("apply_end"),
        "{kinds:?}"
    );
    assert!(!kinds.contains("state"), "{kinds:?}");
    let all = Scratch::project("audit-sink-entries-all");
    all.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[defaults]\naudit_sink_entries = \"all\"\n",
    );
    all.write("p.df", PROG);
    let sink = format!("cat >> {}", all.path("sink.jsonl").display());
    mock(&all, &["--audit-sink", &sink, "apply"]).success();
    assert!(sunk(&all).contains("state"), "{:?}", sunk(&all));
    all.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[defaults]\naudit_sink_entries = \"some\"\n",
    );
    let r = mock(&all, &["plan"]).failure();
    assert!(
        r.stderr.contains("audit_sink_entries = \"some\": `audit`"),
        "{}",
        r.stderr
    );
}
