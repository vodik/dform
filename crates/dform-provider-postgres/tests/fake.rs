//! The provider in process against the fake server (`fake`): each call as
//! dform sends it, the server's catalog after it, and what crossed the
//! wire.

use dform_core::plugin::backend::{Call, CallError, Handler, Reply, silent};
use dform_core::plugin::{pb, wire};
use dform_provider_postgres::fake::{self, ADMIN, ADMIN_PASSWORD, Server};
use dform_provider_postgres::{Postgres, provider, scram};
use dform_sdk::Typed;
use serde_json::{Value as Json, json};

fn configured(settings: Json) -> Typed<Postgres> {
    let h = provider();
    h.handle(
        pb::ConfigureRequest {
            config: Some(wire::doc(&json!({ "settings": settings }))),
        }
        .into(),
        &silent,
    )
    .unwrap();
    h
}

fn admin(s: &Server) -> Json {
    json!({"host": "127.0.0.1", "port": s.port, "user": ADMIN, "password": ADMIN_PASSWORD,
           "sslmode": "disable"})
}

fn call<R: TryFrom<Reply, Error = Reply>>(h: &Typed<Postgres>, c: impl Into<Call>) -> R {
    R::try_from(h.handle(c.into(), &silent).unwrap()).unwrap()
}

fn try_apply(
    h: &Typed<Postgres>,
    op: pb::Op,
    typ: &str,
    remote: &str,
    doc: Option<Json>,
    key: &str,
) -> Result<pb::ApplyResponse, CallError> {
    h.handle(
        pb::ApplyRequest {
            op: op as i32,
            r#type: typ.into(),
            name: "it".into(),
            remote: remote.into(),
            config: doc.as_ref().map(wire::doc),
            idempotency_key: key.into(),
            ..Default::default()
        }
        .into(),
        &silent,
    )
    .map(|r| pb::ApplyResponse::try_from(r).unwrap())
}

fn apply(
    h: &Typed<Postgres>,
    op: pb::Op,
    typ: &str,
    remote: &str,
    doc: Option<Json>,
) -> pb::ApplyResponse {
    try_apply(h, op, typ, remote, doc, "k1").unwrap()
}

fn read(h: &Typed<Postgres>, typ: &str, remote: &str) -> Option<(Json, Json)> {
    let r: pb::ReadResponse = call(
        h,
        pb::ReadRequest {
            r#type: typ.into(),
            remote: remote.into(),
            name: "it".into(),
        },
    );
    r.found.then(|| {
        (
            wire::from_doc(r.attrs.as_ref().unwrap()).unwrap(),
            wire::from_doc(r.computed.as_ref().unwrap()).unwrap(),
        )
    })
}

fn plan(
    h: &Typed<Postgres>,
    typ: &str,
    prior: Option<Json>,
    desired: Json,
) -> Result<pb::PlanResponse, CallError> {
    h.handle(
        pb::PlanRequest {
            r#type: typ.into(),
            name: "it".into(),
            prior: prior.as_ref().map(wire::doc),
            desired: Some(wire::doc(&desired)),
            remote: String::new(),
        }
        .into(),
        &silent,
    )
    .map(|r| pb::PlanResponse::try_from(r).unwrap())
}

