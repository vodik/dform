//! R-140: a provider's process is owned, and every wait on it has a
//! budget. A link dropped while a call to its provider is stuck stops the
//! process (and reaps it: no zombie); a provider whose handshake names an
//! address nobody listens on is killed and reaped when the dial fails; one
//! that never answers its `Manifest` fails startup within the budget.

mod common;
use common::Scratch;
use dform::plugin::backend::CallError;
use dform::plugin::pb;
use dform::plugin::policy::Policy;
use dform::plugin::wire;
use dform_grpc::client::Conn;
use dform_grpc::spawn::{Env, Program};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

/// Whether the process `pid` still exists, a zombie included (`/proc`
/// keeps a zombie's entry until its parent reaps it).
fn exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Wait up to `budget` for `pid` to be gone and reaped: a bound on a
/// process that is never reaped, not a speed.
fn gone_within(pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while exists(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// The pid of this process's child whose environment carries `tag`.
fn child_tagged(tag: &str) -> Option<u32> {
    let me = std::process::id().to_string();
    let want = format!("DFORM_TEST_TAG={tag}");
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `PID (COMM) STATE PPID ..`: COMM may hold spaces, so from the
        // last `)`.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().nth(1));
        if ppid != Some(me.as_str()) {
            continue;
        }
        let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        if env.split(|&b| b == 0).any(|v| v == want.as_bytes()) {
            return Some(pid);
        }
    }
    None
}

/// `sh -c SCRIPT`, which writes its pid to `pidfile` first: a "provider"
/// that ignores its stdin, as a misbehaving one may.
fn shell(script: &str, pidfile: &Path) -> Program {
    Program {
        exe: "sh".into(),
        args: vec![
            "-c".into(),
            format!("echo $$ > '{}'; {script}", pidfile.display()).into(),
        ],
    }
}

fn pid_in(pidfile: &Path) -> u32 {
    std::fs::read_to_string(pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A loopback port nobody listens on.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// The mock provider's Apply of `net.vpc a` answers 30s late (chaos
/// `delay`), past the link's 200ms timeout: it answers `MaybeApplied`.
/// Dropping the link then stops the provider's process: gone, and reaped,
/// within 1s (before R-140 the thread holding it was let go, and the
/// process ran until its 30s were up, or forever).
#[test]
fn a_link_dropped_with_a_call_stuck_stops_its_provider() {
    let s = Scratch::new("provider-lifetimes-stuck");
    let tag = format!("stuck-{}", std::process::id());
    let mut link = Conn::link(
        &Program::exe(common::exe("dform-provider-fake")),
        &Env::default().set("DFORM_TEST_TAG", &tag),
    )
    .unwrap();
    let pid = child_tagged(&tag).expect("the provider's process");
    link.set_policy(Policy {
        timeout: Duration::from_millis(200),
        ..link.policy()
    });
    let config = json!({
        "world": s.path("w.json").display().to_string(),
        "inventory": s.path("i.json").display().to_string(),
        "chaos": ["delay=net.vpc[\"a\"]:30000"],
    });
    let _: pb::ConfigureResponse = link
        .call(pb::ConfigureRequest {
            config: Some(wire::doc(&config)),
        })
        .unwrap();
    let t = link.submit(pb::ApplyRequest {
        op: pb::Op::Create as i32,
        r#type: "net.vpc".into(),
        name: "a".into(),
        config: Some(wire::doc(&json!({ "cidr": "10.0.0.0/16" }))),
        idempotency_key: "k".into(),
        ..Default::default()
    });
    let r = link.wait(t);
    assert!(
        matches!(&r, Err(CallError::MaybeApplied(m)) if m.contains("within 200ms")),
        "{r:?}"
    );
    assert!(exists(pid), "the provider is in its late call");
    let from = Instant::now();
    drop(link);
    assert!(
        gone_within(pid, Duration::from_secs(10)),
        "the provider (pid {pid}) is still there {:?} after its link was dropped",
        from.elapsed()
    );
}

/// A provider whose handshake names an address nobody listens on: the
/// dial fails, and the process (which ignores its stdin) is killed and
/// reaped (before R-140 it was dropped as a bare `Child`, left running).
#[test]
fn a_failed_dial_leaves_no_provider_running() {
    let s = Scratch::new("provider-lifetimes-dial");
    let pidfile = s.path("pid");
    let port = closed_port();
    let program = shell(
        &format!("echo 'dform-provider|1|tcp://127.0.0.1:{port}'; exec sleep 300"),
        &pidfile,
    );
    let e = Conn::start(&program, &Env::default())
        .err()
        .expect("nothing listens there");
    assert!(format!("{e:#}").contains("dial provider"), "{e:#}");
    let pid = pid_in(&pidfile);
    assert!(
        gone_within(pid, Duration::from_secs(10)),
        "the provider (pid {pid}) outlived its failed dial"
    );
}

/// A provider that accepts the connection and never answers its
/// `Manifest` fails startup within the budget, rather than stopping dform
/// there (before R-140 it waited without a limit); the process goes with
/// the connection.
#[test]
fn a_manifest_never_answered_fails_within_its_budget() {
    let s = Scratch::new("provider-lifetimes-manifest");
    let pidfile = s.path("pid");
    // Takes connections and says nothing on them.
    let mute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = mute.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in mute.incoming().flatten() {
            held.push(c);
        }
    });
    let program = shell(
        &format!("echo 'dform-provider|1|tcp://127.0.0.1:{port}'; exec sleep 300"),
        &pidfile,
    );
    let mut conn = Conn::start(&program, &Env::default()).unwrap();
    let pid = pid_in(&pidfile);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = conn.manifest_within(Duration::from_millis(300));
        let _ = tx.send(r.map_err(|e| format!("{e:#}")));
        drop(conn);
    });
    // The budget is the call's: the error says it ran out, never a clock
    // here (10s bounds only a call that would wait for ever).
    let r = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the Manifest call ends within its budget");
    let e = r.expect_err("no answer");
    assert!(e.contains("did not answer its Manifest in 0.3s"), "{e}");
    assert!(
        gone_within(pid, Duration::from_secs(10)),
        "the provider (pid {pid}) outlived its connection"
    );
}
