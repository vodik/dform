//! R-127: apply's progress is the tick's block filling in, on stderr. Not
//! a terminal (these tests): a line per change of state, a change's mark
//! and address as it starts and with its time once it answered, a
//! heartbeat line per running change, the tick's end line; a failure
//! stops the tick, its line marked `!` with its time, its error below the
//! block; `-q`
//! only the end; Ctrl-C stops after the change in flight, what never
//! started `interrupted`, and the next apply resumes it.

mod common;
use common::Scratch;
use std::time::Duration;

const PROG: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(main), cidr = "10.0.1.0/24" }
resource net.subnet b { vpc_id = ref(main), cidr = "10.0.2.0/24" }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\n",
    );
    s.write("p.df", PROG);
    s
}

/// `dform dev --world w.json [--chaos C].. apply [EXTRA..] p.df`.
fn apply(s: &Scratch, chaos: &[&str], extra: &[&str]) -> std::process::Command {
    let mut mock = vec!["--world", "w.json"];
    for c in chaos {
        mock.extend(["--chaos", c]);
    }
    let mut args = vec!["apply"];
    args.extend_from_slice(extra);
    let mut c = common::dform();
    c.args(common::on("p.df", &mock, &args))
        .current_dir(s.path(""));
    c
}

/// The progress lines of `stderr`, each time `T`: what a run prints is
/// the same line for line, whatever the machine's speed.
fn block(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|l| !l.starts_with("chaos: ") && !l.starts_with("Error: "))
        .map(|l| {
            l.split(' ')
                .map(|w| {
                    let digits = w.trim_end_matches('s').replace(['.', 'm', 'h'], "");
                    match w.ends_with('s')
                        && !digits.is_empty()
                        && digits.chars().all(|c| c.is_ascii_digit())
                    {
                        true => "T",
                        false => w,
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// A slow create counts up, the creates after it wait for it; the first
/// failure stops the tick with its error inline.
#[test]
fn a_tick_says_each_change_of_state_and_stops_at_a_failure() {
    let s = project("progress-fail");
    let out = apply(
        &s,
        &["delay=net.subnet[\"a\"]:600", "fail=net.subnet[\"b\"]"],
        &["--yes"],
    )
    .output()
    .unwrap();
    let r = common::Run::from(out).failure();
    assert_eq!(
        block(&r.stderr),
        [
            "tick 1  3 changes",
            "  + net.vpc main",
            "  + net.vpc main  T",
            "  + net.subnet a",
            // The mock says it made the object and answers late (R-130).
            "  + net.subnet a  T  made",
            "  + net.subnet a  T  made",
            "  + net.subnet b",
            // The failure's mark and time; its error once, below the
            // block, in full (R-109).
            "  ! net.subnet b  T",
            "tick 1  failed  T",
            "! apply net.subnet b: refused, nothing changed",
            "    injected failure (chaos fail=net.subnet b)",
            "    p.df:5",
        ],
        "{}",
        r.stderr
    );
    // The slow one took its time, and the tick at least that.
    let took = r
        .stderr
        .lines()
        .rfind(|l| l.starts_with("  + net.subnet a  "))
        .unwrap();
    assert!(!took.contains(" 0.0s") && !took.contains(" 0.1s"), "{took}");
    // Stdout keeps the plan, no progress.
    assert!(!r.stdout.contains("tick 1  failed"), "{}", r.stdout);
}

/// A running change says so again every heartbeat (`DFORM_HEARTBEAT_MS`;
/// 30s by default); `-q` prints the tick's end alone.
#[test]
fn a_running_change_beats_and_quiet_says_only_the_end() {
    let s = project("progress-beat");
    let out = apply(&s, &["delay=net.subnet[\"a\"]:900"], &["--yes"])
        .env("DFORM_HEARTBEAT_MS", "200")
        .output()
        .unwrap();
    let r = common::Run::from(out).success();
    let beats = r
        .stderr
        .lines()
        .filter(|l| l.starts_with("  + net.subnet a  "))
        .count();
    assert!(beats >= 3, "{}", r.stderr);
    assert!(
        block(&r.stderr).contains(&"tick 1  done  T".to_string()),
        "{}",
        r.stderr
    );

    let s = project("progress-quiet");
    let out = apply(&s, &[], &["--yes", "-q"]).output().unwrap();
    let r = common::Run::from(out).success();
    assert_eq!(block(&r.stderr), ["tick 1  done  T"], "{}", r.stderr);
}

/// Ctrl-C while a create runs: said, the create awaited, what never
/// started `interrupted`, the tick's end, what to do; the next apply
/// resumes the tick.
#[test]
fn an_interrupt_says_what_ran_and_the_next_apply_resumes() {
    let s = project("progress-int");
    let child = apply(&s, &["delay=net.subnet[\"a\"]:3000"], &["--yes"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait for the slow create to start.
    let started = std::time::Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        // The vpc's answer is in the log (R-146): the subnet's create runs.
        let log = std::fs::read_to_string(s.path("w.state.audit.jsonl")).unwrap_or_default();
        if log.contains("\"address\":\"net.vpc[\\\"main\\\"]\"")
            || started.elapsed() > Duration::from_secs(20)
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(400));
    // SAFETY: a signal to the child this test spawned.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{stderr}");
    let lines = block(&stderr);
    // The create in flight is awaited (R-137): it answers; what never
    // started is interrupted.
    assert!(
        lines.contains(
            &"Ctrl-C: stopping after the calls in flight; Ctrl-C again to quit now".to_string()
        ) && lines.contains(&"  + net.subnet a  T  made".to_string())
            && lines.contains(&"  + net.subnet b    interrupted".to_string())
            && lines.contains(&"tick 1  interrupted  T".to_string())
            && lines.last().unwrap() == "interrupted: the next apply resumes it",
        "{stderr}"
    );
    let r = common::Run::from(apply(&s, &[], &["--yes"]).output().unwrap()).success();
    assert!(
        !r.stdout.contains("apply: complete"),
        "{}{}",
        r.stdout,
        r.stderr
    );
}
