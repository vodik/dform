//! `dform provider check PATH`: the conformance suite. It starts the
//! provider at PATH, configures it with a synthetic schema (`check.thing`),
//! extern answers and an inventory in a scratch directory, and runs every
//! method of the protocol against it, printing one line per check and
//! whether the provider deviates. The mock provider passes it. A provider
//! that serves its own schema (a real API's) instead of the one it is given
//! is checked with the documents its Schema offers as `examples`; one that
//! does neither stops after Schema, with that as the deviation.

use super::backend::CallError;
use super::link::Link;
use super::pb;
use super::providers::Launch;
use super::source::{self, Source};
use super::wire;
use crate::provider::{diff, flatten, get_path, remove_path, set_path};
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use std::cell::RefCell;
use std::path::Path;

const TYPE: &str = "check.thing";

const SCHEMA: &str = r#"# The synthetic schema `dform provider check` configures a provider with.
edition 2026

type_provider(check.thing, "check")
type_attr(check.thing, "id", "string", ["computed", "id"])
type_attr(check.thing, "endpoint", "string", ["computed"])
type_attr(check.thing, "token", "string", ["computed", "sensitive"])
type_attr(check.thing, "name", "string", ["required"])
type_attr(check.thing, "zone", "string", ["force_new"])
type_attr(check.thing, "password", "string", ["sensitive"])
type_attr(check.thing, "ports", "list", [])
type_list_key(check.thing, "ports", ["name"])
type_retry(check.thing, 1)
"#;

const EXTERNS: &str = r#"edition 2026

check.lookup("a", "1")
check.lookup("a", "one")
check.lookup("b", "2")
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

/// A started provider, called through `&`.
type Conn = RefCell<Link>;

fn call<R>(conn: &Conn, c: impl Into<super::backend::Call>) -> Result<R>
where
    R: TryFrom<super::backend::Reply, Error = super::backend::Reply>,
{
    conn.borrow_mut().call(c)
}

