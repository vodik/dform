//! R-127, R-206, R-137: what only a run of the binary shows of a tick's
//! block: a failure stops the tick and the run fails; Ctrl-C (chaos
//! `interrupt`, the stop a signal asks for, sent with a create) stops
//! after the call in flight, exits 130, and the next apply resumes the
//! tick. What the block prints is the printer's (tests/apply_events.rs,
//! over a fixed list of events); that a signal is what asks is
//! interrupt.rs's.

mod common;
use common::Scratch;

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

/// `dform dev --world w.json [--chaos C].. apply --yes p.df`.
fn apply(s: &Scratch, chaos: &[&str]) -> std::process::Output {
    let mut mock = vec!["--world", "w.json"];
    for c in chaos {
        mock.extend(["--chaos", c]);
    }
    common::dform()
        .args(common::on("p.df", &mock, &["apply", "--yes"]))
        .current_dir(s.path(""))
        .output()
        .unwrap()
}

/// The first failure stops the tick: what failed is not in state, the
/// run fails.
#[test]
fn a_failure_stops_the_tick() {
    let s = project("progress-fail");
    let out = apply(&s, &["fail=net.subnet[\"b\"]"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let made = common::identities(&s);
    assert!(made.contains(&"net.vpc::main".to_string()), "{made:?}");
    assert!(!made.contains(&"net.subnet::b".to_string()), "{made:?}");
}

/// Ctrl-C while a create runs: the create awaited and kept, what never
/// started not made, exit 130; the next apply resumes the tick and makes
/// the rest.
#[test]
fn an_interrupt_exits_130_and_the_next_apply_resumes() {
    let s = project("progress-int");
    let chaos = ["delay=net.subnet[\"a\"]:200", "interrupt=net.subnet[\"a\"]"];
    let out = apply(&s, &chaos);
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(common::identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let out = apply(&s, &[]);
    assert!(out.status.success());
    assert_eq!(common::identities(&s).len(), 3);
}