/// A role made, read, changed in place, its memberships granted and
/// revoked, replaced and dropped; the plaintext never in a statement.
#[test]
fn a_role_through_its_life() {
    let s = Server::start();
    let h = configured(admin(&s));
    let made = apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "readers"})),
    );
    assert_eq!(made.remote, "readers");
    let made = apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse", "login": true, "password": "pw-1",
                    "connection_limit": 20, "member_of": ["readers"]})),
    );
    assert_eq!(made.remote, "synapse");
    let attrs = wire::from_doc(made.attrs.as_ref().unwrap()).unwrap();
    let computed = wire::from_doc(made.computed.as_ref().unwrap()).unwrap();
    // Configured: the name and the memberships; the flags the server's,
    // compared where the program writes them; never the password.
    assert_eq!(attrs, json!({"name": "synapse", "member_of": ["readers"]}));
    assert_eq!(computed["login"], json!(true));
    assert_eq!(computed["inherit"], json!(true));
    assert_eq!(computed["connection_limit"], json!(20));
    assert_eq!(computed["id"], json!("synapse"));
    assert!(
        s.accepts("synapse", "pw-1"),
        "the verifier is of the password"
    );
    assert_eq!(
        s.world().roles["synapse"].comment.as_deref(),
        Some("dform:k1")
    );

    // Sent again with its key: what the first made; with another key, a
    // role that exists is refused, saying to adopt it.
    let again = apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse", "login": true})),
    );
    assert_eq!(again.remote, "synapse");
    let e = try_apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse"})),
        "k2",
    )
    .unwrap_err();
    assert!(
        e.to_string()
            .contains("exists, not made by this resource: adopt it"),
        "{e}"
    );

    // In place: LOGIN taken away, the membership revoked; the password
    // the same, so not written again.
    let before = s.world().roles["synapse"].verifier.clone();
    apply(
        &h,
        pb::Op::Update,
        "postgres.role",
        "synapse",
        Some(json!({"name": "synapse", "login": false, "password": "pw-1"})),
    );
    let w = s.world();
    assert!(!w.roles["synapse"].login);
    assert!(w.members.is_empty(), "{:?}", w.members);
    assert_eq!(
        w.roles["synapse"].verifier, before,
        "an unchanged password is not sent"
    );
    assert!(
        !s.queries().iter().any(|q| q.contains("pw-1")),
        "{:?}",
        s.queries()
    );

    // Gone: delete, then a read answers none.
    apply(&h, pb::Op::Delete, "postgres.role", "synapse", None);
    assert!(read(&h, "postgres.role", "synapse").is_none());
    // Replace (destroy first) makes it again.
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "a"})),
    );
    let r = apply(
        &h,
        pb::Op::Replace,
        "postgres.role",
        "a",
        Some(json!({"name": "a", "createdb": true})),
    );
    assert_eq!(r.remote, "a");
    assert!(s.world().roles["a"].createdb);
}

/// A rotation: the plan of a changed password is an update, never a
/// replace; the apply sends a new verifier (never the plaintext), and the
/// role logs in with the new password and not the old.
#[test]
fn a_password_rotation_is_an_update() {
    let s = Server::start();
    let h = configured(admin(&s));
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse", "login": true, "password": "old-pw"})),
    );
    // As dform asks: the world's side has the program's value where its
    // digest is state's, `(write-only)` where it is not.
    let p = plan(
        &h,
        "postgres.role",
        Some(json!({"name": "synapse", "login": true, "password": "(write-only)"})),
        json!({"name": "synapse", "login": true, "password": "new-pw"}),
    )
    .unwrap();
    assert!(!p.requires_replace);
    assert_eq!(p.changes.len(), 1);
    assert_eq!(p.changes[0].path, "password");
    assert!(p.changes[0].sensitive);

    apply(
        &h,
        pb::Op::Update,
        "postgres.role",
        "synapse",
        Some(json!({"name": "synapse", "login": true, "password": "new-pw"})),
    );
    assert!(s.accepts("synapse", "new-pw"));
    assert!(!s.accepts("synapse", "old-pw"));
    let sent: Vec<String> = s
        .queries()
        .into_iter()
        .filter(|q| q.contains("PASSWORD"))
        .collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent.iter().all(|q| q.contains("'SCRAM-SHA-256$4096:")),
        "{sent:?}"
    );
    assert!(
        !s.queries()
            .iter()
            .any(|q| q.contains("new-pw") || q.contains("old-pw")),
        "{:?}",
        s.queries()
    );
    // The role logs in with it (the fake checks SCRAM as the server does).
    let as_synapse = configured(
        json!({"host": "127.0.0.1", "port": s.port, "user": "synapse",
                                        "password": "new-pw", "sslmode": "disable"}),
    );
    assert!(read(&as_synapse, "postgres.role", "synapse").is_some());
}

/// A provider whose role may not read `pg_authid` cannot tell the
/// password it keeps: it sends one each update that carries a password.
#[test]
fn without_pg_authid_a_password_is_sent() {
    let s = Server::start();
    let h = configured(admin(&s));
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "ops", "login": true, "createrole": true, "password": "ops-pw"})),
    );
    let ops = configured(json!({"host": "127.0.0.1", "port": s.port, "user": "ops",
                                "password": "ops-pw", "sslmode": "disable"}));
    apply(
        &ops,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "app", "password": "a"})),
    );
    let n = |s: &Server| {
        s.queries()
            .iter()
            .filter(|q| q.contains("PASSWORD"))
            .count()
    };
    let before = n(&s);
    apply(
        &ops,
        pb::Op::Update,
        "postgres.role",
        "app",
        Some(json!({"name": "app", "password": "a"})),
    );
    assert_eq!(n(&s), before + 1);
    assert!(s.accepts("app", "a"));
}

