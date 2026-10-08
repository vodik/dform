//! `convert!` round-trips every message the protocol has through the WIT's
//! types: a value's tree, absent documents as the empty obj.

dform_wit::convert!(c, dform_wit::guest::provider::dform::provider::types);

use dform_core::plugin::{pb, wire};

#[test]
fn a_value_round_trips_through_its_tree() {
    let v = pb::Value::from(&dform_core::value::Value::List(vec![]));
    assert_eq!(c::from_tree(&c::to_tree(&v)).unwrap(), v);
    let doc = wire::doc(&serde_json::json!({"a": [1, 0.5, "x", {"b": true}], "c": null}));
    let t = c::to_tree(&doc);
    assert_eq!(c::from_tree(&t).unwrap(), doc);
    let r = pb::ApplyRequest {
        op: pb::Op::Replace as i32,
        r#type: "net.vpc".into(),
        config: Some(doc.clone()),
        assertions: vec![pb::Assertion {
            path: "a".into(),
            op: "eq".into(),
            value: Some(doc),
            message: "m".into(),
        }],
        ..Default::default()
    };
    assert_eq!(c::from_apply_request(&c::to_apply_request(&r)).unwrap(), r);
    // A read with no documents answers the empty obj for each.
    let read = c::from_read_response(&c::to_read_response(&pb::ReadResponse::default())).unwrap();
    assert_eq!(read.attrs, Some(wire::doc(&serde_json::json!({}))));
}

/// A tree whose child is not after its parent is refused, not looped on.
#[test]
fn a_malformed_tree_is_refused() {
    use dform_wit::guest::provider::dform::provider::types as w;
    let t = w::Tree {
        nodes: vec![w::Value::List(w::List { items: vec![0] })],
    };
    assert!(
        c::from_tree(&t)
            .unwrap_err()
            .contains("not after its parent")
    );
}

/// An Apply's event and a reveal's request and answer round-trip (R-130).
#[test]
fn an_event_and_a_reveal_round_trip() {
    let e = pb::Event {
        address: "ovh.instance[\"web\"]".into(),
        status: Some("BUILD".into()),
        message: None,
    };
    assert_eq!(c::from_event(&c::to_event(&e)), e);
    let r = pb::RevealRequest {
        held: Some(pb::Held {
            provider: "fakecloud".into(),
            deployment: "platform".into(),
            r#type: "db.postgres".into(),
            remote: "pg-1".into(),
            path: "password".into(),
            digest: "hmac-sha256:00".into(),
        }),
        lease: "lease-1".into(),
    };
    assert_eq!(
        c::from_reveal_request(&c::to_reveal_request(&r)).unwrap(),
        r
    );
    let a = pb::RevealResponse {
        value: b"s3cret".to_vec(),
    };
    assert_eq!(
        c::from_reveal_response(&c::to_reveal_response(&a)).unwrap(),
        a
    );
}

/// A handshake's declared settings, each with its sensitive flag, cross
/// the WIT as the proto has them (a component provider declares them too).
#[test]
fn a_handshake_carries_its_settings() {
    let hs = pb::HandshakeResponse {
        protocol_version: 1,
        name: "kubernetes".into(),
        capabilities: vec!["resource".into()],
        version: "0.1.0".into(),
        settings: vec![
            pb::SettingDecl {
                name: "kubeconfig".into(),
                sensitive: true,
            },
            pb::SettingDecl {
                name: "namespace".into(),
                sensitive: false,
            },
        ],
    };
    assert_eq!(
        c::from_handshake_response(&c::to_handshake_response(&hs)).unwrap(),
        hs
    );
}
