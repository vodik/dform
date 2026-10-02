//! The fake provider: a mock cloud driven by provider schemas and a world
//! file, answering the provider protocol one call at a time (`Mock`, a
//! `Handler`). `dform-provider-fake` serves it over gRPC; the direct and
//! wire backends link it in (`Linked`). dform talks to it as to any
//! provider.
//!
//! The world file (`--world`, default `dform.state/<stack>/remote.json`, given
//! at Configure) is what "exists": each object's configured `attrs` and its
//! `computed` values. Read answers from it; Apply writes it back after every
//! call. The inventory file is what Query answers `cloud_exists/2`,
//! `cloud_attr/4` and `cloud_computed/4` with, and what Import and ADOPT
//! find an object dform does not manage in.
//!
//! Apply mints computed values per the schema (`type_mint` or a default by
//! class and type). A sensitive computed value stays in the world; what
//! Read and Apply hand back is its label, `{"$secret": "T/N#Attr"}`. A
//! secret label in an Apply document is materialized inside the call and
//! kept as the object's `materialized`: another stack's (`"held"`) from that
//! deployment's world (`worlds` at Configure), which must have it. An
//! extern's secret column is answered with where the mock keeps it (the
//! world's `held`), never the value (`FakeCloud::hold`).
//!
//! The program's settings (a `provider` block's, `provider_config`) arrive
//! at a second Configure as `settings`; the mock reports `settings.account`
//! as the account its credentials reach, for `expect_account`.
//!
//! Chaos knobs (`dform dev --chaos`, `chaos`) arrive at Configure. The world's
//! clock advances at every END_TICK, when chaos `mutate` lands.

use anyhow::{Context, Result, anyhow, bail};
use dform_core::ast::{Atom, Term};
use dform_core::chaos::Chaos;
use dform_core::ir::Address;
use dform_core::plugin::backend::{self, CallError, Handler, Reply, VERSION};
use dform_core::plugin::link::Link;
use dform_core::plugin::pb;
use dform_core::plugin::providers::{CREATED, INVENTORY, Launch};
use dform_core::plugin::queue::{Order, Queue};
use dform_core::plugin::wire;
use dform_core::provider::{self, Change, get_path, norm_path, set_path, short_hash};
use dform_core::schema::Schema;
use dform_core::value::{NullClass, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteState {
    /// The world's clock: every apply is one tick. Chaos knobs count in ticks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tick: u64,
    pub resources: BTreeMap<String, RemoteResource>,
    /// The last tick's Apply calls in simulated time, in the order they
    /// started, when chaos `latency` put one on the clock: what
    /// `--parallel` overlapped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<Span>,
    /// Chaos `fresh-ids`: how many Creates have minted under it. Each salts
    /// its minted values with the next serial.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub serial: u64,
    /// The secrets the mock answered an extern's secret column with, kept
    /// here (the provider's own store, as a real one keeps them in a secret
    /// manager) and handed out as where they are held: by
    /// `key(pred, inputs)`, column (from 1) -> value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held: BTreeMap<String, BTreeMap<String, Json>>,
}

/// One Apply call on the simulated clock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Span {
    pub addr: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResource {
    pub typ: String,
    pub name: String,
    pub attrs: Json,
    pub computed: Json,
    /// Chaos `read-lag`: how many more Reads return nothing for this
    /// resource (eventual consistency after Create).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_lag: Option<u64>,
    /// The idempotency key of the Create or Replace that made it: a second
    /// call with the key answers with this object.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// What the cloud holds where the document dform sent has a secret's
    /// label (`attrs` keeps the label, as dform sent it): the value, read
    /// inside Apply from the object that holds it, by path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub materialized: BTreeMap<String, Json>,
}

fn key(typ: &str, name: &str) -> String {
    format!("{typ}::{name}")
}

/// How an Apply call failed.
#[derive(Debug)]
pub enum Failed {
    /// Nothing changed.
    Refused(String),
    /// The change took effect; no answer (chaos `timeout`).
    TimedOut(String),
    /// The provider died during the call (chaos `crash`, linked in).
    Crashed(String),
}

/// What an answered Apply call returns.
#[derive(Debug, Default)]
pub struct Applied {
    pub remote: String,
    pub attrs: Json,
    pub computed: Json,
    pub elapsed_ms: u64,
    pub notes: Vec<String>,
}

/// One Apply call, as the fake reads it.
#[derive(Debug, Clone)]
pub struct Call {
    pub op: pb::Op,
    pub addr: Address,
    pub remote: String,
    pub config: Json,
    pub create_first: bool,
    pub assertions: Vec<Assertion>,
    /// A Create's or a Replace's idempotency key; empty for none.
    pub key: String,
}

/// `path` `op` `value` (F DR-13), checked after secrets are materialized.
#[derive(Debug, Clone)]
pub struct Assertion {
    pub path: String,
    pub op: String,
    pub value: Json,
    pub message: String,
}

#[derive(Default)]
pub struct FakeCloud {
    world_path: Option<PathBuf>,
    inventory_path: Option<PathBuf>,
    schema: Schema,
    chaos: Chaos,
    /// What `query` answers externs with: `providers/<name>/externs.df`.
    answers: Vec<Atom>,
    /// The world, read once: the fake is its only writer during a run.
    world: Option<RemoteState>,
    /// Other deployments' worlds, by the name the run reads them by
    /// (`worlds` at Configure): where a held secret of theirs is.
    worlds: BTreeMap<String, PathBuf>,
    /// The deployment the run is of (`stack` at Configure): whose world
    /// this one is.
    stack: String,
    /// The key a secret the mock holds is digested with (`digest_key` at
    /// Configure); none, no digest.
    digest_key: Option<dform_core::zset::file::Key>,
    inventory: Option<RemoteState>,
    /// The chaos `mutate` specs that have landed this run.
    mutated: BTreeSet<usize>,
    /// Address -> remote id, as Read and Apply saw them: where a chaos
    /// `mutate` of an address lands.
    remotes: BTreeMap<Address, String>,
    /// Linked in: chaos `crash` cannot kill the process, so the mock is
    /// gone instead.
    in_process: bool,
    /// The address whose Apply crashed the mock linked in.
    crashed: Option<String>,
}

fn load_json(path: &Option<PathBuf>, what: &str) -> Result<RemoteState> {
    let Some(path) = path.as_ref().filter(|p| p.exists()) else {
        return Ok(RemoteState::default());
    };
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {what} {}", path.display()))
}

fn strings(config: &Json, k: &str) -> Result<Vec<String>> {
    match config.get(k) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(xs)) => xs
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("config {k}: a list of strings"))
            })
            .collect(),
        Some(_) => bail!("config {k}: a list of strings"),
    }
}