/// The role the provider connects as is not one it manages: Plan refuses
/// it after the address, Apply refuses before anything changes, and so
/// does a delete.
#[test]
fn the_providers_own_role_is_refused() {
    let s = Server::start();
    let h = configured(admin(&s));
    let e = plan(
        &h,
        "postgres.role",
        None,
        json!({"name": ADMIN, "password": "x"}),
    )
    .unwrap_err();
    let e = e.to_string();
    assert!(e.contains(r#"plan postgres.role["it"]: name: "dform_admin" is the role provider postgres connects as"#), "{e}");
    assert!(e.contains("separate admin role"), "{e}");
    let e = try_apply(
        &h,
        pb::Op::Update,
        "postgres.role",
        ADMIN,
        Some(json!({"name": ADMIN, "password": "x"})),
        "",
    )
    .unwrap_err();
    assert!(e.to_string().contains("connects as"), "{e}");
    let e = try_apply(&h, pb::Op::Delete, "postgres.role", ADMIN, None, "").unwrap_err();
    assert!(e.to_string().contains("connects as"), "{e}");
    assert!(s.accepts(ADMIN, ADMIN_PASSWORD));
    assert!(
        !s.queries()
            .iter()
            .any(|q| q.contains("ALTER") || q.contains("DROP"))
    );
}

/// A database made from template0 with its owner, encoding and locale;
/// adopted; its owner changed in place; an encoding as the server does
/// not name it refused at Plan; dropped.
#[test]
fn a_database_through_its_life() {
    let s = Server::start();
    let h = configured(admin(&s));
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse"})),
    );
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "other"})),
    );
    let made = apply(
        &h,
        pb::Op::Create,
        "postgres.database",
        "",
        Some(
            json!({"name": "synapse", "owner": "synapse", "encoding": "UTF8",
                    "lc_collate": "C", "lc_ctype": "C"}),
        ),
    );
    assert_eq!(made.remote, "synapse");
    let d = &s.world().databases["synapse"];
    assert_eq!(
        (d.owner.as_str(), d.collate.as_str(), d.ctype.as_str()),
        ("synapse", "C", "C")
    );
    assert!(
        s.queries()
            .iter()
            .any(|q| q.starts_with("CREATE DATABASE \"synapse\" WITH TEMPLATE template0")),
        "{:?}",
        s.queries()
    );
    let (attrs, computed) = read(&h, "postgres.database", "synapse").unwrap();
    assert_eq!(attrs, json!({"name": "synapse"}));
    assert_eq!(computed["owner"], json!("synapse"));
    assert_eq!(computed["encoding"], json!("UTF8"));

    // A plan: the owner in place, the encoding replaces.
    let prior = json!({"name": "synapse", "owner": "synapse", "encoding": "UTF8"});
    let p = plan(
        &h,
        "postgres.database",
        Some(prior.clone()),
        json!({"name": "synapse", "owner": "other", "encoding": "UTF8"}),
    )
    .unwrap();
    assert!(!p.requires_replace);
    let p = plan(
        &h,
        "postgres.database",
        Some(prior),
        json!({"name": "synapse", "encoding": "LATIN1"}),
    )
    .unwrap();
    assert!(p.requires_replace);
    let e = plan(
        &h,
        "postgres.database",
        None,
        json!({"name": "x", "encoding": "utf-8"}),
    )
    .unwrap_err();
    assert!(
        e.to_string()
            .contains(r#"write "UTF8", as the server names it"#),
        "{e}"
    );

    apply(
        &h,
        pb::Op::Update,
        "postgres.database",
        "synapse",
        Some(json!({"name": "synapse", "owner": "other"})),
    );
    assert_eq!(s.world().databases["synapse"].owner, "other");

    // Made elsewhere: read, and adopted with no change.
    s.with_world(|w| {
        let mut d = w.databases["synapse"].clone();
        d.comment = None;
        w.databases.insert("legacy".into(), d);
    });
    let adopted = apply(
        &h,
        pb::Op::Adopt,
        "postgres.database",
        "legacy",
        Some(json!({"name": "legacy"})),
    );
    assert_eq!(adopted.remote, "legacy");

    // A role that owns a database is not dropped: the server says why.
    let e = try_apply(&h, pb::Op::Delete, "postgres.role", "other", None, "").unwrap_err();
    assert!(
        e.to_string()
            .contains("cannot be dropped because some objects depend on it"),
        "{e}"
    );
    apply(&h, pb::Op::Delete, "postgres.database", "synapse", None);
    assert!(read(&h, "postgres.database", "synapse").is_none());
    let e = try_apply(
        &h,
        pb::Op::Delete,
        "postgres.database",
        "postgres",
        None,
        "",
    )
    .unwrap_err();
    assert!(
        e.to_string()
            .contains("the database provider postgres connects to"),
        "{e}"
    );
}

