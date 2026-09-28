//! `dform provider check PATH`: the conformance suite. It starts the
//! provider at PATH, configures it with a synthetic schema (`check.thing`),
//! extern answers and an inventory in a scratch directory, and runs every
//! method of the protocol against it, printing one line per check and
//! whether the provider deviates. The mock provider passes it; a provider
//! that plays no schema it is given stops after Schema, with that as the
//! deviation.

use super::client::{CallError, Conn};
use super::pb;
use super::spawn::{self, Source};
use super::wire;
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use std::path::{Path, PathBuf};

const TYPE: &str = "check.thing";

const SCHEMA: &str = r#"# The synthetic schema `dform provider check` configures a provider with.
edition 2026.

type_provider(check.thing, check).
type_attr(check.thing, id, string, [computed, id]).
type_attr(check.thing, endpoint, string, [computed]).
type_attr(check.thing, token, string, [computed, sensitive]).
type_attr(check.thing, name, string, [required]).
type_attr(check.thing, zone, string, [force_new]).
type_attr(check.thing, password, string, [sensitive]).
type_attr(check.thing, ports, list, []).
type_list_key(check.thing, ports, [name]).
type_retry(check.thing, 1).
"#;

const EXTERNS: &str = r#"edition 2026.

check.lookup("a", "1").
check.lookup("a", "one").
check.lookup("b", "2").
"#;

/// One line of the report.
struct Report {
    lines: Vec<String>,
    failed: usize,
}

impl Report {
    fn check(&mut self, what: &str, r: Result<()>) {
        match r {
            Ok(()) => self.lines.push(format!("ok    {what}")),
            Err(e) => {
                self.failed += 1;
                self.lines.push(format!("FAIL  {what}: {e:#}"));
            }
        }
    }

    fn skip(&mut self, what: &str, why: &str) {
        self.lines.push(format!("skip  {what}: {why}"));
    }
}

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(anyhow::anyhow!(msg()))
    }
}

/// The executable PATH names: a plugin, else (a mock schema) the mock.
pub fn executable(path: &str) -> Result<PathBuf> {
    match spawn::resolve(path) {
        Source::Plugin(p) => Ok(p),
        Source::Mock(_) => spawn::fake_executable(),
    }
}

