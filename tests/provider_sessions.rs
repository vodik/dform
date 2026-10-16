//! Sessions on the wire: one provider process serves several configured
//! instances of itself. Each deployment's Configure opens a session and
//! every call it makes names it, so a call lands in its own deployment's
//! world whatever the other calls in flight are doing.

use crate::common;
use common::{Scratch, attr, copy_dir, repo, traced};
use dform::plugin::backend::Session;
use dform::plugin::link::Link;
use dform::plugin::{pb, wire};
use dform_grpc::client::Conn;
use dform_grpc::spawn::{Env, Program};
use serde_json::{Value as Json, json};

/// How long each Apply is slowed (chaos `delay`): long enough for the
/// other to be sent and say what it made meanwhile.
const DELAY_MS: u64 = 1500;

/// The deployment `d`'s configuration of the fake: its own world, and its
/// vpc's Apply slowed.
fn config(s: &Scratch, d: &str, vpc: &str) -> Json {
    json!({
        "stack": d,
        "world": s.path(&format!("{d}.json")).display().to_string(),
        "chaos": [format!("delay=net.vpc[\"{vpc}\"]:{DELAY_MS}")],
    })
}

/// A create of the vpc `name`.
fn create(name: &str, cidr: &str) -> pb::ApplyRequest {
    pb::ApplyRequest {
        op: pb::Op::Create as i32,
        r#type: "net.vpc".into(),
        name: name.into(),
        config: Some(wire::doc(&json!({ "cidr": cidr }))),
        idempotency_key: name.into(),
        ..Default::default()
    }
}

/// The objects the world file of `d` holds.
fn world(s: &Scratch, d: &str) -> Vec<String> {
    let w: Json = serde_json::from_str(&s.read(&format!("{d}.json"))).unwrap();
    w["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// Two deployments' Applies in flight together on one process of the
/// fake, each session configured with its own world: both are inside the
/// provider at once (each says it made its object before either answers),
/// and each object lands in its own deployment's world. The second
/// deployment configured again while the first's Apply is in flight is
/// what, with one configuration per process, carried the first's object
/// into the second's world.
#[test]
fn two_sessions_calls_in_flight_each_land_in_their_own_world() {
    let s = Scratch::new("sessions-in-flight");
    let mut link = Conn::link(
        &Program::exe(common::exe("dform-provider-fake")),
        &Env::default(),
    )
    .unwrap();
    let open = |link: &mut Link, c: Json| -> Session {
        Session(
            link.configure(Session::NONE, wire::doc(&c))
                .unwrap()
                .session,
        )
    };
    let staging = open(&mut link, config(&s, "staging", "a"));
    let prod = open(&mut link, config(&s, "prod", "b"));
    assert_ne!(staging, prod);
    let a = link.submit(staging, create("a", "10.0.0.0/16"));
    link.configure(prod, wire::doc(&config(&s, "prod", "b")))
        .unwrap();
    let b = link.submit(prod, create("b", "10.1.0.0/16"));
    let name = |t| match t {
        t if t == a => "a",
        t if t == b => "b",
        _ => "another call",
    };
    let mut seen = Vec::new();
    for _ in 0..2 {
        let (t, r) = link.next_completed(&mut |t, e| {
            seen.push(format!("{} {}", name(t), e.status.unwrap_or_default()))
        });
        assert!(r.is_ok(), "{r:?}");
        seen.push(format!("{} answered", name(t)));
    }
    assert!(
        t_in(&seen[..2], "a made") && t_in(&seen[..2], "b made"),
        "{seen:?}"
    );
    assert_eq!(world(&s, "staging"), ["net.vpc::a"]);
    assert_eq!(world(&s, "prod"), ["net.vpc::b"]);
}

fn t_in(seen: &[String], what: &str) -> bool {
    seen.iter().any(|x| x == what)
}

/// k3s-platform on the mock: its four deployments (platform and apps,
/// staging and prod) are four sessions of one process of the fake, each
/// configured once: no deployment's configuration is sent again before
/// its calls.
#[test]
fn a_trees_deployments_are_sessions_of_one_process() {
    let s = Scratch::in_target("examples", "k3s-platform-sessions");
    copy_dir(&repo().join("examples/k3s-platform"), &s.dir);
    let (r, spans) = traced(&s, &["plan"]);
    r.success();
    let named = |kind: &str| -> Vec<&serde_json::Value> {
        spans
            .iter()
            .filter(|x| x["name"].as_str().is_some_and(|n| n.ends_with(kind)))
            .collect()
    };
    assert_eq!(named(" Handshake").len(), 1, "one process");
    let mut sessions: Vec<String> = named(" Configure")
        .iter()
        .map(|x| attr(x, "session").expect("a Configure's session"))
        .collect();
    sessions.sort();
    assert_eq!(sessions, ["1", "2", "3", "4"], "a session per deployment");
    // Every other call names one of them.
    for x in spans.iter().filter(|x| {
        x["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("fakecloud ") && !n.ends_with(" Configure"))
    }) {
        let session = attr(x, "session").unwrap_or_default();
        assert!(sessions.contains(&session), "{}: {session:?}", x["name"]);
    }
}
