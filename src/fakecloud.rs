//! The fake provider: a mock cloud driven by provider schemas and a world
//! file, served over the provider protocol by `dform-provider-fake`
//! (`serve`). dform talks to it as to any provider.
//!
//! The world file (`--world`, default `.dform/<stack>/remote.json`, given
//! at Configure) is what "exists": each object's configured `attrs` and its
//! `computed` values. Read answers from it; Apply writes it back after every
//! call. The inventory file is what Query answers `cloud_exists/2`,
//! `cloud_attr/4` and `cloud_computed/4` with, and what Import and ADOPT
//! find an object dform does not manage in.
//!
//! Apply mints computed values per the schema (`type_mint` or a default by
//! class and type). A sensitive computed value stays in the world; what
//! Read and Apply hand back is its label, `{"$secret": "T/N#Attr"}`.
//!
//! Chaos knobs (`apply --chaos`, `chaos`) arrive at Configure. The world's
//! clock advances at every END_TICK, when chaos `mutate` lands.

use crate::ast::{Atom, Term};
use crate::chaos::Chaos;
use crate::ir::Address;
use crate::plugin::pb;
use crate::plugin::providers::{INVENTORY, MANAGED};
use crate::plugin::wire;
use crate::provider::{self, Change, get_path, norm_path, set_path, short_hash};
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, anyhow, bail};
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
    inventory: Option<RemoteState>,
    /// The chaos `mutate` specs that have landed this run.
    mutated: BTreeSet<usize>,
    /// Address -> remote id, as Read and Apply saw them: where a chaos
    /// `mutate` of an address lands.
    remotes: BTreeMap<Address, String>,
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
            schema = schema.merge(crate::schema::load_provider(n)?)?;
        }
        self.schema = schema;
        self.answers = crate::externs::load_answers(&specs)?;
        self.world_path = path_of(config, "world")?;
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
    /// `cloud_computed/4`; the world's objects for `MANAGED`. No row is an
    /// answer too: nothing matches.
    pub fn query(
        &mut self,
        pred: &str,
        plus: &[bool],
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let all = match pred {
            MANAGED => self
                .world()?
                .resources
                .values()
                .map(|rr| vec![Value::Str(rr.typ.clone()), Value::Str(rr.name.clone())])
                .collect(),
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
                atoms
                    .iter()
                    .filter(|a| a.pred == pred)
                    .map(ground_row)
                    .collect::<Result<_>>()?
            }
            _ => {
                let mut rows = Vec::new();
                for a in self.answers.iter().filter(|a| a.pred == pred) {
                    if a.args.len() != plus.len() {
                        bail!(
                            "the mock's answer {} has {} columns, the extern {}",
                            crate::partition::fmt_atom(a),
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
                let label = crate::value::null_label(typ, remote, &path);
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
            let at = format!("{}/{}", rr.typ, rr.name);
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
                bail!(
                    "plan {typ}/{}: required attribute {path} is not set",
                    addr.name
                );
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
        let Some((typ, name)) = crate::value::null_owner(label) else {
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
                // A refinement's own op (`crate::refine::to_assertion`); a
                // path the document does not set holds vacuously.
                (op, v) if let Some(c) = crate::refine::from_assertion(op, &a.value) => v
                    .is_none_or(|v| {
                        c.check(&crate::provider::json_to_value(v)) == crate::lattice::Truth::True
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
        let at = format!("{}/{}", addr.typ, addr.name);
        let refuse = |e: anyhow::Error| Failed::Refused(format!("{e:#}"));
        self.check_assertions(&at, &c)?;
        if self.chaos.crash.contains(addr) {
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
        let remote = match c.op {
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
                self.create_object(addr, &remote, doc);
                Some(remote)
            }
            pb::Op::Replace => {
                if !c.create_first {
                    self.delete_object(&addr.typ, &c.remote);
                }
                let remote = self.free_name(&addr.typ, &addr.name);
                self.create_object(addr, &remote, doc);
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
                    },
                );
                Some(c.remote.clone())
            }
            pb::Op::Update => {
                let k = key(&addr.typ, &c.remote);
                let computed = match self.world_ref().resources.get(&k) {
                    Some(cur) => cur.computed.clone(),
                    None => {
                        let salt = self.salt();
                        self.mint(&addr.typ, &c.remote, &doc, salt)
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
            let at = format!("{}/{}", addr.typ, addr.name);
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
                        "mutate {at}: {path} = {v} after tick {}",
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
    fn create_object(&mut self, addr: &Address, remote: &str, doc: Json) {
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
            .map(|a| a.ty.as_str())
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
                crate::partition::fmt_atom(a)
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

/// The fake behind the protocol: every call locks the one mock cloud.
pub struct Service {
    cloud: std::sync::Mutex<FakeCloud>,
}

type Reply<T> = std::result::Result<tonic::Response<T>, tonic::Status>;

fn invalid(e: anyhow::Error) -> tonic::Status {
    tonic::Status::invalid_argument(format!("{e:#}"))
}

#[allow(clippy::result_large_err)] // tonic's own error type
fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, tonic::Status> {
    v.map(wire::from_doc).transpose().map_err(invalid)
}

impl Service {
    fn cloud(&self) -> std::sync::MutexGuard<'_, FakeCloud> {
        self.cloud.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[tonic::async_trait]
#[allow(clippy::result_large_err)] // tonic's own error type
impl pb::provider_server::Provider for Service {
    async fn handshake(
        &self,
        req: tonic::Request<pb::HandshakeRequest>,
    ) -> Reply<pb::HandshakeResponse> {
        let v = req.into_inner().protocol_version;
        if v != crate::plugin::spawn::VERSION {
            return Err(tonic::Status::failed_precondition(format!(
                "this provider speaks protocol version {}, not {v}",
                crate::plugin::spawn::VERSION
            )));
        }
        Ok(tonic::Response::new(pb::HandshakeResponse {
            protocol_version: crate::plugin::spawn::VERSION,
            name: "fakecloud".into(),
            capabilities: ["resource", "fact", "inventory", "managed"]
                .map(String::from)
                .to_vec(),
        }))
    }

    async fn configure(
        &self,
        req: tonic::Request<pb::ConfigureRequest>,
    ) -> Reply<pb::ConfigureResponse> {
        let config = doc_of(req.into_inner().config.as_ref())?.unwrap_or(json!({}));
        self.cloud().configure(&config).map_err(invalid)?;
        Ok(tonic::Response::new(pb::ConfigureResponse {}))
    }

    async fn schema(&self, req: tonic::Request<pb::SchemaRequest>) -> Reply<pb::SchemaResponse> {
        let cloud = self.cloud();
        let facts = wire::schema_facts(cloud.schema(), req.get_ref()).map_err(invalid)?;
        let externs = cloud
            .externs()
            .into_iter()
            .map(|(pred, arity)| pb::ExternDecl {
                pred,
                arity: arity as u32,
                input: Vec::new(),
            })
            .collect();
        Ok(tonic::Response::new(pb::SchemaResponse {
            facts,
            externs,
            checks_refinements: true,
            examples: Vec::new(),
        }))
    }

    type QueryStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<pb::Row, tonic::Status>>,
    >;

    async fn query(&self, req: tonic::Request<pb::QueryRequest>) -> Reply<Self::QueryStream> {
        let q = req.into_inner();
        let inputs = q
            .inputs
            .iter()
            .map(wire::from_value)
            .collect::<Result<Vec<_>>>()
            .map_err(invalid)?;
        let rows = self
            .cloud()
            .query(&q.pred, &q.input, &inputs)
            .map_err(invalid)?;
        let rows: Vec<_> = rows
            .iter()
            .map(|r| {
                Ok(pb::Row {
                    values: r.iter().map(wire::value).collect(),
                })
            })
            .collect();
        Ok(tonic::Response::new(tonic::codegen::tokio_stream::iter(
            rows,
        )))
    }

    async fn read(&self, req: tonic::Request<pb::ReadRequest>) -> Reply<pb::ReadResponse> {
        let r = req.into_inner();
        let addr = Address {
            typ: r.r#type,
            name: r.name,
        };
        let found = self.cloud().read(&addr, &r.remote).map_err(invalid)?;
        Ok(tonic::Response::new(match found {
            Some((attrs, computed)) => pb::ReadResponse {
                found: true,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ReadResponse::default(),
        }))
    }

    async fn plan(&self, req: tonic::Request<pb::PlanRequest>) -> Reply<pb::PlanResponse> {
        let r = req.into_inner();
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
        Ok(tonic::Response::new(pb::PlanResponse {
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
        }))
    }

    async fn apply(&self, req: tonic::Request<pb::ApplyRequest>) -> Reply<pb::ApplyResponse> {
        let r = req.into_inner();
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
            return Ok(tonic::Response::new(pb::ApplyResponse {
                notes,
                ..Default::default()
            }));
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
            .collect::<std::result::Result<_, tonic::Status>>()?;
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
        };
        match self.cloud().apply(call) {
            Ok(a) => Ok(tonic::Response::new(pb::ApplyResponse {
                remote: a.remote,
                attrs: Some(wire::doc(&a.attrs)),
                computed: Some(wire::doc(&a.computed)),
                elapsed_ms: a.elapsed_ms,
                notes: a.notes,
            })),
            Err(Failed::Refused(m)) => Err(tonic::Status::failed_precondition(m)),
            Err(Failed::TimedOut(m)) => Err(tonic::Status::deadline_exceeded(m)),
        }
    }

    async fn import(&self, req: tonic::Request<pb::ImportRequest>) -> Reply<pb::ImportResponse> {
        let r = req.into_inner();
        let found = self.cloud().import(&r.r#type, &r.remote).map_err(invalid)?;
        Ok(tonic::Response::new(match found {
            Some((name, attrs, computed)) => pb::ImportResponse {
                found: true,
                r#type: r.r#type,
                name,
                attrs: Some(wire::doc(&attrs)),
                computed: Some(wire::doc(&computed)),
            },
            None => pb::ImportResponse::default(),
        }))
    }
}

/// `dform-provider-fake`: serve the mock on a loopback port (or a unix
/// socket, `plugin::transport`), print the handshake line, and exit when
/// stdin closes (dform is done or gone).
pub fn serve() -> Result<()> {
    let service = Service {
        cloud: std::sync::Mutex::new(FakeCloud::default()),
    };
    crate::plugin::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(service)
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
        ),
    )
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