/// Run the suite against the provider at `path`. Returns the report's
/// lines and how many checks failed.
pub fn run(path: &str) -> Result<(Vec<String>, usize)> {
    let exe = executable(path)?;
    let dir = std::env::temp_dir().join(format!("dform-provider-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let out = suite(&exe, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn suite(exe: &Path, dir: &Path) -> Result<(Vec<String>, usize)> {
    let mut r = Report {
        lines: Vec::new(),
        failed: 0,
    };
    std::fs::write(dir.join("schema.df"), SCHEMA)?;
    std::fs::write(dir.join("externs.df"), EXTERNS)?;
    std::fs::write(
        dir.join("inventory.json"),
        serde_json::to_string(&json!({"resources": {"check.thing::found": {
            "typ": TYPE, "name": "found",
            "attrs": {"name": "found", "zone": "z1"},
            "computed": {"id": "found-id", "endpoint": "found.example", "token": "t0ken"}}}}))?,
    )?;

    let conn = match Conn::start(exe) {
        Ok(c) => c,
        Err(e) => {
            r.check("Handshake", Err(e));
            return Ok((r.lines, r.failed));
        }
    };
    r.check(
        "Handshake",
        ensure(!conn.name.is_empty(), || {
            "the handshake names no provider".into()
        }),
    );
    let config = json!({
        "schemas": [dir.join("schema.df").display().to_string()],
        "world": dir.join("world.json").display().to_string(),
        "inventory": dir.join("inventory.json").display().to_string(),
        "chaos": [],
    });
    let configured = conn
        .call(|mut c| async move {
            c.configure(pb::ConfigureRequest {
                config: Some(wire::doc(&config)),
            })
            .await
        })
        .map(|_| ());
    let ok = configured.is_ok();
    r.check("Configure", configured);
    if !ok {
        return Ok((r.lines, r.failed));
    }

    // Schema: the synthetic type, as facts the engine can read.
    let schema = conn
        .call(|mut c| async move { c.schema(pb::SchemaRequest {}).await })
        .and_then(|resp| {
            let facts = resp
                .facts
                .iter()
                .map(wire::from_fact)
                .collect::<Result<Vec<_>>>()?;
            Ok((Schema::from_facts(&facts)?, resp))
        });
    let (schema, resp) = match schema {
        Ok(x) => x,
        Err(e) => {
            r.check("Schema", Err(e));
            return Ok((r.lines, r.failed));
        }
    };
    let served = schema.class_of(TYPE, "id") == Some(NullClass::Fresh)
        && schema.class_of(TYPE, "token") == Some(NullClass::Secret)
        && schema.list_key(TYPE, "ports").is_some()
        && schema.forces_new(TYPE, "zone");
    r.check(
        "Schema serves the synthetic type",
        ensure(served, || {
            format!(
                "{TYPE} is not in the schema as configured (id fresh, token secret, ports keyed \
                 by name, zone force_new); the resource checks need a provider that plays the \
                 schema it is given"
            )
        }),
    );
    if !served {
        return Ok((r.lines, r.failed));
    }

    // Query: bound inputs in, every matching row out, streamed.
    if conn.has("fact") {
        r.check(
            "Schema declares the externs Query answers",
            ensure(
                resp.externs.iter().any(|e| e.pred == "check.lookup"),
                || "no extern check.lookup".into(),
            ),
        );
        let q = |input: &str| {
            conn.query(pb::QueryRequest {
                pred: "check.lookup".into(),
                input: vec![true, false],
                inputs: vec![wire::value(&Value::Str(input.into()))],
            })
            .and_then(|rows| {
                rows.iter()
                    .map(|row| row.values.iter().map(wire::from_value).collect())
                    .collect::<Result<Vec<Vec<Value>>>>()
            })
        };
        r.check(
            "Query answers every row with the bound inputs",
            q("a").and_then(|rows| {
                let want = vec![
                    vec![Value::Str("a".into()), Value::Str("1".into())],
                    vec![Value::Str("a".into()), Value::Str("one".into())],
                ];
                ensure(rows == want, || format!("got {rows:?}"))
            }),
        );
        r.check(
            "Query with no match answers no row",
            q("zzz").and_then(|rows| ensure(rows.is_empty(), || format!("got {rows:?}"))),
        );
    } else {
        r.skip("Query", "no `fact` capability");
    }
    if !conn.has("resource") {
        r.skip("Read, Plan, Apply, Import", "no `resource` capability");
        return Ok((r.lines, r.failed));
    }
    resources(&conn, &mut r);
    Ok((r.lines, r.failed))
}

fn read(conn: &Conn, remote: &str) -> Result<Option<(Json, Json)>> {
    let req = pb::ReadRequest {
        r#type: TYPE.into(),
        remote: remote.into(),
        name: "a".into(),
    };
    let resp = conn.call(|mut c| async move { c.read(req).await })?;
    if !resp.found {
        return Ok(None);
    }
    Ok(Some((
        wire::from_doc_or_empty(resp.attrs.as_ref())?,
        wire::from_doc_or_empty(resp.computed.as_ref())?,
    )))
}

fn plan(conn: &Conn, prior: Option<&Json>, desired: Option<&Json>) -> Result<pb::PlanResponse> {
    let req = pb::PlanRequest {
        r#type: TYPE.into(),
        name: "a".into(),
        prior: prior.map(wire::doc),
        desired: desired.map(wire::doc),
    };
    conn.call(|mut c| async move { c.plan(req).await })
}

fn apply(
    conn: &Conn,
    op: pb::Op,
    remote: &str,
    config: Option<&Json>,
    assertions: Vec<pb::Assertion>,
) -> std::result::Result<pb::ApplyResponse, CallError> {
    let req = pb::ApplyRequest {
        op: op as i32,
        r#type: TYPE.into(),
        name: "a".into(),
        remote: remote.into(),
        config: config.map(wire::doc),
        assertions,
        ..Default::default()
    };
    conn.try_call(|mut c| async move { c.apply(req).await })
}

/// A sensitive computed value leaves the provider as a secret null only.
fn secret_is_label(computed: &Json) -> Result<()> {
    let token = computed.get("token");
    ensure(
        matches!(
            token.and_then(crate::provider::marker),
            Some((crate::provider::SECRET_KEY, _))
        ),
        || format!("the sensitive computed token is {token:?}, not its label"),
    )
}

fn resources(conn: &Conn, r: &mut Report) {
    let doc = json!({"name": "a", "zone": "z1", "password": "hunter2",
                     "ports": [{"name": "http", "port": 80}]});
    r.check(
        "Read of an id nothing has answers not found",
        read(conn, "no-such-object").and_then(|o| ensure(o.is_none(), || format!("got {o:?}"))),
    );

    // Plan: validation, the diff, requires_replace.
    r.check(
        "Plan refuses a document without a required attribute",
        match plan(conn, None, Some(&json!({"zone": "z1"}))) {
            Ok(_) => Err(anyhow::anyhow!("a document without `name` was planned")),
            Err(e) => ensure(format!("{e:#}").contains("name"), || {
                format!("the refusal does not name the attribute: {e:#}")
            }),
        },
    );
    r.check(
        "Plan of a create is every leaf, not a replace",
        plan(conn, None, Some(&doc)).and_then(|p| {
            ensure(!p.requires_replace, || "a create requires replace".into())?;
            let paths: Vec<&str> = p.changes.iter().map(|c| c.path.as_str()).collect();
            ensure(paths.contains(&"ports[name=http].port"), || {
                format!("a keyed list is not spelled by key: {paths:?}")
            })?;
            ensure(
                p.changes
                    .iter()
                    .any(|c| c.path == "password" && c.sensitive),
                || "the sensitive password is not marked sensitive".into(),
            )
        }),
    );
    let moved = json!({"name": "a", "zone": "z2", "password": "hunter2",
                       "ports": [{"name": "http", "port": 80}]});
    r.check(
        "Plan of a force_new change requires replace",
        plan(conn, Some(&doc), Some(&moved))
            .and_then(|p| ensure(p.requires_replace, || "zone is force_new".into())),
    );
    let renamed = json!({"name": "b", "zone": "z1", "password": "hunter2",
                         "ports": [{"name": "http", "port": 80}]});
    r.check(
        "Plan of an in-place change does not replace",
        plan(conn, Some(&doc), Some(&renamed)).and_then(|p| {
            ensure(!p.requires_replace, || "name is not force_new".into())?;
            ensure(p.changes.len() == 1 && p.changes[0].path == "name", || {
                format!("changes {:?}", p.changes)
            })
        }),
    );

    // Apply: a create mints computed values and hides the secret.
    let created = apply(conn, pb::Op::Create, "", Some(&doc), vec![]);
    let remote = match &created {
        Ok(resp) => resp.remote.clone(),
        Err(_) => String::new(),
    };
    r.check(
        "Apply CREATE returns the object with its computed values",
        created.map_err(anyhow::Error::new).and_then(|resp| {
            ensure(!resp.remote.is_empty(), || "no remote id".into())?;
            let computed = wire::from_doc_or_empty(resp.computed.as_ref())?;
            ensure(computed.get("id").is_some_and(Json::is_string), || {
                format!("no computed id: {computed}")
            })?;
            secret_is_label(&computed)?;
            let attrs = wire::from_doc_or_empty(resp.attrs.as_ref())?;
            ensure(attrs == doc, || format!("attrs {attrs} are not the config"))
        }),
    );
    if remote.is_empty() {
        return;
    }
    r.check(
        "Read returns what Apply created",
        read(conn, &remote).and_then(|o| {
            let (attrs, computed) = o.ok_or_else(|| anyhow::anyhow!("not found"))?;
            ensure(attrs == doc, || format!("attrs {attrs}"))?;
            secret_is_label(&computed)
        }),
    );
    r.check(
        "Import answers a managed object by remote id",
        import(conn, &remote).and_then(|o| ensure(o.is_some(), || "not found".into())),
    );
    if conn.has("inventory") {
        r.check(
            "Import answers an inventory object",
            import(conn, "found").and_then(|o| {
                let (attrs, computed) = o.ok_or_else(|| anyhow::anyhow!("not found"))?;
                ensure(attrs["zone"] == "z1", || format!("attrs {attrs}"))?;
                secret_is_label(&computed)
            }),
        );
    }

    // Apply UPDATE keeps the identity.
    r.check(
        "Apply UPDATE changes the object in place",
        apply(conn, pb::Op::Update, &remote, Some(&renamed), vec![])
            .map_err(anyhow::Error::new)
            .and_then(|resp| {
                ensure(resp.remote == remote, || {
                    format!("remote {} is not {remote}", resp.remote)
                })?;
                let (attrs, _) =
                    read(conn, &remote)?.ok_or_else(|| anyhow::anyhow!("gone after update"))?;
                ensure(attrs == renamed, || format!("attrs {attrs}"))
            }),
    );

    // Assertions (F DR-13): a failing one refuses the action.
    let schema = conn.call(|mut c| async move { c.schema(pb::SchemaRequest {}).await });
    if schema.is_ok_and(|s| s.checks_refinements) {
        let assertion = |op: &str, v: Json| pb::Assertion {
            path: "password".into(),
            op: op.into(),
            value: Some(wire::doc(&v)),
            message: format!("password {op} {v}"),
        };
        r.check(
            "Apply refuses an action whose assertion fails, and changes nothing",
            match apply(
                conn,
                pb::Op::Update,
                &remote,
                Some(&doc),
                vec![assertion("len_ge", json!(64))],
            ) {
                Ok(_) => Err(anyhow::anyhow!("applied despite a failing assertion")),
                Err(CallError::Refused(_)) => read(conn, &remote).and_then(|o| {
                    let (attrs, _) = o.ok_or_else(|| anyhow::anyhow!("gone"))?;
                    ensure(attrs == renamed, || format!("attrs changed to {attrs}"))
                }),
                Err(e) => Err(anyhow::anyhow!("not a refusal: {e}")),
            },
        );
        r.check(
            "Apply applies an action whose assertions hold",
            apply(
                conn,
                pb::Op::Update,
                &remote,
                Some(&renamed),
                vec![
                    assertion("len_ge", json!(4)),
                    assertion("prefix", json!("hun")),
                ],
            )
            .map(|_| ())
            .map_err(anyhow::Error::new),
        );
    } else {
        r.skip("Apply assertions", "the schema does not check refinements");
    }

    // REPLACE, destroy first: a new object, the old one gone.
    let replaced = apply(conn, pb::Op::Replace, &remote, Some(&moved), vec![]);
    let new_remote = replaced.as_ref().map(|x| x.remote.clone()).ok();
    r.check(
        "Apply REPLACE makes a new object",
        replaced.map_err(anyhow::Error::new).and_then(|resp| {
            let (attrs, _) = read(conn, &resp.remote)?
                .ok_or_else(|| anyhow::anyhow!("the new object is not there"))?;
            ensure(attrs == moved, || format!("attrs {attrs}"))
        }),
    );
    let remote = new_remote.unwrap_or(remote);
    r.check(
        "Apply DELETE removes the object",
        apply(conn, pb::Op::Delete, &remote, None, vec![])
            .map_err(anyhow::Error::new)
            .and_then(|_| {
                let o = read(conn, &remote)?;
                ensure(o.is_none(), || format!("still there: {o:?}"))
            }),
    );
    r.check(
        "Apply END_TICK ends the tick",
        conn.call(|mut c| async move {
            c.apply(pb::ApplyRequest {
                op: pb::Op::EndTick as i32,
                ..Default::default()
            })
            .await
        })
        .map(|_| ()),
    );
}

fn import(conn: &Conn, remote: &str) -> Result<Option<(Json, Json)>> {
    let req = pb::ImportRequest {
        r#type: TYPE.into(),
        remote: remote.into(),
    };
    let resp = conn.call(|mut c| async move { c.import(req).await })?;
    if !resp.found {
        return Ok(None);
    }
    Ok(Some((
        wire::from_doc_or_empty(resp.attrs.as_ref())?,
        wire::from_doc_or_empty(resp.computed.as_ref())?,
    )))
}
