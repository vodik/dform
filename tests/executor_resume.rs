//! Resume a half-applied plan: `apply` on a stack an apply left partial
//! finishes the remaining actions, or reports what changed underneath them
//! and stops.

mod common;
use common::{BACKENDS, Backend, Scratch, mock};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
provider fake
"#;

fn dform_on(s: &Scratch, backend: Backend, args: &[&str]) -> common::Run {
    s.run_on(backend, &common::on("p.df", &["--world", "w.json"], args))
}

fn state(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.state.json")).unwrap()
}

/// The provider dies as it is called to Apply the vm (a process exits; the
/// mock linked in is gone); the next apply finishes, on every backend.
#[test]
fn apply_after_a_crash_finishes_the_remaining_actions() {
    for backend in BACKENDS {
        apply_after_a_crash_finishes_on(backend);
    }
}

fn apply_after_a_crash_finishes_on(backend: Backend) {
    let s = Scratch::new("resume-crash");
    s.write("p.df", PROG);
    let r = dform_on(
        &s,
        backend,
        &["apply", "--chaos", "crash=compute.vm[\"app\"]"],
    )
    .failure();
    assert!(
        r.stderr
            .contains("apply compute.vm[\"app\"]: the provider fakecloud exited during the call"),
        "{backend:?}: {}",
        r.stderr
    );
    let st = state(&s);
    assert_eq!(
        st["in_flight"]["remaining"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["compute.vm::app"],
        "{st}"
    );
    let r = dform_on(&s, backend, &["apply"]).success();
    assert_eq!(
        r.stdout,
        "resuming the apply interrupted at tick 1; remaining: compute.vm[\"app\"]\n\
         plan: 1 deformation (1 create)\ndefinite:\n\
         + compute.vm[\"app\"]\n  subnet_id = \"net.subnet:a\"\n\
         apply order:\n  tick 1\n    compute.vm[\"app\"]\n\
         resumed from the apply interrupted at tick 1:\n  \
         compute.vm[\"app\"]  (retried with its idempotency key: nothing it made was found)\n\
         apply: complete\n"
    );
    let st = state(&s);
    assert!(st.get("in_flight").is_none(), "{st}");
    assert_eq!(st["resources"].as_object().unwrap().len(), 3);
    let r = dform_on(&s, backend, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
}

/// Chaos `stop-after=N` is the executor's crash: dform stops as if killed
/// once N Apply calls have returned, each identity persisted and nothing
/// after it called; the next apply finishes. On every backend.
#[test]
fn apply_after_a_stop_finishes_the_remaining_actions() {
    for backend in BACKENDS {
        let s = Scratch::new("resume-stop");
        s.write("p.df", PROG);
        let r = dform_on(&s, backend, &["apply", "--chaos", "stop-after=2"]).failure();
        assert!(
            r.stderr.contains(
                "apply net.subnet[\"a\"]: dform stopped after this Apply call returned \
                 (chaos stop-after)"
            ),
            "{backend:?}: {}",
            r.stderr
        );
        let st = state(&s);
        assert_eq!(
            st["resources"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["net.subnet::a", "net.vpc::main"],
            "{backend:?}: {st}"
        );
        assert_eq!(
            st["in_flight"]["remaining"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["compute.vm::app"],
            "{backend:?}: {st}"
        );
        // The tick never ended: the world's clock did not move.
        let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
        assert!(w.get("tick").is_none(), "{backend:?}: {w}");
        let r = dform_on(&s, backend, &["apply"]).success();
        assert!(
            r.stdout.starts_with(
                "resuming the apply interrupted at tick 1; remaining: compute.vm[\"app\"]\n"
            ),
            "{backend:?}: {}",
            r.stdout
        );
        let r = dform_on(&s, backend, &["plan"]).success();
        assert_eq!(r.summary(), "stack p is undeformed", "{backend:?}");
    }
}

/// The first apply updates the vpc and fails on the subnet; after that tick
/// the world changes the subnet under the remaining update. The next apply
/// prints the change, derives the deny and stops before any Apply call; the
/// one after that plans against the world as it now is.
#[test]
fn apply_stops_when_the_world_changed_under_a_remaining_action() {
    let s = Scratch::new("resume-changed");
    s.write("p.df", PROG);
    mock(&s, &["apply"]).success();
    s.write("p.df", &PROG.replace("\" }", "\", tier = \"web\" }"));
    mock(
        &s,
        &[
            "apply",
            "--chaos",
            "fail=net.subnet[\"a\"]",
            "--chaos",
            r#"mutate=net.subnet["a"].tags.owner="someone""#,
        ],
    )
    .failure();
    let before = s.read("w.json");
    let r = mock(&s, &["apply"]).failure();
    assert!(
        r.stderr.contains(
            "the world changed under a remaining action:\n~ net.subnet[\"a\"]\n  tags.owner: <none> -> \"someone\"\n"
        ),
        "{}",
        r.stderr
    );
    // The stop is a deny the evaluator derives from the remaining
    // deformation and the world's digest (`zset::POLICY_RULES`), as at a
    // phase boundary.
    assert!(
        r.stderr.contains(
            "constraint violations:\n- the world changed under a remaining action: net.subnet[\"a\"]\n"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("apply stopped: blocked by constraints on the remaining actions"),
        "{}",
        r.stderr
    );
    assert_eq!(s.read("w.json"), before, "no Apply call was made");
    assert!(state(&s).get("in_flight").is_none());
    let r = mock(&s, &["apply"]).success();
    assert!(
        r.stdout.contains(
            "~ net.subnet[\"a\"]\n  tags.owner: \"someone\" -> <none>\n  tier: <none> -> \"web\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}
