//! State is a checkpoint of the audit log (R-146). Every change of state
//! is appended to the log, durably, before anything after it; the state
//! file is written whole (a temporary, fsynced, renamed over it) per tick;
//! a run replays the log's entries after the checkpoint. Killed (kill -9,
//! `DFORM_TEST_ABORT_AT`) between an append and the checkpoint, mid-tick,
//! or between a checkpoint's temporary and its rename, the next run
//! recovers to the state an uninterrupted apply leaves. The fake S3
//! store's case is in tests/s3.rs.

mod common;
use common::{Scratch, mock};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

/// `apply` killed at `at` (`POINT:N`): its output.
fn killed_at(s: &Scratch, at: &str) -> common::Run {
    let args = common::yes(&common::on("p.df", &["--world", "w.json"], &["apply"]));
    let out = common::dform()
        .args(args)
        .current_dir(&s.dir)
        .env("DFORM_TEST_ABORT_AT", at)
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(9), "{at}: {out:?}");
    common::Run::from(out)
}

/// The state and the world's objects an apply leaves.
fn outcome(s: &Scratch) -> (serde_json::Value, serde_json::Value) {
    let world = s.json("w.json");
    (
        common::replayed(s, "w.state.json"),
        world["resources"].clone(),
    )
}

/// The temporaries of a write left in the directory.
fn temporaries(s: &Scratch) -> Vec<String> {
    std::fs::read_dir(&s.dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.') && n.ends_with(".tmp"))
        .collect()
}

#[test]
fn a_run_killed_anywhere_in_a_write_recovers_to_the_same_state() {
    let base = Scratch::project("wal-base");
    base.write("p.df", PROG);
    mock(&base, &["apply"]).success();
    let want = outcome(&base);
    assert_eq!(want.0["resources"].as_object().unwrap().len(), 3);
    // `logged:1` is the tick's in-flight record, logged before its first
    // checkpoint (there is no state file yet); 2 to 4 each call's answer,
    // mid-tick; 5 the apply's end. `renamed:1` the tick's first
    // checkpoint, 2 its last, 3 the apply's end.
    for at in [
        "logged:1",
        "logged:2",
        "logged:3",
        "logged:4",
        "logged:5",
        "renamed:1",
        "renamed:2",
        "renamed:3",
    ] {
        let s = Scratch::project("wal-killed");
        s.write("p.df", PROG);
        killed_at(&s, at);
        // Whatever was killed, the state file is whole, or not there.
        if s.path("w.state.json").exists() {
            s.json("w.state.json");
        }
        mock(&s, &["apply"]).success();
        assert_eq!(outcome(&s), want, "{at}");
        assert!(temporaries(&s).is_empty(), "{at}: {:?}", temporaries(&s));
        let r = mock(&s, &["plan"]).success();
        assert_eq!(r.summary(), "stack p is up to date", "{at}: {}", r.stdout);
        mock(&s, &["log", "verify"]).success();
    }
}

/// Killed after a call's answer is logged and before anything else: the
/// state file does not have it, a run does (the log's entry after the
/// checkpoint is replayed), and `state show --from-log` rebuilds it from
/// the log alone.
#[test]
fn the_log_has_what_the_checkpoint_does_not() {
    let s = Scratch::project("wal-replay");
    s.write("p.df", PROG);
    let out = common::dform()
        .args(common::yes(&["apply", "p.df"]))
        .current_dir(&s.dir)
        .env("DFORM_TEST_ABORT_AT", "logged:3")
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(9), "{out:?}");
    let checkpoint = s.json("dform.state/p/state.json");
    assert_eq!(
        checkpoint["resources"].as_object().map_or(0, |r| r.len()),
        0,
        "{checkpoint}"
    );
    let st = common::replayed(&s, "dform.state/p/state.json");
    let mapped: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    assert_eq!(mapped, ["net.subnet::a", "net.vpc::main"]);
    let r = s.run(&["state", "show", "--from-log", "p.df"]).success();
    assert!(r.stdout.contains("net.subnet[\"a\"]"), "{}", r.stdout);
    assert!(
        r.stdout.contains("rebuilt from the log alone"),
        "{}",
        r.stdout
    );
    let r = s.run(&["state", "show", "p.df"]).success();
    assert!(r.stdout.contains("net.subnet[\"a\"]"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("an apply was interrupted: the next apply resumes it"),
        "{}",
        r.stdout
    );
}

/// A line an append was cut short in (a crash mid-write) is skipped by the
/// replay, and the next entry starts a line of its own.
#[test]
fn a_cut_last_line_is_skipped() {
    let s = Scratch::project("wal-cut");
    s.write("p.df", PROG);
    killed_at(&s, "logged:3");
    let log = s.read("w.state.audit.jsonl");
    s.write(
        "w.state.audit.jsonl",
        &format!("{log}{{\"seq\":99,\"kind\":\"sta"),
    );
    let st = common::replayed(&s, "w.state.json");
    assert_eq!(st["resources"].as_object().unwrap().len(), 2);
    mock(&s, &["apply"]).success();
    let st = common::replayed(&s, "w.state.json");
    assert_eq!(st["resources"].as_object().unwrap().len(), 3);
    // The cut line is the one broken link `log verify` names.
    let r = mock(&s, &["log", "verify"]).failure();
    assert!(r.stderr.contains("is not a JSON object"), "{}", r.stderr);
}

/// No state file, the plan key or the stack registry is written by
/// truncating it first (R-138): the state's writes are whole, through
/// `store::write_atomic`.
#[test]
fn no_state_path_is_written_by_truncating_it() {
    let root = common::repo().join("crates/dform-core/src");
    for f in [
        "store.rs", "state.rs", "stack.rs", "zset.rs", "audit.rs", "wal.rs",
    ] {
        let text = std::fs::read_to_string(root.join(f)).unwrap();
        // The module's own code: its tests may write fixtures.
        let code = text.split("#[cfg(test)]").next().unwrap();
        let writes: Vec<&str> = code
            .lines()
            .filter(|l| l.contains("fs::write(") && !l.trim_start().starts_with("//"))
            .collect();
        // The stall hook's marker file is a test's, not state.
        let writes: Vec<&&str> = writes
            .iter()
            .filter(|l| !l.contains("\"stalled\""))
            .collect();
        assert!(writes.is_empty(), "{f}: {writes:?}");
    }
}