fn path_of(config: &Json, k: &str) -> Result<Option<PathBuf>> {
    match config.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s.is_empty() => Ok(None),
        Some(Json::String(s)) => Ok(Some(PathBuf::from(s))),
        Some(_) => bail!("config {k}: a path string"),
    }
}

impl FakeCloud {
    /// `schemas` (names or paths; none is `fake`), `world`, `inventory`,
    /// `chaos`.
    pub fn configure(&mut self, config: &Json) -> Result<()> {
        let specs = strings(config, "schemas")?;
        let specs = if specs.is_empty() {
            vec!["fake".to_string()]
        } else {
            specs
        };
        let mut schema = Schema::default();
        for n in &specs {
            schema = schema.merge(dform_core::schema::load_provider(n)?)?;
        }
        self.schema = schema;
        self.answers = dform_core::externs::load_answers(&specs)?;
        self.world_path = path_of(config, "world")?;
        self.stack = config
            .get("stack")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        self.digest_key = match config.get("digest_key").and_then(Json::as_str) {
            Some(h) => Some(
                dform_core::zset::file::Key::from_hex(h)
                    .ok_or_else(|| anyhow!("config digest_key: 64 hex digits"))?,
            ),
            None => None,
        };
        self.worlds = match config.get("worlds") {
            None | Some(Json::Null) => BTreeMap::new(),
            Some(w) => serde_json::from_value(w.clone())
                .context("config worlds: deployment -> world file")?,
        };
        self.inventory_path = path_of(config, "inventory")?;
        self.chaos = Chaos::parse(&strings(config, "chaos")?)?;
        self.world = None;
        self.inventory = None;
        Ok(())
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The externs the answers file has facts of: any binding pattern.
    pub fn externs(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .answers
            .iter()
            .map(|a| (a.pred.clone(), a.args.len()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn world(&mut self) -> Result<&mut RemoteState> {
        if self.world.is_none() {
            self.world = Some(load_json(&self.world_path, "world")?);
        }
        Ok(self.world.as_mut().expect("loaded"))
    }

    fn inventory(&mut self) -> Result<&RemoteState> {
        if self.inventory.is_none() {
            self.inventory = Some(load_json(&self.inventory_path, "inventory")?);
        }
        Ok(self.inventory.as_ref().expect("loaded"))
    }

    fn save(&mut self) -> Result<()> {
        let (Some(path), Some(st)) = (&self.world_path, &self.world) else {
            return Ok(());
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(st)?;
        fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Query (E DR-18): the rows of `pred` whose `+` columns (`plus`) are
    /// `inputs`, in order. The mock's answer facts for an extern; the
    /// inventory flattened for `cloud_exists/2`, `cloud_attr/4`,
    /// `cloud_computed/4`; for `CREATED`, the object the Create or Replace
    /// with the key made, whatever its name. No row is an answer too:
    /// nothing matches.
    pub fn query(
        &mut self,
        pred: &str,
        plus: &[bool],
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let all = match pred {
            CREATED => match inputs {
                [Value::Str(typ), name, Value::Str(k)] if !k.is_empty() => self
                    .world()?
                    .resources
                    .values()
                    .filter(|rr| rr.typ == *typ && rr.key == *k)
                    .map(|rr| {
                        vec![
                            Value::Str(typ.clone()),
                            name.clone(),
                            Value::Str(k.clone()),
                            Value::Str(rr.name.clone()),
                        ]
                    })
                    .collect(),
                _ => Vec::new(),
            },
            _ if INVENTORY.iter().any(|(p, _)| *p == pred) => {
                let mut atoms = Vec::new();
                for rr in self.inventory()?.resources.values() {
                    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
                    atoms.push(Atom {
                        pred: "cloud_exists".into(),
                        args: vec![s(&rr.typ), s(&rr.name)],
                        record: None,
                        span: Default::default(),
                    });
                    flatten_json_facts(&mut atoms, "cloud_attr", &rr.typ, &rr.name, "", &rr.attrs);
                    flatten_json_facts(
                        &mut atoms,
                        "cloud_computed",
                        &rr.typ,
                        &rr.name,
                        "",
                        &rr.computed,
                    );
                }
                // The bound columns select.
                let bound = |a: &Atom| {
                    plus.iter()
                        .enumerate()
                        .filter(|(_, b)| **b)
                        .zip(inputs)
                        .all(|((i, _), v)| a.args.get(i) == Some(&Term::Val(v.clone())))
                };
                atoms
                    .iter()
                    .filter(|a| a.pred == pred && bound(a))
                    .map(ground_row)
                    .collect::<Result<_>>()?
            }
            _ => {
                let mut rows = Vec::new();
                for a in self.answers.iter().filter(|a| a.pred == pred) {
                    if a.args.len() != plus.len() {
                        bail!(
                            "the mock's answer {} has {} columns, the extern {}",
                            dform_core::partition::fmt_atom(a),
                            a.args.len(),
                            plus.len()
                        );
                    }
                    rows.push(ground_row(a)?);
                }
                rows
            }
        };
        Ok(all
            .into_iter()
            .filter(|row: &Vec<Value>| {
                row.len() == plus.len()
                    && row
                        .iter()
                        .zip(plus)
                        .filter(|(_, p)| **p)
                        .map(|(v, _)| v)
                        .eq(inputs.iter())
            })
            .collect())
    }

    /// A computed document as it leaves the provider: every sensitive
    /// value replaced by its label.
    fn outward(&self, typ: &str, remote: &str, computed: &Json) -> Json {
        let mut out = computed.clone();
        let paths = self
            .schema
            .computed_of(typ)
            .into_iter()
            .chain(self.schema.optional_computed_of(typ));
        for (path, class) in paths {
            if class == NullClass::Secret && get_path(&out, &path).is_some() {
                let label = dform_core::value::null_label(typ, remote, &path);
                set_path(&mut out, &path, provider::secret_json(&label));
            }
        }
        out
    }

    /// Read one object. One that Read does not see yet (chaos `read-lag`)
    /// is tried again, up to the type's `type_retry` attempts (default 3),
    /// each retry logged on stderr: right after a Create a real cloud's Read
    /// may not see the object yet, and that is not drift. Still missing
    /// after the last attempt, it is gone.
    pub fn read(&mut self, addr: &Address, remote: &str) -> Result<Option<(Json, Json)>> {
        self.remotes.insert(addr.clone(), remote.to_string());
        let attempts = u64::from(self.schema.read_attempts(&addr.typ));
        let k = key(&addr.typ, remote);
        let world = self.world()?;
        let Some(rr) = world.resources.get_mut(&k) else {
            return Ok(None);
        };
        if let Some(lag) = rr.read_lag {
            let at = addr.to_string();
            for attempt in 2..=attempts.min(lag + 1) {
                eprintln!("retry {at} read ({attempt}/{attempts})");
            }
            let missed = lag.min(attempts);
            rr.read_lag = Some(lag - missed).filter(|l| *l > 0);
            let gone = missed == attempts;
            // Reads the lag swallowed are the mock cloud's own clock.
            self.save()?;
            if gone {
                eprintln!("read {at}: nothing after {attempts} attempts; taken as gone");
                return Ok(None);
            }
        }
        let rr = &self.world.as_ref().expect("loaded").resources[&k];
        Ok(Some((
            rr.attrs.clone(),
            self.outward(&rr.typ, remote, &rr.computed),
        )))
    }

    /// Import: an object by remote id, from the world, else the inventory.
    pub fn import(&mut self, typ: &str, remote: &str) -> Result<Option<(String, Json, Json)>> {
        let k = key(typ, remote);
        let found = match self.world()?.resources.get(&k) {
            Some(rr) => Some(rr.clone()),
            None => self.inventory()?.resources.get(&k).cloned(),
        };
        Ok(found.map(|rr| {
            let computed = self.outward(typ, remote, &rr.computed);
            (rr.name, rr.attrs, computed)
        }))
    }

    /// Every `required` attribute is set. Paths inside a list element are not
    /// checked.
    fn check_required(&self, addr: &Address, doc: &Json) -> Result<()> {
        let typ = addr.typ.as_str();
        for ((t, path), spec) in &self.schema.attrs {
            if t != typ || !spec.has("required") {
                continue;
            }
            if !self.schema.in_list(typ, path) && get_path(doc, path).is_none() {
                bail!("plan {addr}: required attribute {path} is not set");
            }
        }
        Ok(())
    }

    /// Plan one resource: the desired document is valid, its diff against
    /// the world's, and whether a change is to a `force_new` path.
    pub fn plan(
        &self,
        addr: &Address,
        prior: Option<&Json>,
        desired: Option<&Json>,
    ) -> Result<(Vec<Change>, bool)> {
        if let Some(d) = desired {
            self.check_required(addr, d)?;
        }
        let changes = provider::diff(&self.schema, &addr.typ, prior, desired);
        let replaces = prior.is_some()
            && desired.is_some()
            && changes
                .iter()
                .any(|c| self.schema.forces_new(&addr.typ, &norm_path(&c.path)));
        Ok((changes, replaces))
    }

    /// A secret's value, from the computed values of the object its label
    /// `T/N#P` names.
    fn materialize(&mut self, label: &str) -> Result<Option<Json>> {
        let Some((typ, name)) = dform_core::value::null_owner(label) else {
            return Ok(None);
        };
        let Some((_, path)) = label.split_once('#') else {
            return Ok(None);
        };
        let addr = Address { typ, name };
        let remote = self
            .remotes
            .get(&addr)
            .cloned()
            .unwrap_or_else(|| addr.name.clone());
        let world = self.world()?;
        Ok(world
            .resources
            .get(&key(&addr.typ, &remote))
            .and_then(|rr| get_path(&rr.computed, path))
            .cloned())
    }

    /// Answer an extern's secret columns (`secret`, per column) with where
    /// the mock holds them: each value kept in the world's `held`, and
    /// answered as a SECRET null with its place (`Null.held`) and the
    /// keyed digest of the value. The label is the engine's to choose.
    pub fn hold(
        &mut self,
        pred: &str,
        inputs: &[Value],
        secret: &[bool],
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<pb::Row>> {
        let remote = serde_json::to_string(
            &inputs
                .iter()
                .map(dform_core::engine::value_to_json)
                .collect::<Vec<_>>(),
        )?;
        let mut out = Vec::new();
        let mut kept = BTreeMap::new();
        for row in rows {
            let mut values = Vec::new();
            for (c, v) in row.iter().enumerate() {
                if !secret.get(c).copied().unwrap_or(false) {
                    values.push(wire::value(v));
                    continue;
                }
                let j = dform_core::engine::value_to_json(v);
                let digest = self
                    .digest_key
                    .as_ref()
                    .map(|k| {
                        format!(
                            "hmac-sha256:{}",
                            k.digest(dform_core::approval::canonical_json(&j).as_bytes())
                        )
                    })
                    .unwrap_or_default();
                let path = (c + 1).to_string();
                kept.insert(path.clone(), j);
                values.push(pb::Value {
                    kind: Some(pb::value::Kind::Null(pb::Null {
                        label: format!("{pred}#{path}"),
                        class: pb::NullClass::Secret as i32,
                        ty: String::new(),
                        held: Some(pb::Held {
                            provider: backend::FAKECLOUD.into(),
                            deployment: self.stack.clone(),
                            r#type: pred.to_string(),
                            remote: remote.clone(),
                            path,
                            digest,
                        }),
                    })),
                });
            }
            out.push(pb::Row { values });
        }
        if !kept.is_empty() {
            self.world()?.held.insert(key(pred, &remote), kept);
            self.save()?;
        }
        Ok(out)
    }

    /// A secret held by an object (`Held`), this deployment's or another's:
    /// read from that deployment's world, where the mock kept it (an
    /// extern's), else the object's attribute (what it materialized there,
    /// else what it was sent), else its computed value.
    fn materialize_held(&mut self, h: &provider::Held) -> Result<Json> {
        let (world, at) = if h.deployment == self.stack {
            (
                self.world()?.clone(),
                self.world_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            )
        } else {
            let Some(path) = self.worlds.get(&h.deployment) else {
                bail!(
                    "it is held by {} of {}, whose world this run was not given",
                    h.typ,
                    h.deployment
                );
            };
            (
                load_json(&Some(path.clone()), "world")?,
                path.display().to_string(),
            )
        };
        if let Some(v) = world
            .held
            .get(&key(&h.typ, &h.remote))
            .and_then(|m| m.get(&h.path))
        {
            return Ok(v.clone());
        }
        let Some(rr) = world.resources.get(&key(&h.typ, &h.remote)) else {
            bail!(
                "{} {} of {} is not in its world {at}",
                h.typ,
                h.remote,
                h.deployment,
            );
        };
        rr.materialized
            .get(&h.path)
            .or_else(|| get_path(&rr.attrs, &h.path))
            .or_else(|| get_path(&rr.computed, &h.path))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "{} {} of {} does not set {}",
                    h.typ,
                    h.remote,
                    h.deployment,
                    h.path
                )
            })
    }

    /// Every secret label in an Apply document, by path, materialized: a
    /// held one from the object that holds it (one that cannot be read is a
    /// refusal naming the path), another from this world's computed values
    /// when an object here has it.
    fn materialize_doc(&mut self, at: &str, doc: &Json) -> Result<BTreeMap<String, Json>, Failed> {
        fn walk(v: &Json, path: &str, out: &mut Vec<(String, Json)>) {
            if provider::marker(v).is_some_and(|(k, _)| k == provider::SECRET_KEY) {
                out.push((path.to_string(), v.clone()));
                return;
            }
            let join = |k: &str| match path {
                "" => k.to_string(),
                p => format!("{p}.{k}"),
            };
            match v {
                Json::Object(m) => m.iter().for_each(|(k, x)| walk(x, &join(k), out)),
                Json::Array(xs) => xs
                    .iter()
                    .enumerate()
                    .for_each(|(i, x)| walk(x, &join(&i.to_string()), out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(doc, "", &mut found);
        let mut out = BTreeMap::new();
        for (path, m) in found {
            let label = provider::marker(&m)
                .map(|(_, l)| l.to_string())
                .unwrap_or_default();
            let v = match provider::held(&m) {
                Some(h) => self.materialize_held(&h).map(Some),
                None => self.materialize(&label),
            };
            match v {
                Ok(Some(v)) => {
                    out.insert(path, v);
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(Failed::Refused(format!(
                        "apply {at}: {path}: the secret {label} cannot be read: {e:#}"
                    )));
                }
            }
        }
        Ok(out)
    }

    fn check_assertions(&mut self, at: &str, c: &Call) -> Result<(), Failed> {
        for a in &c.assertions {
            let mut v = get_path(&c.config, &a.path).cloned();
            if let Some((provider::SECRET_KEY, label)) = v.as_ref().and_then(provider::marker) {
                let label = label.to_string();
                v = self
                    .materialize(&label)
                    .map_err(|e| Failed::Refused(format!("apply {at}: {e:#}")))?;
            }
            let holds = match (a.op.as_str(), v.as_ref()) {
                ("eq", v) => v == Some(&a.value),
                ("ne", v) => v != Some(&a.value),
                ("len_ge" | "len_le", Some(Json::String(s))) => {
                    let n = s.chars().count() as i64;
                    let want = a.value.as_i64().unwrap_or(0);
                    if a.op == "len_ge" {
                        n >= want
                    } else {
                        n <= want
                    }
                }
                ("prefix", Some(Json::String(s))) => {
                    a.value.as_str().is_some_and(|p| s.starts_with(p))
                }
                ("len_ge" | "len_le" | "prefix", _) => false,
                // A refinement's own op (`dform_core::refine::to_assertion`); a
                // path the document does not set holds vacuously.
                (op, v) if let Some(c) = dform_core::refine::from_assertion(op, &a.value) => v
                    .is_none_or(|v| {
                        c.check(&dform_core::provider::json_to_value(v))
                            == dform_core::lattice::Truth::True
                    }),
                (op, _) => {
                    return Err(Failed::Refused(format!(
                        "apply {at}: unknown assertion {op} (eq, ne, len_ge, len_le, prefix, or a refinement)"
                    )));
                }
            };
            if !holds {
                let what = if a.message.is_empty() {
                    format!("{} {} {}", a.path, a.op, a.value)
                } else {
                    a.message.clone()
                };
                return Err(Failed::Refused(format!(
                    "apply {at}: assertion failed: {what}"
                )));
            }
        }
        Ok(())
    }

    /// One Apply call. The world is saved after it (the cloud keeps what it
    /// did, whatever happens next).
    pub fn apply(&mut self, c: Call) -> Result<Applied, Failed> {
        let addr = &c.addr;
        let at = addr.to_string();
        let refuse = |e: anyhow::Error| Failed::Refused(format!("{e:#}"));
        self.check_assertions(&at, &c)?;
        if self.chaos.crash.contains(addr) {
            if self.in_process {
                eprintln!("chaos: crash during apply {at}: the provider is gone");
                self.crashed = Some(at.clone());
                return Err(Failed::Crashed(format!(
                    "the provider fakecloud exited during the call (chaos crash={at})"
                )));
            }
            eprintln!("chaos: crash during apply {at}: the process is killed");
            std::process::exit(137);
        }
        if self.chaos.fail.contains(addr) {
            return Err(Failed::Refused(format!(
                "apply {at}: injected failure (chaos fail={at})"
            )));
        }
        let mut out = Applied::default();
        if let Some(ms) = self.chaos.latency.get(addr) {
            out.notes
                .push(format!("latency {at}: {ms}ms (simulated, not slept)"));
            out.elapsed_ms = *ms;
        }
        // A timed-out call takes effect in the world, but dform never
        // hears back.
        let answered = !self.chaos.timeout.contains(addr);
        let doc = c.config;
        self.world().map_err(refuse)?;
        let materialized = match c.op {
            pb::Op::Delete => BTreeMap::new(),
            _ => self.materialize_doc(&at, &doc)?,
        };
        // The same key again: the object the first call made, unchanged.
        let made = match c.op {
            pb::Op::Create | pb::Op::Replace if !c.key.is_empty() => self
                .world_ref()
                .resources
                .values()
                .find(|rr| rr.typ == addr.typ && rr.key == c.key)
                .map(|rr| rr.name.clone()),
            _ => None,
        };
        let remote = match c.op {
            _ if made.is_some() => {
                out.notes.push(format!(
                    "apply {at}: idempotency key {} made {} already",
                    c.key,
                    made.as_deref().unwrap_or_default()
                ));
                made
            }
            pb::Op::Delete => {
                self.delete_object(&addr.typ, &c.remote);
                self.remotes.remove(addr);
                None
            }
            pb::Op::Create => {
                let remote = addr.name.clone();
                let k = key(&addr.typ, &remote);
                if self.world_ref().resources.contains_key(&k) {
                    return Err(Failed::Refused(format!(
                        "apply {at}: create failed: {k} already exists in the world"
                    )));
                }
                self.create_object(addr, &remote, doc, &c.key);
                Some(remote)
            }
            pb::Op::Replace => {
                if !c.create_first {
                    self.delete_object(&addr.typ, &c.remote);
                }
                let remote = self.free_name(&addr.typ, &addr.name);
                self.create_object(addr, &remote, doc, &c.key);
                Some(remote)
            }
            pb::Op::Adopt => {
                let k = key(&addr.typ, &c.remote);
                let Some(inv_rr) = self.inventory().map_err(refuse)?.resources.get(&k) else {
                    return Err(Failed::Refused(format!(
                        "apply {at}: adopt requested but inventory missing {k}"
                    )));
                };
                let computed = inv_rr.computed.clone();
                self.world_mut().resources.insert(
                    k,
                    RemoteResource {
                        typ: addr.typ.clone(),
                        name: c.remote.clone(),
                        attrs: doc,
                        computed,
                        read_lag: None,
                        key: String::new(),
                        materialized: BTreeMap::new(),
                    },
                );
                Some(c.remote.clone())
            }
            pb::Op::Update => {
                let k = key(&addr.typ, &c.remote);
                let (computed, made_by) = match self.world_ref().resources.get(&k) {
                    Some(cur) => (cur.computed.clone(), cur.key.clone()),
                    None => {
                        let salt = self.salt();
                        (self.mint(&addr.typ, &c.remote, &doc, salt), String::new())
                    }
                };
                self.world_mut().resources.insert(
                    k,
                    RemoteResource {
                        typ: addr.typ.clone(),
                        name: c.remote.clone(),
                        attrs: doc,
                        computed,
                        read_lag: None,
                        key: made_by,
                        materialized: BTreeMap::new(),
                    },
                );
                Some(c.remote.clone())
            }
            pb::Op::EndTick | pb::Op::Unspecified => {
                return Err(Failed::Refused(format!(
                    "apply {at}: {:?} is not an action",
                    c.op
                )));
            }
        };
        if let Some(r) = &remote {
            self.remotes.insert(addr.clone(), r.clone());
            if let Some(rr) = self.world_mut().resources.get_mut(&key(&addr.typ, r)) {
                rr.materialized = materialized;
            }
            let rr = &self.world_ref().resources[&key(&addr.typ, r)];
            out.attrs = rr.attrs.clone();
            out.computed = self.outward(&addr.typ, r, &rr.computed);
            out.remote = r.clone();
        }
        self.save().map_err(refuse)?;
        if !answered {
            return Err(Failed::TimedOut(format!(
                "apply {at}: timed out waiting for the provider (chaos timeout={at}); \
                 the change may have taken effect"
            )));
        }
        Ok(out)
    }

    fn world_ref(&self) -> &RemoteState {
        self.world.as_ref().expect("loaded")
    }

    fn world_mut(&mut self) -> &mut RemoteState {
        self.world.as_mut().expect("loaded")
    }

    /// The tick ends (a phase boundary): the calls' spans are the world's
    /// timeline, chaos mutations land, the clock advances.
    pub fn end_tick(&mut self, spans: Vec<Span>) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        let makespan = spans.iter().map(|s| s.end_ms).max().unwrap_or(0);
        if makespan > 0 {
            notes.push(format!("simulated apply time: {makespan}ms"));
        }
        let mutate = self.chaos.mutate.clone();
        let remotes = self.remotes.clone();
        let world = self.world()?;
        // Kept only when chaos `latency` put a call on the clock.
        world.timeline = match makespan {
            0 => Vec::new(),
            _ => spans,
        };
        for (i, (addr, path, v)) in mutate.iter().enumerate() {
            // Once per run: after the first tick the resource exists at.
            if self.mutated.contains(&i) {
                continue;
            }
            let at = addr.to_string();
            let remote = remotes
                .get(addr)
                .cloned()
                .unwrap_or_else(|| addr.name.clone());
            let world = self.world.as_mut().expect("loaded");
            match world.resources.get_mut(&key(&addr.typ, &remote)) {
                Some(rr) => {
                    set_path(&mut rr.attrs, path, v.clone());
                    self.mutated.insert(i);
                    notes.push(format!(
                        "mutate {} = {v} after tick {}",
                        addr.attr(path),
                        world.tick
                    ));
                }
                None => notes.push(format!("mutate {at}: skipped, not in the world")),
            }
        }
        self.world_mut().tick += 1;
        self.save()?;
        Ok(notes)
    }

    /// Chaos `fresh-ids`: the next serial to salt a Create's minted values
    /// with, else none (the mock's default: the same name mints the same
    /// id).
    fn salt(&mut self) -> Option<u64> {
        if !self.chaos.fresh_ids {
            return None;
        }
        let w = self.world_mut();
        w.serial += 1;
        Some(w.serial)
    }

    /// A new object at `remote`, its computed values minted.
    fn create_object(&mut self, addr: &Address, remote: &str, doc: Json, idempotency_key: &str) {
        let salt = self.salt();
        let computed = self.mint(&addr.typ, remote, &doc, salt);
        let read_lag = self.chaos.read_lag.get(addr).copied();
        self.world_mut().resources.insert(
            key(&addr.typ, remote),
            RemoteResource {
                typ: addr.typ.clone(),
                name: remote.to_string(),
                attrs: doc,
                computed,
                read_lag,
                key: idempotency_key.to_string(),
                materialized: BTreeMap::new(),
            },
        );
    }

    fn delete_object(&mut self, typ: &str, remote: &str) {
        self.world_mut().resources.remove(&key(typ, remote));
    }

    /// `name`, or the first `name-N` no object of `typ` has: a replacement
    /// created before its old object is deleted cannot take its name.
    fn free_name(&self, typ: &str, name: &str) -> String {
        std::iter::once(name.to_string())
            .chain((2..).map(|n| format!("{name}-{n}")))
            .find(|r| !self.world_ref().resources.contains_key(&key(typ, r)))
            .unwrap()
    }

    /// What Apply returns for a new resource: every computed attribute of the
    /// type, and every Optional+Computed one the program left unset. A
    /// `salt` (chaos `fresh-ids`) makes every `{hash}` and default id new.
    fn mint(&self, typ: &str, name: &str, doc: &Json, salt: Option<u64>) -> Json {
        let mut out = json!({});
        // Optional+Computed first: a computed template may spell the picked
        // value (an IAM role's id is its name).
        for (attr, class) in self.schema.optional_computed_of(typ) {
            if get_path(doc, &attr).is_none() && !self.schema.in_list(typ, &attr) {
                let v = self.mint_value(typ, name, &attr, class, doc, &out, salt);
                set_path(&mut out, &attr, v);
            }
        }
        for (attr, class) in self.schema.computed_of(typ) {
            if !self.schema.in_list(typ, &attr) {
                let v = self.mint_value(typ, name, &attr, class, doc, &out, salt);
                set_path(&mut out, &attr, v);
            }
        }
        if !self.schema.knows_type(typ) {
            set_path(&mut out, "id", json!(default_id(typ, name, salt)));
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn mint_value(
        &self,
        typ: &str,
        name: &str,
        attr: &str,
        class: NullClass,
        doc: &Json,
        minted: &Json,
        salt: Option<u64>,
    ) -> Json {
        let salted = salt.map(|n| format!("@{n}")).unwrap_or_default();
        let hash = short_hash(&format!("{typ}/{name}#{attr}{salted}"));
        let n = u64::from_str_radix(&hash, 36).unwrap_or(0);
        match self.schema.mints.get(&(typ.to_string(), attr.to_string())) {
            // A template that is one `{doc:PATH}` takes the value there,
            // whatever its type (a list of zones), or the default when unset.
            Some(Value::Str(tpl))
                if tpl.starts_with("{doc:")
                    && tpl.ends_with('}')
                    && tpl.matches('{').count() == 1 =>
            {
                let path = &tpl[5..tpl.len() - 1];
                if let Some(v) = get_path(doc, path).or_else(|| get_path(minted, path)) {
                    return v.clone();
                }
            }
            Some(Value::Str(tpl)) => {
                let s = tpl
                    .replace("{type}", typ)
                    .replace("{name}", name)
                    .replace("{attr}", attr)
                    .replace("{hash}", &hash)
                    .replace("{n}", &(n % 254 + 1).to_string());
                return json!(fill_doc_refs(&s, doc, minted));
            }
            Some(v) => return value_to_json(v),
            None => {}
        }
        let ty = self
            .schema
            .attr(typ, attr)
            .map(|a| a.kind())
            .unwrap_or("string");
        match (class, ty) {
            (NullClass::Secret, _) => json!(format!("fake-secret-{hash}")),
            (NullClass::Fresh, _) if attr == "id" => json!(default_id(typ, name, salt)),
            (NullClass::Fresh, _) => json!(format!("{name}-{hash}")),
            (_, "int") => json!(n % 100),
            (_, "bool") => json!(true),
            (_, "list" | "set") => json!([]),
            (_, "map" | "object") => json!({}),
            _ => json!(format!("{name}.{}.fake", attr.replace('.', "-"))),
        }
    }
}

/// An answer fact as a row.
fn ground_row(a: &Atom) -> Result<Vec<Value>> {
    fn value(t: &Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v.clone()),
            Term::List(xs) => xs.iter().map(value).collect::<Option<_>>().map(Value::List),
            Term::Obj(m) => m
                .iter()
                .map(|(k, v)| Some((k.clone(), value(v)?)))
                .collect::<Option<_>>()
                .map(Value::Obj),
            _ => None,
        }
    }
    a.args
        .iter()
        .map(value)
        .collect::<Option<Vec<Value>>>()
        .ok_or_else(|| {
            anyhow!(
                "the mock's answer {} is not ground",
                dform_core::partition::fmt_atom(a)
            )
        })
}

fn flatten_json_facts(
    out: &mut Vec<Atom>,
    pred: &str,
    typ: &str,
    name: &str,
    prefix: &str,
    v: &Json,
) {
    match v {
        Json::Object(m) => {
            for (k, vv) in m {
                let p = if prefix.is_empty() {
                    k.to_string()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_json_facts(out, pred, typ, name, &p, vv);
            }
        }
        Json::Array(xs) => {
            for (i, vv) in xs.iter().enumerate() {
                let p = format!("{prefix}[{i}]");
                flatten_json_facts(out, pred, typ, name, &p, vv);
            }
        }
        other => {
            let val = match other {
                Json::String(s) => Value::Str(s.clone()),
                Json::Bool(b) => Value::Bool(*b),
                Json::Number(n) => n
                    .as_i64()
                    .map(Value::Int)
                    .unwrap_or(Value::Str(n.to_string())),
                Json::Null => Value::Str("null".to_string()),
                _ => Value::Str(other.to_string()),
            };
            out.push(Atom {
                pred: pred.to_string(),
                args: vec![
                    Term::Val(Value::Str(typ.to_string())),
                    Term::Val(Value::Str(name.to_string())),
                    Term::Val(Value::Str(prefix.to_string())),
                    Term::Val(val),
                ],
                record: None,
                span: Default::default(),
            });
        }
    }
}

/// The fake behind the protocol: every call locks the one mock cloud. As
/// a process (`dform-provider-fake`, `in_process` false) chaos `crash`
/// kills it; linked in, it is gone from that call on.
pub struct Mock {
    cloud: std::sync::Mutex<FakeCloud>,
    in_process: bool,
}

impl Mock {
    /// The mock as `dform-provider-fake` serves it.
    pub fn process() -> Mock {
        Mock::new(false)
    }

    /// The mock linked in (the direct and wire backends).
    pub fn linked() -> Mock {
        Mock::new(true)
    }

    fn new(in_process: bool) -> Mock {
        Mock {
            cloud: std::sync::Mutex::new(FakeCloud {
                in_process,
                ..FakeCloud::default()
            }),
            in_process,
        }
    }

    fn cloud(&self) -> std::sync::MutexGuard<'_, FakeCloud> {
        self.cloud.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn invalid(e: anyhow::Error) -> CallError {
    CallError::Refused(format!("{e:#}"))
}

fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, CallError> {
    v.map(wire::from_doc).transpose().map_err(invalid)
}

impl Handler for Mock {
    fn handle(&self, call: backend::Call) -> std::result::Result<Reply, CallError> {
        use backend::Call as C;
        if let Some(at) = &self.cloud().crashed {
            return Err(CallError::Crashed(format!(
                "the provider fakecloud has exited (chaos crash={at})"
            )));
        }
        Ok(match call {
            C::Handshake(req) => {
                let v = req.protocol_version;
                if v != VERSION {
                    return Err(CallError::Refused(format!(
                        "this provider speaks protocol version {VERSION}, not {v}"
                    )));
                }
                Reply::Handshake(pb::HandshakeResponse {
                    protocol_version: VERSION,
                    name: backend::FAKECLOUD.into(),
                    capabilities: ["resource", "fact", "inventory", "managed"]
                        .map(String::from)
                        .to_vec(),
                    version: backend::BUILD.into(),
                })
            }
            C::Configure(req) => {
                let config = doc_of(req.config.as_ref())?.unwrap_or(json!({}));
                self.cloud().configure(&config).map_err(invalid)?;
                // The account its settings name (`provider fake { account
                // = .. }`): what `expect_account` is checked against.
                let account = config
                    .pointer("/settings/account")
                    .and_then(Json::as_str)
                    .map(str::to_string);
                Reply::Configure(pb::ConfigureResponse { account })
            }
            C::Schema(req) => {
                let cloud = self.cloud();
                let facts = wire::schema_facts(cloud.schema(), &req).map_err(invalid)?;
                let externs = cloud
                    .externs()
                    .into_iter()
                    .map(|(pred, arity)| pb::ExternDecl {
                        pred,
                        arity: arity as u32,
                        input: Vec::new(),
                    })
                    .collect();
                Reply::Schema(pb::SchemaResponse {
                    facts,
                    externs,
                    checks_refinements: true,
                    examples: Vec::new(),
                })
            }
            C::Query(q) => {
                let inputs = q
                    .inputs
                    .iter()
                    .map(wire::from_value)
                    .collect::<Result<Vec<_>>>()
                    .map_err(invalid)?;
                let mut cloud = self.cloud();
                let rows = cloud.query(&q.pred, &q.input, &inputs).map_err(invalid)?;
                Reply::Query(
                    cloud
                        .hold(&q.pred, &inputs, &q.secret, rows)
                        .map_err(invalid)?,
                )
            }
            C::Read(r) => {
                let addr = Address {
                    typ: r.r#type,
                    name: r.name,
                };
                let found = self.cloud().read(&addr, &r.remote).map_err(invalid)?;
                Reply::Read(match found {
                    Some((attrs, computed)) => pb::ReadResponse {
                        found: true,
                        attrs: Some(wire::doc(&attrs)),
                        computed: Some(wire::doc(&computed)),
                    },
                    None => pb::ReadResponse::default(),
                })
            }
            C::Plan(r) => {
                let addr = Address {
                    typ: r.r#type,
                    name: r.name,
                };
                let prior = doc_of(r.prior.as_ref())?;
                let desired = doc_of(r.desired.as_ref())?;
                let (changes, requires_replace) = self
                    .cloud()
                    .plan(&addr, prior.as_ref(), desired.as_ref())
                    .map_err(invalid)?;
                Reply::Plan(pb::PlanResponse {
                    changes: changes
                        .iter()
                        .map(|c| pb::Change {
                            path: c.path.clone(),
                            before: c.before.as_ref().map(wire::doc),
                            after: c.after.as_ref().map(wire::doc),
                            sensitive: c.sensitive,
                        })
                        .collect(),
                    requires_replace,
                })
            }
            C::Apply(r) => Reply::Apply(self.apply(r)?),
            C::Import(r) => {
                let found = self.cloud().import(&r.r#type, &r.remote).map_err(invalid)?;
                Reply::Import(match found {
                    Some((name, attrs, computed)) => pb::ImportResponse {
                        found: true,
                        r#type: r.r#type,
                        name,
                        attrs: Some(wire::doc(&attrs)),
                        computed: Some(wire::doc(&computed)),
                    },
                    None => pb::ImportResponse::default(),
                })
            }
        })
    }

    fn is_dead(&self) -> bool {
        self.in_process && self.cloud().crashed.is_some()
    }
}

impl Mock {
    fn apply(&self, r: pb::ApplyRequest) -> std::result::Result<pb::ApplyResponse, CallError> {
        let op = pb::Op::try_from(r.op).unwrap_or(pb::Op::Unspecified);
        if op == pb::Op::EndTick {
            let spans = r
                .spans
                .into_iter()
                .map(|s| Span {
                    addr: s.addr,
                    start_ms: s.start_ms,
                    end_ms: s.end_ms,
                })
                .collect();
            let notes = self.cloud().end_tick(spans).map_err(invalid)?;
            return Ok(pb::ApplyResponse {
                notes,
                ..Default::default()
            });
        }
        let assertions = r
            .assertions
            .iter()
            .map(|a| {
                Ok(Assertion {
                    path: a.path.clone(),
                    op: a.op.clone(),
                    value: doc_of(a.value.as_ref())?.unwrap_or(Json::Null),
                    message: a.message.clone(),
                })
            })
            .collect::<std::result::Result<_, CallError>>()?;
        let call = Call {
            op,
            addr: Address {
                typ: r.r#type,
                name: r.name,
            },
            remote: r.remote,
            config: doc_of(r.config.as_ref())?.unwrap_or(Json::Null),
            create_first: r.create_first,
            assertions,
            key: r.idempotency_key,
        };
        match self.cloud().apply(call) {
            Ok(a) => Ok(pb::ApplyResponse {
                remote: a.remote,
                attrs: Some(wire::doc(&a.attrs)),
                computed: Some(wire::doc(&a.computed)),
                elapsed_ms: a.elapsed_ms,
                notes: a.notes,
            }),
            Err(Failed::Refused(m)) => Err(CallError::Refused(m)),
            Err(Failed::TimedOut(m)) => Err(CallError::MaybeApplied(m)),
            Err(Failed::Crashed(m)) => Err(CallError::Crashed(m)),
        }
    }
}

/// The direct and wire backends: the mock linked in, its calls queued
/// (`plugin::queue`). A plugin executable is out of their reach.
pub struct Linked {
    pub order: Order,
    /// Every message encoded and decoded through prost (the wire backend).
    pub wire: bool,
}

impl Linked {
    /// The direct backend on the simulated clock.
    pub fn direct() -> Linked {
        Linked {
            order: Order::Clock,
            wire: false,
        }
    }

    /// The wire backend on the simulated clock.
    pub fn wire() -> Linked {
        Linked {
            order: Order::Clock,
            wire: true,
        }
    }
}

impl Launch for Linked {
    fn mock(&self) -> Result<Link> {
        let what = if self.wire { "wire" } else { "direct" };
        Link::start(
            format!("the mock ({what})"),
            Box::new(Queue::new(Mock::linked(), self.order, self.wire)),
        )
    }

    fn plugin(&self, exe: &std::path::Path) -> Result<Link> {
        bail!(
            "provider {}: a plugin executable needs the process backend; this backend \
             links only the mock",
            exe.display()
        )
    }
}

/// The id the mock mints when the schema gives no template: `T:NAME`, and
/// under chaos `fresh-ids` `T:NAME@SERIAL`.
fn default_id(typ: &str, name: &str, salt: Option<u64>) -> String {
    match salt {
        Some(n) => format!("{typ}:{name}@{n}"),
        None => format!("{typ}:{name}"),
    }
}

/// Replace each `{doc:PATH}` in a mint template with the resource's string
/// value at PATH: the program's, else one minted before it, else nothing.
fn fill_doc_refs(tpl: &str, doc: &Json, minted: &Json) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find("{doc:") {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else { break };
        let path = &rest[i + 5..i + j];
        if let Some(Json::String(v)) = get_path(doc, path).or_else(|| get_path(minted, path)) {
            out.push_str(v);
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}

/// A schema fact's ground value as JSON.
fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Str(s) => json!(s),
        Value::Int(i) => json!(i),
        Value::Bool(b) => json!(b),
        Value::List(xs) => Json::Array(xs.iter().map(value_to_json).collect()),
        Value::Obj(m) => Json::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), value_to_json(x)))
                .collect(),
        ),
        other => json!(format!("{other:?}")),
    }
}
