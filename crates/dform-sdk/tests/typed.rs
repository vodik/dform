//! A provider from Rust types: `#[derive(Resource)]` gives the schema,
//! `Lifecycle` the calls, Plan comes from the schema (R-25).

use dform_core::plugin::backend::{Call, Handler, Reply, silent};
use dform_core::plugin::{pb, wire};
use dform_sdk::typed::Result;
use dform_sdk::{Lifecycle, Provider, Resource, Typed};
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
#[dform(type = "acme.bucket", replace = "destroy_first")]
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
    fn create(p: &Acme, mut desired: Bucket, _key: &str) -> Result<(String, Bucket)> {
        let id = format!("b-{}", desired.name);
        desired.id = Some(id.clone());
        p.buckets
            .lock()
            .unwrap()
            .insert(id.clone(), desired.clone());
        Ok((id, desired))
    }
    fn update(p: &Acme, remote: &str, prior: Bucket, mut desired: Bucket) -> Result<Bucket> {
        desired.id = prior.id;
        p.buckets
            .lock()
            .unwrap()
            .insert(remote.to_string(), desired.clone());
        Ok(desired)
    }
    fn delete(p: &Acme, remote: &str) -> Result<()> {
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
        e.to_string().contains("required attribute name is not set"),
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
