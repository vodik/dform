//! A provider from Rust types: `#[derive(Resource)]` gives the schema,
//! `Lifecycle` the calls, Plan comes from the schema (R-25).

use dform_core::plugin::backend::{Call, Handler, Reply, silent};
use dform_core::plugin::{pb, wire};
use dform_sdk::typed::Result;
use dform_sdk::{Lifecycle, Progress, Provider, Resource, Typed};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// A cloud in memory.
struct Acme {
    buckets: Mutex<BTreeMap<String, Bucket>>,
}

impl Provider for Acme {
    const NAME: &'static str = "acme";
    fn configure(settings: &Json) -> Result<(Acme, Option<String>)> {
        Ok((
            Acme {
                buckets: Mutex::new(BTreeMap::new()),
            },
            settings["account"].as_str().map(String::from),
        ))
    }
}

#[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[dform(type = "acme.bucket", replace = "destroy_first", lookup = "name")]
struct Bucket {
    #[dform(required, force_new)]
    name: String,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[dform(computed, id)]
    #[serde(default)]
    id: Option<String>,
}

impl Lifecycle<Acme> for Bucket {
    fn read(p: &Acme, remote: &str) -> Result<Option<Bucket>> {
        Ok(p.buckets.lock().unwrap().get(remote).cloned())
    }
    fn create(
        p: &Acme,
        mut desired: Bucket,
        _key: &str,
        progress: &Progress,
    ) -> Result<(String, Bucket)> {
        // As an API answers a create: there, not ready, then ready.
        progress.status("PROVISIONING");
        progress.message("waiting for the bucket to be ready");
        progress.status("READY");
        let id = format!("b-{}", desired.name);
        desired.id = Some(id.clone());
        p.buckets
            .lock()
            .unwrap()
            .insert(id.clone(), desired.clone());
        Ok((id, desired))
    }
    fn update(
        p: &Acme,
        remote: &str,
        prior: Bucket,
        mut desired: Bucket,
        _: &Progress,
    ) -> Result<Bucket> {
        desired.id = prior.id;
        p.buckets
            .lock()
            .unwrap()
            .insert(remote.to_string(), desired.clone());
        Ok(desired)
    }
    fn delete(p: &Acme, remote: &str, _: &Progress) -> Result<()> {
        p.buckets.lock().unwrap().remove(remote);
        Ok(())
    }
}

fn provider() -> Typed<Acme> {
    Typed::new().resource::<Bucket>()
}

fn call<R: TryFrom<Reply, Error = Reply>>(h: &Typed<Acme>, c: impl Into<Call>) -> R {
    R::try_from(h.handle(c.into(), &silent).unwrap()).unwrap()
}

#[test]
fn the_derive_gives_the_schema() {
    let h = provider();
    let facts = h.facts();
    assert!(
        facts.contains("type_provider(\"acme.bucket\", \"acme\")"),
        "{facts}"
    );
    assert!(
        facts.contains(
            "type_attr(\"acme.bucket\", \"name\", \"string\", [\"required\", \"force_new\"])"
        ),
        "{facts}"
    );
    assert!(
        facts.contains("type_attr(\"acme.bucket\", \"tags\", \"map\", [])"),
        "{facts}"
    );
    assert!(
        facts.contains("type_attr(\"acme.bucket\", \"id\", \"string\", [\"computed\", \"id\"])"),
        "{facts}"
    );
    assert!(
        facts.contains("type_replace(\"acme.bucket\", \"destroy_first\")"),
        "{facts}"
    );
    assert!(
        facts.contains("type_lookup(\"acme.bucket\", [\"name\"])"),
        "{facts}"
    );
    let s: pb::SchemaResponse = call(&h, pb::SchemaRequest::default());
    assert!(s.facts.iter().any(|f| f.pred == "type_attr"));
}

