//! `apply --chaos`: failure and latency injection on the fake provider.

mod common;
use common::Scratch;

const PROG: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
"#;

fn stack(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    s
}

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

fn world(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.json")).unwrap()
}

fn state(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.state.json")).unwrap()
}

#[test]
fn fail_stops_before_the_action_and_keeps_what_came_before() {
    let s = stack("chaos-fail");
    let r = dform(&s, &["apply", "--chaos", "fail=net.subnet/a"]).failure();
    assert!(
        r.stderr.contains("apply net.subnet/a: injected failure"),
        "{}",
        r.stderr
    );
    assert!(world(&s)["resources"].get("net.vpc::main").is_some());
    assert!(world(&s)["resources"].get("net.subnet::a").is_none());
    assert!(state(&s)["resources"].get("net.vpc::main").is_some());
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 create)",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("vpc_id = \"net.vpc:main\""),
        "{}",
        r.stdout
    );
}

#[test]
fn timeout_takes_effect_but_leaves_an_orphan() {
    let s = stack("chaos-timeout");
    let r = dform(&s, &["apply", "--chaos", "timeout=net.subnet/a"]).failure();
    assert!(
        r.stderr.contains("apply net.subnet/a: timed out"),
        "{}",
        r.stderr
    );
    assert!(world(&s)["resources"].get("net.subnet::a").is_some());
    assert!(state(&s)["resources"].get("net.subnet::a").is_none());
    // dform does not know it exists: it plans a create, and the cloud refuses.
    let r = dform(&s, &["plan"]).success();
    assert!(r.stdout.contains("+ net.subnet.a"), "{}", r.stdout);
    let r = dform(&s, &["apply"]).failure();
    assert!(
        r.stderr.contains("net.subnet::a already exists"),
        "{}",
        r.stderr
    );
}

/// Read misses a new resource for K Reads. Past the retry budget (three
/// attempts) the resource is taken as gone: plan creates it again and the
/// cloud refuses.
#[test]
fn read_lag_past_the_retry_budget_is_gone() {
    let s = stack("chaos-lag");
    // Within the apply the subnet gets the vpc's id from the Create response.
    dform(&s, &["apply", "--chaos", "read-lag=net.vpc/main:3"]).success();
    assert_eq!(
        world(&s)["resources"]["net.subnet::a"]["attrs"]["vpc_id"],
        "net.vpc:main"
    );
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stderr.contains(
            "retry net.vpc/main read (2/3)\nretry net.vpc/main read (3/3)\n\
             read net.vpc/main: nothing after 3 attempts; taken as gone\n"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stdout.contains("+ net.vpc.main"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("vpc_id: \"net.vpc:main\" -> ?net.vpc/main#id"),
        "{}",
        r.stdout
    );
    // Those three Reads used up the lag: visible and undeformed.
    let r = dform(&s, &["plan"]).success();
    assert_eq!(r.stderr, "");
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

#[test]
fn mutate_changes_the_world_after_the_tick() {
    let s = stack("chaos-mutate");
    let r = dform(
        &s,
        &[
            "apply",
            "--chaos",
            r#"mutate=net.vpc/main:cidr="10.9.0.0/16""#,
        ],
    )
    .success();
    assert!(
        r.stdout
            .contains("chaos: mutate net.vpc/main: cidr = \"10.9.0.0/16\" after tick 0"),
        "{}",
        r.stdout
    );
    // The fake schema's cidr is force_new: undoing the drift replaces, and
    // the subnet waits for the replacement's id.
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 replace), 1 pending",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("-/+ net.vpc.main  (replace)\n  cidr: \"10.9.0.0/16\" -> \"10.0.0.0/16\""),
        "{}",
        r.stdout
    );
}

#[test]
fn latency_is_recorded_not_slept() {
    let s = stack("chaos-latency");
    let started = std::time::Instant::now();
    let r = dform(&s, &["apply", "--chaos", "latency=net.vpc/main:60000"]).success();
    assert!(started.elapsed().as_secs() < 30);
    assert!(
        r.stdout
            .contains("chaos: latency net.vpc/main: 60000ms (simulated, not slept)"),
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
    let r = dform(&s, &["apply", "--chaos", "fail=net.subnet/b"]).failure();
    assert!(
        r.stderr
            .contains("net.subnet/b is not a resource of this stack"),
        "{}",
        r.stderr
    );
    let r = dform(&s, &["apply", "--chaos", "explode=net.subnet/a"]).failure();
    assert!(
        r.stderr.contains("unknown chaos knob 'explode'"),
        "{}",
        r.stderr
    );
    assert!(!s.path("w.json").exists());
}
