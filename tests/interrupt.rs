//! Ctrl-C and SIGTERM ask dform to stop (R-137): no new Apply call, the
//! calls in flight awaited and their answers logged, then the run unwinds
//! (the lock released, the audit log's `apply_end` written, every
//! destructor run) and exits 128 + the signal. A second Ctrl-C quits at
//! once; a run started with SIGINT ignored keeps ignoring it; a signal at
//! the prompt applies nothing; a controller stops after its event.

mod common;
use common::Scratch;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

const PROG: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(main), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", PROG);
    s
}

/// `dform dev --world w.json --chaos C.. apply --yes p.df`, its stderr a
/// pipe (a line per change of state).
fn apply(s: &Scratch, chaos: &[&str]) -> std::process::Command {
    let mut mock = vec!["--world", "w.json"];
    for c in chaos {
        mock.extend(["--chaos", c]);
    }
    let mut c = common::dform();
    c.args(common::yes(&common::on("p.df", &mock, &["apply"])))
        .current_dir(&s.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn signal(child: &Child, sig: i32) {
    // SAFETY: a signal to the child this test spawned.
    unsafe {
        libc::kill(child.id() as i32, sig);
    }
}

/// Read `child`'s stderr until a line starts with `start`; the lines read,
/// and the rest of the stream on a thread.
fn until_line(
    child: &mut Child,
    start: &str,
) -> (Vec<String>, std::thread::JoinHandle<Vec<String>>) {
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = Vec::new();
    for l in lines.by_ref() {
        let l = l.unwrap();
        let done = l.starts_with(start);
        seen.push(l);
        if done {
            break;
        }
    }
    let rest = std::thread::spawn(move || lines.map_while(|l| l.ok()).collect());
    (seen, rest)
}

fn audit(s: &Scratch) -> Vec<serde_json::Value> {
    s.read("w.state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// SIGINT while the subnet's create runs (a 1s call): the create is
/// awaited and its answer kept, the vm never starts, the exit is 130 after
/// the unwind: `apply_end` says the apply stopped, interrupted, and the
/// lock is released. The next apply resumes and creates the vm.
#[test]
fn an_interrupt_stops_after_the_calls_in_flight_and_unwinds() {
    let s = project("int-unwind");
    let mut child = apply(&s, &["delay=net.subnet[\"a\"]:1000"])
        .spawn()
        .unwrap();
    let (mut seen, rest) = until_line(&mut child, "  + net.subnet a");
    std::thread::sleep(Duration::from_millis(200));
    signal(&child, libc::SIGINT);
    let out = child.wait_with_output().unwrap();
    seen.extend(rest.join().unwrap());
    let stderr = seen.join("\n");
    assert_eq!(out.status.code(), Some(130), "{stderr}");
    assert!(
        stderr.contains("Ctrl-C: stopping after the calls in flight; Ctrl-C again to quit now"),
        "{stderr}"
    );
    assert!(
        seen.last().unwrap() == "interrupted: the next apply resumes it",
        "{stderr}"
    );
    let st = common::replayed(&s, "w.state.json");
    let mapped: Vec<&String> = st["resources"].as_object().unwrap().keys().collect();
    assert_eq!(mapped, ["net.subnet::a", "net.vpc::main"], "{stderr}");
    let end = audit(&s)
        .into_iter()
        .rev()
        .find(|e| e["kind"] == "apply_end")
        .unwrap();
    assert_eq!(
        (
            end["result"].as_str(),
            end["why"].as_str(),
            end["signal"].as_str()
        ),
        (Some("stopped"), Some("interrupted"), Some("SIGINT")),
        "{end}"
    );
    assert!(!s.path("w.state.lock").exists(), "the lock is released");
    let r = common::mock(&s, &["apply"]).success();
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
    assert_eq!(common::identities(&s).len(), 3);
}

/// A second Ctrl-C while the first waits on a call quits at once: the
/// process ends by the signal, long before the 30s call would answer.
#[test]
fn a_second_interrupt_quits_at_once() {
    let s = project("int-twice");
    let mut child = apply(&s, &["delay=net.subnet[\"a\"]:30000"])
        .spawn()
        .unwrap();
    let (_, rest) = until_line(&mut child, "  + net.subnet a");
    let start = Instant::now();
    signal(&child, libc::SIGINT);
    std::thread::sleep(Duration::from_millis(300));
    signal(&child, libc::SIGINT);
    let out = child.wait_with_output().unwrap();
    let _ = rest.join();
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(out.status.signal(), Some(libc::SIGINT), "{out:?}");
    // The answers before the kill are in the log.
    let st = common::replayed(&s, "w.state.json");
    assert!(st["resources"].get("net.vpc::main").is_some(), "{st}");
}

/// A run started with SIGINT ignored (`nohup`, a background job) keeps
/// ignoring it: the apply runs to its end.
#[test]
fn a_run_started_with_sigint_ignored_still_ignores_it() {
    use std::os::unix::process::CommandExt;
    let s = project("int-ignored");
    let mut cmd = apply(&s, &["delay=net.subnet[\"a\"]:1000"]);
    // SAFETY: only `signal` between fork and exec: async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let (_, rest) = until_line(&mut child, "  + net.subnet a");
    signal(&child, libc::SIGINT);
    let out = child.wait_with_output().unwrap();
    let stderr = rest.join().unwrap().join("\n");
    assert!(out.status.success(), "{stderr}");
    assert_eq!(common::identities(&s).len(), 3);
}

/// SIGTERM to a controller between events: it stops, exits 143, and its
/// lock is not held.
#[test]
fn sigterm_stops_a_controller_after_its_event() {
    let s = project("int-controller");
    let child = common::dform()
        .args(["controller", "run", "--poll", "50", "p.df"])
        .current_dir(&s.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its first event applies the stack.
    let start = Instant::now();
    while !s.path("dform.state/p/state.json").exists() {
        assert!(start.elapsed() < Duration::from_secs(60), "no first event");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    signal(&child, libc::SIGTERM);
    let out = child.wait_with_output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(143), "{stdout}{stderr}");
    assert!(
        stdout.contains("controller p: SIGTERM: stopped"),
        "{stdout}"
    );
    assert!(!s.path("dform.state/p/state.lock").exists());
}

/// Ctrl-C at the prompt (a pty): nothing is applied, the exit is 130, and
/// the stack is not left locked.
#[test]
fn an_interrupt_at_the_prompt_applies_nothing() {
    use expectrl::{Eof, Expect, Session};
    let s = project("int-prompt");
    let mut cmd = common::dform();
    cmd.args(["dev", "--world", "w.json", "apply", "p.df"])
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(60)));
    p.expect("[y/N] ").unwrap();
    // Ctrl-C: the terminal's interrupt character.
    p.send("\x03").unwrap();
    p.expect(Eof).unwrap();
    match p.get_process().wait().unwrap() {
        expectrl::process::unix::WaitStatus::Exited(_, code) => assert_eq!(code, 130),
        other => panic!("{other:?}"),
    }
    assert!(!s.path("w.json").exists() || common::identities(&s).is_empty());
    assert!(!s.path("w.state.lock").exists());
}