#[test]
fn plan_comes_from_the_schema_and_apply_from_the_lifecycle() {
    let h = provider();
    let _: pb::ConfigureResponse = call(
        &h,
        pb::ConfigureRequest {
            config: Some(wire::doc(&json!({"settings": {"account": "acct-1"}}))),
        },
    );
    // A required attribute missing refuses.
    let e = h
        .handle(
            pb::PlanRequest {
                r#type: "acme.bucket".into(),
                name: "logs".into(),
                desired: Some(wire::doc(&json!({"tags": {}}))),
                ..Default::default()
            }
            .into(),
            &silent,
        )
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("plan acme.bucket[\"logs\"]: name is required: "),
        "{e}"
    );
    // Create: the id is computed.
    let a: pb::ApplyResponse = call(
        &h,
        pb::ApplyRequest {
            op: pb::Op::Create as i32,
            r#type: "acme.bucket".into(),
            name: "logs".into(),
            config: Some(wire::doc(&json!({"name": "logs", "tags": {"team": "a"}}))),
            idempotency_key: "k1".into(),
            ..Default::default()
        },
    );
    assert_eq!(a.remote, "b-logs");
    let computed = wire::from_doc(a.computed.as_ref().unwrap()).unwrap();
    assert_eq!(computed, json!({"id": "b-logs"}));
    let attrs = wire::from_doc(a.attrs.as_ref().unwrap()).unwrap();
    assert_eq!(attrs, json!({"name": "logs", "tags": {"team": "a"}}));
    // A tag changes in place; the name, force_new, replaces.
    let plan = |desired: Json| -> pb::PlanResponse {
        call(
            &h,
            pb::PlanRequest {
                r#type: "acme.bucket".into(),
                name: "logs".into(),
                prior: Some(wire::doc(&attrs)),
                desired: Some(wire::doc(&desired)),
                remote: "b-logs".into(),
            },
        )
    };
    let p = plan(json!({"name": "logs", "tags": {"team": "b"}}));
    assert_eq!(p.changes.len(), 1);
    assert!(!p.requires_replace);
    assert!(plan(json!({"name": "logs2", "tags": {"team": "a"}})).requires_replace);
    // Read answers what the lifecycle has.
    let r: pb::ReadResponse = call(
        &h,
        pb::ReadRequest {
            r#type: "acme.bucket".into(),
            remote: "b-logs".into(),
            name: "logs".into(),
        },
    );
    assert!(r.found);
    let _: pb::ApplyResponse = call(
        &h,
        pb::ApplyRequest {
            op: pb::Op::Delete as i32,
            r#type: "acme.bucket".into(),
            name: "logs".into(),
            remote: "b-logs".into(),
            ..Default::default()
        },
    );
    let r: pb::ReadResponse = call(
        &h,
        pb::ReadRequest {
            r#type: "acme.bucket".into(),
            remote: "b-logs".into(),
            name: "logs".into(),
        },
    );
    assert!(!r.found);
}

