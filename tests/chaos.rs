//! `dform dev --chaos ... apply`: failure and latency injection on the fake provider.

mod common;
use common::{Scratch, mock};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc = main, cidr = "10.0.1.0/24" }
provider fake
"#;

fn stack(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    s
}

#[test]
fn fail_stops_before_the_action_and_keeps_what_came_before() {
    let s = stack("chaos-fail");
    let r = mock(&s, &["apply", "--chaos", "fail=net.subnet[\"a\"]"]).failure();
    assert!(
        r.stderr
            .contains("apply net.subnet[\"a\"]: injected failure"),
        "{}",
        r.stderr
    );
    assert!(s.json("w.json")["resources"].get("net.vpc::main").is_some());
    assert!(s.json("w.json")["resources"].get("net.subnet::a").is_none());
    assert!(
        s.json("w.state.json")["resources"]
            .get("net.vpc::main")
            .is_some()
    );
    let r = mock(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("vpc = main"), "{}", r.stdout);
}

/// A timed-out Create may have taken effect (DEADLINE_EXCEEDED): it is
/// uncertain, and the next run asks the provider for what its idempotency
/// key made before it plans. The object is found and state maps it: no
/// orphan, and no Create again.
#[test]
fn a_create_that_timed_out_is_found_not_created_again() {
    let s = stack("chaos-timeout");
    let r = mock(&s, &["apply", "--chaos", "timeout=net.subnet[\"a\"]"]).failure();
    assert!(
        r.stderr.contains("apply net.subnet[\"a\"]: timed out"),
        "{}",
        r.stderr
    );
    assert!(s.json("w.json")["resources"].get("net.subnet::a").is_some());
    assert!(
        s.json("w.state.json")["resources"]
            .get("net.subnet::a")
            .is_none()
    );
    assert_eq!(
        s.json("w.state.json")["uncertain"]["net.subnet::a"]["op"],
        "create"
    );
    // The plan asks first: the subnet is there, and nothing is to do.
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stderr
            .contains("resolved: net.subnet[\"a\"]: the create whose answer was lost made a"),
        "{}",
        r.stderr
    );
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    let r = mock(&s, &["apply"]).success();
    assert!(!r.stdout.contains("+ net.subnet[\"a\"]"), "{}", r.stdout);
    assert_eq!(
        s.json("w.state.json")["resources"]["net.subnet::a"]["remote"],
        "a"
    );
    assert!(
        s.json("w.state.json").get("uncertain").is_none(),
        "{}",
        s.read("w.state.json")
    );
}

/// Found although the program no longer wants it: it is dform's, so it is
/// deleted, not left behind.
#[test]
fn a_timed_out_create_the_program_dropped_is_deleted() {
    let s = stack("chaos-timeout-dropped");
    mock(&s, &["apply", "--chaos", "timeout=net.subnet[\"a\"]"]).failure();
    s.write(
        "p.df",
        "\n\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n",
    );
    let r = mock(&s, &["apply"]).success();
    assert!(r.stdout.contains("- net.subnet a"), "{}", r.stdout);
    assert!(s.json("w.json")["resources"].get("net.subnet::a").is_none());
}

/// Not found (the timed-out call never reached the world), the Create is
/// sent again with the same idempotency key.
#[test]
fn a_create_that_was_not_found_is_retried_with_its_key() {
    let s = stack("chaos-timeout-retry");
    mock(&s, &["apply", "--chaos", "timeout=net.subnet[\"a\"]"]).failure();
    let key = s.json("w.state.json")["uncertain"]["net.subnet::a"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    // The world lost it after all.
    let mut w = s.json("w.json");
    w["resources"]
        .as_object_mut()
        .unwrap()
        .remove("net.subnet::a");
    std::fs::write(s.path("w.json"), w.to_string()).unwrap();
    let r = mock(&s, &["apply"]).success();
    assert!(r.stdout.contains("+ net.subnet a"), "{}", r.stdout);
    assert_eq!(
        s.json("w.json")["resources"]["net.subnet::a"]["key"],
        key.as_str()
    );
    assert!(
        s.json("w.state.json").get("uncertain").is_none(),
        "{}",
        s.read("w.state.json")
    );
}

/// Read misses a new resource for K Reads. Past the retry budget (three
/// attempts) the resource is taken as gone: plan creates it again and the
/// cloud refuses.
#[test]
fn read_lag_past_the_retry_budget_is_gone() {
    let s = stack("chaos-lag");
    // Within the apply the subnet gets the vpc's id from the Create response.
    mock(&s, &["apply", "--chaos", "read-lag=net.vpc[\"main\"]:3"]).success();
    assert_eq!(
        s.json("w.json")["resources"]["net.subnet::a"]["attrs"]["vpc"],
        "net.vpc:main"
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stderr.contains(
            "retry net.vpc[\"main\"] read (2/3)\nretry net.vpc[\"main\"] read (3/3)\n\
             read net.vpc[\"main\"]: nothing after 3 attempts; taken as gone\n"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
    assert!(
        r.stdout.contains("vpc: \"net.vpc:main\" → main"),
        "{}",
        r.stdout
    );
    // Those three Reads used up the lag: visible and undeformed.
    let r = mock(&s, &["plan"]).success();
    assert_eq!(r.stderr, "");
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

#[test]
fn mutate_changes_the_world_after_the_tick() {
    let s = stack("chaos-mutate");
    let r = mock(
        &s,
        &[
            "apply",
            "--chaos",
            r#"mutate=net.vpc["main"].cidr="10.9.0.0/16""#,
        ],
    )
    .success();
    assert!(
        r.stdout
            .contains("chaos: mutate net.vpc[\"main\"].cidr = \"10.9.0.0/16\" after tick 0"),
        "{}",
        r.stdout
    );
    // The fake schema's cidr is force_new: undoing the drift replaces, and
    // the subnet waits for the replacement's id.
    let r = mock(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 2 changes (1 update, 1 replace) over 2 ticks",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  ± net.vpc main  p.df:3  cidr is immutable\n      \
             cidr: \"10.9.0.0/16\" → \"10.0.0.0/16\""
        ),
        "{}",
        r.stdout
    );
}

#[test]
fn latency_is_recorded_not_slept() {
    let s = stack("chaos-latency");
    let started = std::time::Instant::now();
    let r = mock(&s, &["apply", "--chaos", "latency=net.vpc[\"main\"]:60000"]).success();
    assert!(started.elapsed().as_secs() < 30);
    assert!(
        r.stdout
            .contains("chaos: latency net.vpc[\"main\"]: 60000ms (simulated, not slept)"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("chaos: simulated apply time: 60000ms"),
        "{}",
        r.stdout
    );
}

#[test]
fn chaos_names_must_be_resources_of_the_stack() {
    let s = stack("chaos-typo");
    let r = mock(&s, &["apply", "--chaos", "fail=net.subnet[\"b\"]"]).failure();
    assert!(
        r.stderr
            .contains(r#"net.subnet["b"] is not a resource of this stack"#),
        "{}",
        r.stderr
    );
    let r = mock(&s, &["apply", "--chaos", "explode=net.subnet[\"a\"]"]).failure();
    assert!(
        r.stderr.contains("unknown chaos knob 'explode'"),
        "{}",
        r.stderr
    );
    assert!(!s.path("w.json").exists());
}