/// TLS: `require` (the default) encrypts and takes any certificate;
/// `verify-full` checks the chain against `root_cert` and the name; a
/// server with no TLS is refused under `require`, never fallen back
/// from.
#[test]
fn tls_as_sslmode_says() {
    let s = Server::start();
    let mut settings = admin(&s);
    settings.as_object_mut().unwrap().remove("sslmode");
    let h = configured(settings.clone());
    let e = h
        .handle(
            pb::ReadRequest {
                r#type: "postgres.role".into(),
                remote: ADMIN.into(),
                name: "it".into(),
            }
            .into(),
            &silent,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("retryable: connect to dform_admin@127.0.0.1"),
        "{e}"
    );
    assert!(s.logins().is_empty());

    s.tls();
    let h = configured(settings.clone());
    assert!(read(&h, "postgres.role", ADMIN).is_some());
    settings["sslmode"] = json!("verify-full");
    settings["root_cert"] = json!(fake::CA);
    settings["host"] = json!("localhost");
    let h = configured(settings.clone());
    assert!(read(&h, "postgres.role", ADMIN).is_some());
    assert_eq!(
        s.logins(),
        [(ADMIN.to_string(), true), (ADMIN.to_string(), true)]
    );
    // Checked against roots that did not sign it: refused.
    settings["root_cert"] = json!(include_str!("other-ca.pem"));
    let h = configured(settings);
    assert!(
        h.handle(
            pb::ReadRequest {
                r#type: "postgres.role".into(),
                remote: ADMIN.into(),
                name: "it".into()
            }
            .into(),
            &silent
        )
        .is_err()
    );
}

/// A service reached through the Kubernetes API's port-forward: the
/// service's selector and named target port, its ready pod, and the
/// statements over the forward.
#[test]
fn a_service_through_the_kubernetes_api() {
    let s = Server::start();
    let kube = fake::kube::Kube::start("apps", "synapse-db", s.port);
    let h = configured(json!({
        "host": "synapse-db.apps.svc", "port": 5432, "user": ADMIN, "password": ADMIN_PASSWORD,
        "sslmode": "disable", "kubeconfig": kube.kubeconfig(),
    }));
    apply(
        &h,
        pb::Op::Create,
        "postgres.role",
        "",
        Some(json!({"name": "synapse", "password": "pw"})),
    );
    assert!(s.accepts("synapse", "pw"));
    let seen = kube.seen();
    assert!(
        seen.iter()
            .any(|r| r == "GET /api/v1/namespaces/apps/services/synapse-db"),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|r| r.starts_with("GET /api/v1/namespaces/apps/pods/synapse-db-0/portforward")),
        "{seen:?}"
    );
}

/// What a verifier is, as the provider computes it and the fake checks.
#[test]
fn the_fake_logs_in_with_a_verifier_the_provider_computed() {
    let s = Server::start();
    s.with_world(|w| {
        w.roles.insert(
            "x".into(),
            fake::Role {
                login: true,
                verifier: Some(scram::verifier("p")),
                ..fake::Role::default()
            },
        );
    });
    let h = configured(
        json!({"host": "127.0.0.1", "port": s.port, "user": "x", "password": "p",
                              "sslmode": "disable"}),
    );
    assert!(read(&h, "postgres.role", "x").is_some());
    let h = configured(
        json!({"host": "127.0.0.1", "port": s.port, "user": "x", "password": "q",
                              "sslmode": "disable"}),
    );
    let e = h
        .handle(
            pb::ReadRequest {
                r#type: "postgres.role".into(),
                remote: "x".into(),
                name: "it".into(),
            }
            .into(),
            &silent,
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("password authentication failed"),
        "{e}"
    );
}