/// What a lifecycle function says on its `Progress` reaches the Apply's
/// sink, in order, each event naming the object (R-130).
#[test]
fn a_create_says_how_it_goes_on_the_apply_sink() {
    let h = provider();
    let _: pb::ConfigureResponse = call(&h, pb::ConfigureRequest::default());
    let said = Mutex::new(Vec::new());
    let sink = |e: pb::Event| said.lock().unwrap().push(e);
    let r = h.handle(
        pb::ApplyRequest {
            op: pb::Op::Create as i32,
            r#type: "acme.bucket".into(),
            name: "logs".into(),
            config: Some(wire::doc(&json!({"name": "logs"}))),
            ..Default::default()
        }
        .into(),
        &sink,
    );
    assert!(matches!(r, Ok(Reply::Apply(_))), "{r:?}");
    let said = said.into_inner().unwrap();
    let words: Vec<(Option<&str>, Option<&str>)> = said
        .iter()
        .map(|e| (e.status.as_deref(), e.message.as_deref()))
        .collect();
    assert_eq!(
        words,
        [
            (Some("PROVISIONING"), None),
            (None, Some("waiting for the bucket to be ready")),
            (Some("READY"), None),
        ]
    );
    assert!(
        said.iter().all(|e| e.address == r#"acme.bucket["logs"]"#),
        "{said:?}"
    );
}

/// A key: its secret write-only, its size the API's when not written,
/// and the provider's own key not one it manages.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[dform(type = "acme.key")]
struct Key {
    #[dform(required, force_new)]
    name: String,
    #[dform(sensitive, write_only)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

impl Lifecycle<Acme> for Key {
    fn read(_: &Acme, remote: &str) -> Result<Option<Key>> {
        Ok(Some(Key {
            name: remote.to_string(),
            secret: None,
            size: Some(2048),
            id: Some(remote.to_string()),
        }))
    }
    fn create(p: &Acme, desired: Key, _: &str, _: &Progress) -> Result<(String, Key)> {
        let k = Key::read(p, &desired.name)?.unwrap();
        Ok((desired.name, k))
    }
    fn update(p: &Acme, remote: &str, _: Key, _: Key, _: &Progress) -> Result<Key> {
        Ok(Key::read(p, remote)?.unwrap())
    }
    fn delete(_: &Acme, _: &str, _: &Progress) -> Result<()> {
        Ok(())
    }
    fn check(_: &Acme, desired: &Json) -> Result<()> {
        match desired["name"].as_str() {
            Some("own") => Err(dform_sdk::typed::Error::Refused(
                "name: \"own\" is the provider's own key".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// The derive's `write_only`; an Optional+Computed attribute answered
/// as computed (the engine compares it where the program sets it); the
/// examples `provider check` runs with; a lifecycle's `check` refusing
/// at Plan, after the address; and Configure's settings `{}` when the
/// program writes none, never the engine's own keys.
#[test]
fn write_only_optional_computed_examples_and_check() {
    let h = Typed::<Acme>::new().resource::<Key>().example::<Key>(
        json!({"name": "a"}),
        json!({"name": "a", "size": 4096}),
        "name",
    );
    assert!(
        h.facts()
            .contains(r#"type_attr("acme.key", "secret", "string", ["sensitive", "write_only"])"#),
        "{}",
        h.facts()
    );
    let s: pb::SchemaResponse = call(&h, pb::SchemaRequest::default());
    assert_eq!(s.examples.len(), 1);
    assert_eq!(s.examples[0].r#type, "acme.key");
    assert_eq!(s.examples[0].required, "name");

    let _: pb::ConfigureResponse = call(
        &h,
        pb::ConfigureRequest {
            config: Some(wire::doc(&json!({"world": "/w.json", "stack": "s"}))),
        },
    );
    let r: pb::ReadResponse = call(
        &h,
        pb::ReadRequest {
            r#type: "acme.key".into(),
            remote: "a".into(),
            name: "a".into(),
        },
    );
    let attrs = wire::from_doc(r.attrs.as_ref().unwrap()).unwrap();
    let computed = wire::from_doc(r.computed.as_ref().unwrap()).unwrap();
    assert_eq!(attrs, json!({"name": "a"}));
    assert_eq!(computed, json!({"id": "a", "size": 2048}));

    let e = h
        .handle(
            pb::PlanRequest {
                r#type: "acme.key".into(),
                name: "mine".into(),
                desired: Some(wire::doc(&json!({"name": "own"}))),
                ..Default::default()
            }
            .into(),
            &silent,
        )
        .unwrap_err();
    assert!(
        e.to_string()
            .contains(r#"plan acme.key["mine"]: name: "own" is the provider's own key"#),
        "{e}"
    );
}

/// The settings a typed provider is configured with: the program's, `{}`
/// for none.
#[test]
fn configure_takes_the_programs_settings_only() {
    struct Seen;
    static SEEN: Mutex<Vec<Json>> = Mutex::new(Vec::new());
    impl Provider for Seen {
        const NAME: &'static str = "seen";
        fn configure(settings: &Json) -> Result<(Seen, Option<String>)> {
            SEEN.lock().unwrap().push(settings.clone());
            Ok((Seen, None))
        }
    }
    let h = Typed::<Seen>::new();
    for config in [
        json!({"world": "/w.json", "stack": "s"}),
        json!({"world": "/w.json", "settings": {"region": "r"}}),
    ] {
        let _: pb::ConfigureResponse = h
            .handle(
                pb::ConfigureRequest {
                    config: Some(wire::doc(&config)),
                }
                .into(),
                &silent,
            )
            .map(|r| pb::ConfigureResponse::try_from(r).unwrap())
            .unwrap();
    }
    assert_eq!(*SEEN.lock().unwrap(), [json!({}), json!({"region": "r"})]);
}

/// `Provider::KEEP` (R-164): an Apply update's `keep` path is left as the
/// object has it: a provider whose update sends only what `desired`
/// holds says `keep` in its handshake, and its update never sees the
/// kept write-only secret; one that does not refuses a `keep`.
#[test]
fn keep_is_the_providers_promise() {
    struct Vault;
    static TOKENS: Mutex<BTreeMap<String, Token>> = Mutex::new(BTreeMap::new());
    impl Provider for Vault {
        const NAME: &'static str = "vault";
        const KEEP: bool = true;
        fn configure(_: &Json) -> Result<(Vault, Option<String>)> {
            Ok((Vault, None))
        }
    }
    #[derive(Resource, Serialize, Deserialize, Clone, Debug, PartialEq)]
    #[dform(type = "vault.token")]
    struct Token {
        #[dform(required, force_new)]
        name: String,
        #[serde(default)]
        label: String,
        #[dform(sensitive, write_only)]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    }
    impl Lifecycle<Vault> for Token {
        fn read(_: &Vault, remote: &str) -> Result<Option<Token>> {
            // The API never answers the secret.
            Ok(TOKENS.lock().unwrap().get(remote).map(|t| Token {
                secret: None,
                ..t.clone()
            }))
        }
        fn create(_: &Vault, desired: Token, _: &str, _: &Progress) -> Result<(String, Token)> {
            TOKENS
                .lock()
                .unwrap()
                .insert(desired.name.clone(), desired.clone());
            Ok((desired.name.clone(), desired))
        }
        fn update(
            _: &Vault,
            remote: &str,
            _: Token,
            desired: Token,
            _: &Progress,
        ) -> Result<Token> {
            // Only what `desired` holds is sent.
            let mut all = TOKENS.lock().unwrap();
            let t = all.get_mut(remote).unwrap();
            t.label = desired.label.clone();
            if let Some(s) = desired.secret {
                t.secret = Some(s);
            }
            Ok(t.clone())
        }
        fn delete(_: &Vault, _: &str, _: &Progress) -> Result<()> {
            Ok(())
        }
    }
    let h = Typed::<Vault>::new().resource::<Token>();
    let hs: pb::HandshakeResponse = call_any(
        &h,
        pb::HandshakeRequest {
            protocol_version: dform_core::plugin::backend::VERSION,
        },
    );
    assert!(hs.capabilities.contains(&"keep".to_string()), "{hs:?}");
    let _: pb::ConfigureResponse = call_any(&h, pb::ConfigureRequest::default());
    let apply = |op: pb::Op, config: Json, keep: &[&str]| pb::ApplyRequest {
        op: op as i32,
        r#type: "vault.token".into(),
        name: "t".into(),
        remote: "t".into(),
        config: Some(wire::doc(&config)),
        keep: keep.iter().map(|k| k.to_string()).collect(),
        ..Default::default()
    };
    let _: pb::ApplyResponse = call_any(
        &h,
        apply(
            pb::Op::Create,
            json!({"name": "t", "label": "a", "secret": "s3cr3t"}),
            &[],
        ),
    );
    let _: pb::ApplyResponse = call_any(
        &h,
        apply(
            pb::Op::Update,
            json!({"name": "t", "label": "b"}),
            &["secret"],
        ),
    );
    let h2 = provider();
    let hs: pb::HandshakeResponse = call(
        &h2,
        pb::HandshakeRequest {
            protocol_version: dform_core::plugin::backend::VERSION,
        },
    );
    assert!(!hs.capabilities.contains(&"keep".to_string()), "{hs:?}");
    let _: pb::ConfigureResponse = call(&h2, pb::ConfigureRequest::default());
    let e = h2
        .handle(
            pb::ApplyRequest {
                op: pb::Op::Update as i32,
                r#type: "acme.bucket".into(),
                name: "logs".into(),
                remote: "b-logs".into(),
                config: Some(wire::doc(&json!({"name": "logs"}))),
                keep: vec!["tags".into()],
                ..Default::default()
            }
            .into(),
            &silent,
        )
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("keep tags: this provider cannot leave an attribute as it is"),
        "{e}"
    );
    fn call_any<P: Provider, R: TryFrom<Reply, Error = Reply>>(
        h: &Typed<P>,
        c: impl Into<Call>,
    ) -> R {
        R::try_from(h.handle(c.into(), &silent).unwrap()).unwrap()
    }
    // The update kept the secret the create wrote.
    assert_eq!(
        TOKENS.lock().unwrap()["t"].secret.as_deref(),
        Some("s3cr3t")
    );
    let r: pb::ReadResponse = call_any(
        &h,
        pb::ReadRequest {
            r#type: "vault.token".into(),
            remote: "t".into(),
            name: "t".into(),
        },
    );
    assert_eq!(
        wire::from_doc(r.attrs.as_ref().unwrap()).unwrap()["label"],
        "b"
    );
}