/// Run the suite against the provider at `path` (a plugin, else, a mock
/// schema, the mock), reached through `launch`. Returns the report's lines
/// and how many checks failed.
pub fn run(launch: &dyn Launch, path: &str) -> Result<(Vec<String>, usize)> {
    let dir = std::env::temp_dir().join(format!("dform-provider-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let start = || match source::resolve(path) {
        Source::Plugin(p) => launch.plugin(&p),
        Source::Mock(_) => launch.mock(),
    };
    let out = suite(start, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn suite(start: impl FnOnce() -> Result<Link>, dir: &Path) -> Result<(Vec<String>, usize)> {
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

    let conn = match start() {
        Ok(c) => RefCell::new(c),
        Err(e) => {
            r.check("Handshake", Err(e));
            return Ok((r.lines, r.failed));
        }
    };
    r.check(
        "Handshake",
        ensure(!conn.borrow().name.is_empty(), || {
            "the handshake names no provider".into()
        }),
    );
    let config = json!({
        "schemas": [dir.join("schema.df").display().to_string()],
        "world": dir.join("world.json").display().to_string(),
        "inventory": dir.join("inventory.json").display().to_string(),
        "chaos": [],
    });
    let configured = call::<pb::ConfigureResponse>(
        &conn,
        pb::ConfigureRequest {
            config: Some(wire::doc(&config)),
        },
    )
    .map(|_| ());
    let ok = configured.is_ok();
    r.check("Configure", configured);
    if !ok {
        return Ok((r.lines, r.failed));
    }

    // Schema: the synthetic type, as facts the engine can read.
    let schema = call::<pb::SchemaResponse>(&conn, pb::SchemaRequest::default()).and_then(|resp| {
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
    let fixture = if served {
        r.check("Schema serves the synthetic type", Ok(()));
        Fixture::synthetic()
    } else if !resp.examples.is_empty() {
        match Fixture::examples(&schema, &resp.examples) {
            Ok(f) => {
                r.check("Schema serves its own types, with examples", Ok(()));
                f
            }
            Err(e) => {
                r.check("Schema's examples are documents of its types", Err(e));
                return Ok((r.lines, r.failed));
            }
        }
    } else {
        r.check(
            "Schema serves the synthetic type",
            Err(anyhow::anyhow!(
                "{TYPE} is not in the schema as configured (id fresh, token secret, ports keyed \
                 by name, zone force_new), and the schema offers no examples of its own types; \
                 the resource checks need one or the other"
            )),
        );
        return Ok((r.lines, r.failed));
    };

    // Query: bound inputs in, every matching row out, streamed.
    if conn.borrow().has("fact") {
        r.check(
            "Schema declares the externs Query answers",
            ensure(
                resp.externs.iter().any(|e| e.pred == "check.lookup"),
                || "no extern check.lookup".into(),
            ),
        );
        let q = |input: &str| {
            call::<Vec<pb::Row>>(
                &conn,
                pb::QueryRequest {
                    pred: "check.lookup".into(),
                    input: vec![true, false],
                    inputs: vec![wire::value(&Value::Str(input.into()))],
                },
            )
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
    if !conn.borrow().has("resource") {
        r.skip("Read, Plan, Apply, Import", "no `resource` capability");
        return Ok((r.lines, r.failed));
    }
    resources(&conn, &mut r, &schema, &fixture);
    Ok((r.lines, r.failed))
}

/// What the resource checks exercise: the synthetic type, or the examples
/// of a provider that serves its own schema.
struct Fixture {
    /// The type the Apply checks create, update, replace and delete.
    typ: String,
    doc: Json,
    /// `doc` changed in place.
    renamed: Json,
    /// `doc` with a `force_new` path changed; the Apply REPLACE document.
    moved: Option<Json>,
    /// A document Plan refuses, its type, and the attribute it lacks.
    missing: Option<(String, Json, String)>,
    /// A create whose Plan spells a keyed list by key: its type, and the
    /// path prefix a change must start with.
    keyed: Option<(String, Json, String)>,
    /// A create with a sensitive leaf: its type, and that leaf's path.
    sensitive: Option<(String, Json, String)>,
    /// The computed identity's path, and a sensitive computed path.
    id: String,
    secret: Option<String>,
    /// The synthetic type: the inventory and the assertions apply.
    synthetic: bool,
}

impl Fixture {
    fn synthetic() -> Fixture {
        let doc = json!({"name": "a", "zone": "z1", "password": "hunter2",
                         "ports": [{"name": "http", "port": 80}]});
        let mut renamed = doc.clone();
        renamed["name"] = json!("b");
        let mut moved = doc.clone();
        moved["zone"] = json!("z2");
        Fixture {
            typ: TYPE.into(),
            missing: Some((TYPE.into(), json!({"zone": "z1"}), "name".into())),
            keyed: Some((TYPE.into(), doc.clone(), "ports[".into())),
            sensitive: Some((TYPE.into(), doc.clone(), "password".into())),
            doc,
            renamed,
            moved: Some(moved),
            id: "id".into(),
            secret: Some("token".into()),
            synthetic: true,
        }
    }

    /// The first example is the Apply checks' object; each Plan check takes
    /// the first example that has what it checks.
    fn examples(schema: &Schema, examples: &[pb::Example]) -> Result<Fixture> {
        let mut docs = Vec::new();
        for e in examples {
            ensure(schema.knows_type(&e.r#type), || {
                format!(
                    "example of {}, a type the schema does not declare",
                    e.r#type
                )
            })?;
            let create = wire::from_doc_or_empty(e.create.as_ref())?;
            let update = wire::from_doc_or_empty(e.update.as_ref())?;
            docs.push((e.r#type.clone(), create, update, e.required.clone()));
        }
        let (typ, doc, renamed, _) = docs[0].clone();
        let computed = |class: NullClass| {
            schema
                .computed_of(&typ)
                .into_iter()
                .find(|(p, c)| *c == class && !schema.in_list(&typ, p))
                .map(|(p, _)| p)
        };
        let id = computed(NullClass::Fresh)
            .ok_or_else(|| anyhow::anyhow!("{typ} has no computed identity (`computed, id`)"))?;
        let missing = docs.iter().find(|d| !d.3.is_empty()).map(|(t, d, _, p)| {
            let mut d = d.clone();
            remove_path(&mut d, p);
            (t.clone(), d, p.clone())
        });
        let keyed = docs.iter().find_map(|(t, d, _, _)| {
            schema
                .list_keys
                .keys()
                .filter(|(lt, _)| lt == t)
                .find(|(_, p)| {
                    get_path(d, p)
                        .and_then(Json::as_array)
                        .is_some_and(|a| !a.is_empty())
                })
                .map(|(_, p)| (t.clone(), d.clone(), format!("{p}[")))
        });
        let sensitive = docs.iter().find_map(|(t, d, _, _)| {
            let mut leaves = std::collections::BTreeMap::new();
            flatten(schema, t, d, "", "", true, &mut leaves);
            leaves
                .into_iter()
                .find(|(_, (_, norm))| schema.is_sensitive(t, norm))
                .map(|(p, _)| (t.clone(), d.clone(), p))
        });
        Ok(Fixture {
            secret: computed(NullClass::Secret),
            typ,
            doc,
            renamed,
            moved: None,
            missing,
            keyed,
            sensitive,
            id,
            synthetic: false,
        })
    }
}

/// An object's configured attributes as the engine compares them with a
/// desired document: plus each Optional+Computed value (from `computed`)
/// the document sets.
fn world_doc(schema: &Schema, typ: &str, attrs: &Json, computed: &Json, doc: &Json) -> Json {
    let mut out = attrs.clone();
    for (p, _) in schema.optional_computed_of(typ) {
        if get_path(doc, &p).is_some()
            && get_path(&out, &p).is_none()
            && let Some(v) = get_path(computed, &p)
        {
            set_path(&mut out, &p, v.clone());
        }
    }
    out
}

fn read(conn: &Conn, typ: &str, remote: &str) -> Result<Option<(Json, Json)>> {
    let req = pb::ReadRequest {
        r#type: typ.into(),
        remote: remote.into(),
        name: "a".into(),
    };
    let resp: pb::ReadResponse = call(conn, req)?;
    if !resp.found {
        return Ok(None);
    }
    Ok(Some((
        wire::from_doc_or_empty(resp.attrs.as_ref())?,
        wire::from_doc_or_empty(resp.computed.as_ref())?,
    )))
}

fn plan(
    conn: &Conn,
    typ: &str,
    prior: Option<&Json>,
    desired: Option<&Json>,
) -> Result<pb::PlanResponse> {
    let req = pb::PlanRequest {
        r#type: typ.into(),
        name: "a".into(),
        prior: prior.map(wire::doc),
        desired: desired.map(wire::doc),
        remote: String::new(),
    };
    call(conn, req)
}

fn apply(
    conn: &Conn,
    op: pb::Op,
    typ: &str,
    remote: &str,
    config: Option<&Json>,
    assertions: Vec<pb::Assertion>,
) -> std::result::Result<pb::ApplyResponse, CallError> {
    let req = pb::ApplyRequest {
        op: op as i32,
        r#type: typ.into(),
        name: "a".into(),
        remote: remote.into(),
        config: config.map(wire::doc),
        assertions,
        ..Default::default()
    };
    conn.borrow_mut().try_call(req)
}

/// A sensitive computed value leaves the provider as a secret null only.
fn secret_is_label(computed: &Json, path: &str) -> Result<()> {
    let v = get_path(computed, path);
    ensure(
        matches!(
            v.and_then(crate::provider::marker),
            Some((crate::provider::SECRET_KEY, _))
        ),
        || format!("the sensitive computed {path} is {v:?}, not its label"),
    )
}

fn resources(conn: &Conn, r: &mut Report, schema: &Schema, f: &Fixture) {
    let typ = f.typ.as_str();
    let doc = &f.doc;
    // The object as Read or Apply returns it, compared as the engine does.
    let seen = |attrs: &Json, computed: &Json, want: &Json| {
        let got = world_doc(schema, typ, attrs, computed, want);
        ensure(&got == want, || format!("attrs {got} are not {want}"))
    };
    let secret = |computed: &Json| match &f.secret {
        Some(p) => secret_is_label(computed, p),
        None => Ok(()),
    };
    r.check(
        "Read of an id nothing has answers not found",
        read(conn, typ, "no-such-object")
            .and_then(|o| ensure(o.is_none(), || format!("got {o:?}"))),
    );

    // Plan: validation, the diff, requires_replace.
    match &f.missing {
        Some((t, d, path)) => r.check(
            "Plan refuses a document without a required attribute",
            match plan(conn, t, None, Some(d)) {
                Ok(_) => Err(anyhow::anyhow!("a document without `{path}` was planned")),
                Err(e) => ensure(format!("{e:#}").contains(path.as_str()), || {
                    format!("the refusal does not name the attribute: {e:#}")
                }),
            },
        ),
        None => r.skip(
            "Plan refuses a document without a required attribute",
            "no example names a required attribute",
        ),
    }
    r.check(
        "Plan of a create is every leaf, not a replace",
        plan(conn, typ, None, Some(doc)).and_then(|p| {
            ensure(!p.requires_replace, || "a create requires replace".into())?;
            let want: Vec<String> = diff(schema, typ, None, Some(doc))
                .into_iter()
                .map(|c| c.path)
                .collect();
            let paths: Vec<&str> = p.changes.iter().map(|c| c.path.as_str()).collect();
            ensure(want.iter().all(|w| paths.contains(&w.as_str())), || {
                format!("changes {paths:?} are not every leaf {want:?}")
            })
        }),
    );
    match &f.keyed {
        Some((t, d, prefix)) => r.check(
            "Plan spells a keyed list by key",
            plan(conn, t, None, Some(d)).and_then(|p| {
                let paths: Vec<&str> = p.changes.iter().map(|c| c.path.as_str()).collect();
                let by_key = |x: &&str| {
                    x.starts_with(prefix.as_str())
                        && x[prefix.len()..]
                            .split(']')
                            .next()
                            .is_some_and(|k| k.contains('='))
                };
                ensure(paths.iter().any(by_key), || {
                    format!("a keyed list is not spelled by key: {paths:?}")
                })
            }),
        ),
        None => r.skip(
            "Plan spells a keyed list by key",
            "no example sets a keyed list",
        ),
    }
    match &f.sensitive {
        Some((t, d, path)) => r.check(
            "Plan marks a sensitive attribute sensitive",
            plan(conn, t, None, Some(d)).and_then(|p| {
                ensure(
                    p.changes.iter().any(|c| &c.path == path && c.sensitive),
                    || format!("the sensitive {path} is not marked sensitive"),
                )
            }),
        ),
        None => r.skip(
            "Plan marks a sensitive attribute sensitive",
            "no example sets a sensitive attribute",
        ),
    }
    match &f.moved {
        Some(moved) => r.check(
            "Plan of a force_new change requires replace",
            plan(conn, typ, Some(doc), Some(moved))
                .and_then(|p| ensure(p.requires_replace, || "zone is force_new".into())),
        ),
        None => r.skip(
            "Plan of a force_new change requires replace",
            "the examples change no force_new path",
        ),
    }
    let renamed = &f.renamed;
    r.check(
        "Plan of an in-place change does not replace",
        plan(conn, typ, Some(doc), Some(renamed)).and_then(|p| {
            ensure(!p.requires_replace, || {
                "an in-place change requires replace".into()
            })?;
            let want: Vec<String> = diff(schema, typ, Some(doc), Some(renamed))
                .into_iter()
                .map(|c| c.path)
                .collect();
            let got: Vec<&str> = p.changes.iter().map(|c| c.path.as_str()).collect();
            ensure(!want.is_empty() && got == want, || {
                format!("changes {got:?}, not {want:?}")
            })
        }),
    );

    // Apply: a create mints computed values and hides the secret.
    let created = apply(conn, pb::Op::Create, typ, "", Some(doc), vec![]);
    let remote = match &created {
        Ok(resp) => resp.remote.clone(),
        Err(_) => String::new(),
    };
    r.check(
        "Apply CREATE returns the object with its computed values",
        created.map_err(anyhow::Error::new).and_then(|resp| {
            ensure(!resp.remote.is_empty(), || "no remote id".into())?;
            let computed = wire::from_doc_or_empty(resp.computed.as_ref())?;
            ensure(
                get_path(&computed, &f.id).is_some_and(Json::is_string),
                || format!("no computed {}: {computed}", f.id),
            )?;
            secret(&computed)?;
            let attrs = wire::from_doc_or_empty(resp.attrs.as_ref())?;
            seen(&attrs, &computed, doc)
        }),
    );
    if remote.is_empty() {
        return;
    }
    r.check(
        "Read returns what Apply created",
        read(conn, typ, &remote).and_then(|o| {
            let (attrs, computed) = o.ok_or_else(|| anyhow::anyhow!("not found"))?;
            seen(&attrs, &computed, doc)?;
            secret(&computed)
        }),
    );
    r.check(
        "Import answers a managed object by remote id",
        import(conn, typ, &remote).and_then(|o| ensure(o.is_some(), || "not found".into())),
    );
    if f.synthetic && conn.borrow().has("inventory") {
        r.check(
            "Import answers an inventory object",
            import(conn, typ, "found").and_then(|o| {
                let (attrs, computed) = o.ok_or_else(|| anyhow::anyhow!("not found"))?;
                ensure(attrs["zone"] == "z1", || format!("attrs {attrs}"))?;
                secret(&computed)
            }),
        );
    }

    // Apply UPDATE keeps the identity.
    r.check(
        "Apply UPDATE changes the object in place",
        apply(conn, pb::Op::Update, typ, &remote, Some(renamed), vec![])
            .map_err(anyhow::Error::new)
            .and_then(|resp| {
                ensure(resp.remote == remote, || {
                    format!("remote {} is not {remote}", resp.remote)
                })?;
                let (attrs, computed) = read(conn, typ, &remote)?
                    .ok_or_else(|| anyhow::anyhow!("gone after update"))?;
                seen(&attrs, &computed, renamed)
            }),
    );

    // Assertions (F DR-13): a failing one refuses the action.
    let schema_resp = call::<pb::SchemaResponse>(conn, pb::SchemaRequest::default());
    if f.synthetic && schema_resp.is_ok_and(|s| s.checks_refinements) {
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
                typ,
                &remote,
                Some(doc),
                vec![assertion("len_ge", json!(64))],
            ) {
                Ok(_) => Err(anyhow::anyhow!("applied despite a failing assertion")),
                Err(CallError::Refused(_)) => read(conn, typ, &remote).and_then(|o| {
                    let (attrs, _) = o.ok_or_else(|| anyhow::anyhow!("gone"))?;
                    ensure(&attrs == renamed, || format!("attrs changed to {attrs}"))
                }),
                Err(e) => Err(anyhow::anyhow!("not a refusal: {e}")),
            },
        );
        r.check(
            "Apply applies an action whose assertions hold",
            apply(
                conn,
                pb::Op::Update,
                typ,
                &remote,
                Some(renamed),
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
    let moved = f.moved.as_ref().unwrap_or(doc);
    let replaced = apply(conn, pb::Op::Replace, typ, &remote, Some(moved), vec![]);
    let new_remote = replaced.as_ref().map(|x| x.remote.clone()).ok();
    r.check(
        "Apply REPLACE makes a new object",
        replaced.map_err(anyhow::Error::new).and_then(|resp| {
            let (attrs, computed) = read(conn, typ, &resp.remote)?
                .ok_or_else(|| anyhow::anyhow!("the new object is not there"))?;
            seen(&attrs, &computed, moved)
        }),
    );
    let remote = new_remote.unwrap_or(remote);
    r.check(
        "Apply DELETE removes the object",
        apply(conn, pb::Op::Delete, typ, &remote, None, vec![])
            .map_err(anyhow::Error::new)
            .and_then(|_| {
                let o = read(conn, typ, &remote)?;
                ensure(o.is_none(), || format!("still there: {o:?}"))
            }),
    );
    r.check(
        "Apply END_TICK ends the tick",
        call::<pb::ApplyResponse>(
            conn,
            pb::ApplyRequest {
                op: pb::Op::EndTick as i32,
                ..Default::default()
            },
        )
        .map(|_| ()),
    );
}

fn import(conn: &Conn, typ: &str, remote: &str) -> Result<Option<(Json, Json)>> {
    let req = pb::ImportRequest {
        r#type: typ.into(),
        remote: remote.into(),
    };
    let resp: pb::ImportResponse = call(conn, req)?;
    if !resp.found {
        return Ok(None);
    }
    Ok(Some((
        wire::from_doc_or_empty(resp.attrs.as_ref())?,
        wire::from_doc_or_empty(resp.computed.as_ref())?,
    )))
}
