//! The fake provider: a mock cloud driven by a provider schema and a world file.
//!
//! The world file (`--world`, default `.dform/<stack>/remote.json`) is what
//! "exists": each resource's configured `attrs` and its `computed` values. Plan
//! refreshes from it; Apply writes it back after every action.
//!
//! Computed values come only from Apply (proposal E §2.2, DR-11 revised). At
//! plan time a `ref(T, N, Attr)` to a schema-computed attribute is the labeled
//! null `?T/N#Attr` unless N already exists in the world, in which case its
//! computed value is resolved at refresh (round 0), so a steady-state stack
//! carries no nulls. Apply mints computed values per the schema (`type_mint`
//! or a default by class and type) and fills the nulls in dependency order.
//! A sensitive computed value stays in the world; what the provider hands back
//! is its label, `{"$secret": "T/N#Attr"}`, and output prints it redacted.

use crate::ast::{Atom, Term};
use crate::chaos::Chaos;
use crate::ir::{Address, Adopt, Resource};
use crate::provider::{self, Action, ActionKind, Change, Plan, Provider};
use crate::schema::Schema;
use crate::state::{self, State};
use crate::value::{NullClass, Value};
use crate::zset::{self, Lifecycle};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteState {
    /// The world's clock: every apply is one tick. Chaos knobs count in ticks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tick: u64,
    pub resources: BTreeMap<String, RemoteResource>,
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
    /// Chaos `read-lag`: Read does not return this resource while the
    /// world's tick is at most this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_until: Option<u64>,
}

pub struct FakeCloud {
    world: PathBuf,
    inventory: PathBuf,
    schema: Schema,
    chaos: Chaos,
    /// What chaos did during the last apply, for the CLI to print.
    notes: RefCell<Vec<String>>,
}

fn key(typ: &str, name: &str) -> String {
    format!("{typ}::{name}")
}

impl Provider for FakeCloud {
    fn id(&self) -> &str {
        "fakecloud"
    }

    fn catalog(&self) -> Result<Vec<Atom>> {
        FakeCloud::catalog(self)
    }

    fn discover(&self) -> Result<Vec<Atom>> {
        FakeCloud::discover(self)
    }

    fn bootstrap_state(&self, state: &mut State) -> Result<()> {
        // Migration: if state is empty but remote.json exists, treat remote.json as the prior
        // provider-owned "state" and import entries.
        if !state.resources.is_empty() {
            return Ok(());
        }
        let remote = self.load()?;
        for rr in remote.resources.values() {
            let addr = Address {
                typ: rr.typ.clone(),
                name: rr.name.clone(),
            };
            state.set(addr, self.id().to_string(), rr.name.clone());
        }
        Ok(())
    }

    fn plan(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
    ) -> Result<Plan> {
        self.plan_with_state(desired, adopts, lifecycle, state)
    }

    fn apply(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        state: &mut State,
        plan: &Plan,
    ) -> Result<()> {
        crate::executor::run_tick(self, desired, adopts, state, plan, &|_| Ok(()))
    }
}

/// What a ref resolves against: the world (refreshed at plan, live at apply),
/// the inventory, the identity mapping, and the desired documents resolved so
/// far in dependency order.
struct Ctx<'a> {
    world: &'a RemoteState,
    inv: &'a RemoteState,
    state: &'a State,
    adopts: &'a BTreeMap<Address, String>,
    resolved: &'a BTreeMap<Address, Json>,
    /// Apply: a null that cannot be filled is an error naming the resource.
    strict: Option<&'a Address>,
}

impl Ctx<'_> {
    /// The world resource behind an address, through the identity mapping or
    /// an adopt (whose resource is in the world once adopted, else in the
    /// inventory).
    fn existing(&self, addr: &Address) -> Option<&RemoteResource> {
        if let Some(e) = self.state.get(addr) {
            return self.world.resources.get(&key(&addr.typ, &e.remote));
        }
        let rn = self.adopts.get(addr)?;
        let k = key(&addr.typ, rn);
        self.world
            .resources
            .get(&k)
            .or_else(|| self.inv.resources.get(&k))
    }
}

impl FakeCloud {
    /// A fake cloud with the `fake` schema whose world is `<root>/remote.json`
    /// and whose inventory is `<root>/inventory.json`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self::with_paths(
            root.join("remote.json"),
            root.join("inventory.json"),
            crate::schema::fake(),
        )
    }

    pub fn with_paths(
        world: impl Into<PathBuf>,
        inventory: impl Into<PathBuf>,
        schema: Schema,
    ) -> Self {
        Self {
            world: world.into(),
            inventory: inventory.into(),
            schema,
            chaos: Chaos::default(),
            notes: RefCell::new(Vec::new()),
        }
    }

    /// Inject failures and latency into Apply and Read (`apply --chaos`).
    pub fn with_chaos(mut self, chaos: Chaos) -> Self {
        self.chaos = chaos;
        self
    }

    /// What chaos did during the last apply.
    pub fn take_notes(&self) -> Vec<String> {
        self.notes.take()
    }

    fn note(&self, s: String) {
        self.notes.borrow_mut().push(s);
    }

    /// The provider's schema facts (`type_attr`, `type_list_key`,
    /// `type_provider`, `type_mint`), injected into the program as EDB.
    pub fn catalog(&self) -> Result<Vec<Atom>> {
        Ok(self.schema.facts.clone())
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn read_json(path: &PathBuf, what: &str) -> Result<RemoteState> {
        if !path.exists() {
            return Ok(RemoteState::default());
        }
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {what} {}", path.display()))
    }

    /// The world as stored, secrets included. Only the provider sees this.
    pub fn load(&self) -> Result<RemoteState> {
        Self::read_json(&self.world, "world")
    }

    pub fn load_inventory(&self) -> Result<RemoteState> {
        Self::read_json(&self.inventory, "inventory")
    }

    pub fn save(&self, st: &RemoteState) -> Result<()> {
        if let Some(dir) = self.world.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(st)?;
        fs::write(&self.world, bytes).with_context(|| format!("write {}", self.world.display()))?;
        Ok(())
    }

    /// Refresh as facts, for round-0 resolution (E Rule 4, F DR-11 revised):
    /// `identity(T, A, Rid)` for every address state maps to a resource Read
    /// returns, and `world_attr(T, Rid, P, V)` for every schema-computed or
    /// Optional+Computed path the world holds a value for. Secrets are never
    /// handed to the evaluator.
    pub fn world_facts(&self, state: &State) -> Result<Vec<Atom>> {
        let world = self.refresh()?;
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let mut out = Vec::new();
        for (addr, e) in state.entries_for_provider(self.id()) {
            let Some(rr) = world.resources.get(&key(&addr.typ, &e.remote)) else {
                continue;
            };
            out.push(Atom {
                pred: "identity".into(),
                args: vec![s(&addr.typ), s(&addr.name), s(&e.remote)],
                record: None,
            });
            let paths = self
                .schema
                .computed_of(&addr.typ)
                .into_iter()
                .chain(self.schema.optional_computed_of(&addr.typ));
            for (path, class) in paths {
                if class == NullClass::Secret {
                    continue;
                }
                if let Some(v) = get_path(&rr.computed, &path) {
                    out.push(Atom {
                        pred: "world_attr".into(),
                        args: vec![
                            s(&addr.typ),
                            s(&e.remote),
                            s(&path),
                            Term::Val(json_to_value(v)),
                        ],
                        record: None,
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn discover(&self) -> Result<Vec<Atom>> {
        let inv = self.load_inventory()?;
        let mut out = Vec::new();
        for rr in inv.resources.values() {
            out.push(Atom {
                pred: "cloud_exists".to_string(),
                args: vec![
                    Term::Val(Value::Str(rr.typ.clone())),
                    Term::Val(Value::Str(rr.name.clone())),
                ],
                record: None,
            });
            // Flatten attrs + computed.
            flatten_json_facts(&mut out, "cloud_attr", &rr.typ, &rr.name, "", &rr.attrs);
            flatten_json_facts(
                &mut out,
                "cloud_computed",
                &rr.typ,
                &rr.name,
                "",
                &rr.computed,
            );
        }
        Ok(out)
    }

    /// `ref(T, N, Attr)` to a configured attribute (a ref to a computed one
    /// is the evaluator's null): the program's value for N, else the world's.
    fn resolve_ref(&self, ctx: &Ctx, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let addr = Address {
            typ: typ.to_string(),
            name: name.to_string(),
        };
        let label = crate::value::null_label(typ, name, attr);
        let configured = ctx.resolved.get(&addr).and_then(|d| get_path(d, attr));
        let existing = ctx.existing(&addr);
        match configured.or_else(|| existing.and_then(|rr| get_path(&rr.attrs, attr))) {
            Some(v) => Ok(v.clone()),
            None => match ctx.strict {
                Some(at) => bail!(
                    "apply {}/{}: ?{label} is still unknown ({typ}/{name} does not set {attr})",
                    at.typ,
                    at.name
                ),
                None => Ok(provider::null_json(&label)),
            },
        }
    }

    /// A labeled null in a desired document. The executor fills a fresh or
    /// open one at Apply from its owner's Apply earlier in the dependency
    /// order (E §2.7); a secret travels as its label and the provider
    /// materializes it. Plan shows what is still unknown as `?label`.
    fn resolve_null(&self, ctx: &Ctx, label: &str, class: NullClass) -> Result<Json> {
        if class == NullClass::Secret {
            return Ok(provider::secret_json(label));
        }
        let found = label.split_once('#').and_then(|(_, path)| {
            let (typ, name) = crate::value::null_owner(label)?;
            let rr = ctx.existing(&Address { typ, name })?;
            get_path(&rr.computed, path).cloned()
        });
        match (found, ctx.strict) {
            (Some(v), _) => Ok(v),
            (None, Some(at)) => bail!(
                "apply {}/{}: ?{label} is still unknown (its resource has not been created)",
                at.typ,
                at.name
            ),
            (None, None) => Ok(provider::null_json(label)),
        }
    }

    fn resolve_cloud_ref(&self, ctx: &Ctx, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let k = key(typ, name);
        let Some(cur) = ctx.inv.resources.get(&k) else {
            bail!("cloud_ref missing inventory resource {k}");
        };
        if let Some(v) = get_path(&cur.computed, attr).or_else(|| get_path(&cur.attrs, attr)) {
            return Ok(v.clone());
        }
        bail!("cloud_ref missing attribute {typ}.{name}.{attr}");
    }

    fn resolve_value(&self, ctx: &Ctx, v: &Value) -> Result<Json> {
        Ok(match v {
            Value::Str(s) => json!(s),
            Value::Int(i) => json!(i),
            Value::Bool(b) => json!(b),
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.resolve_value(ctx, x))
                    .collect::<Result<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Ok((k.clone(), self.resolve_value(ctx, x)?)))
                    .collect::<Result<_>>()?,
            ),
            Value::Ip(n) => json!(crate::value::u32_to_ipv4(*n)),
            Value::IpNet { addr, prefix } => json!(crate::value::ipnet_to_string(*addr, *prefix)),
            Value::IpRange { start, end } => json!(format!(
                "{}-{}",
                crate::value::u32_to_ipv4(*start),
                crate::value::u32_to_ipv4(*end)
            )),
            Value::Ref { typ, name, attr } => self.resolve_ref(ctx, typ, name, attr)?,
            Value::CloudRef { typ, name, attr } => self.resolve_cloud_ref(ctx, typ, name, attr)?,
            Value::Null { label, class, .. } => self.resolve_null(ctx, label, *class)?,
        })
    }

    /// The document dform wants for `r` (assembled without computed paths,
    /// `ir::compile_resources`), refs and nulls resolved as far as the world
    /// allows.
    fn resolve_doc(&self, ctx: &Ctx, r: &Resource) -> Result<Json> {
        self.resolve_value(ctx, &r.attrs)
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

    /// The world's side of the comparison: configured attributes, plus the
    /// value the provider picked for an Optional+Computed path the program now
    /// sets.
    fn world_doc(&self, typ: &str, cur: &RemoteResource, desired: &Json) -> Json {
        let mut doc = cur.attrs.clone();
        for (attr, _) in self.schema.optional_computed_of(typ) {
            if get_path(desired, &attr).is_some()
                && get_path(&doc, &attr).is_none()
                && let Some(v) = get_path(&cur.computed, &attr)
            {
                set_path(&mut doc, &attr, v.clone());
            }
        }
        doc
    }

    /// The world's configured attributes for every address state maps to a
    /// resource Read returns: what the executor compares to see whether the
    /// world moved under a deformation.
    pub fn observe(&self, state: &State) -> Result<BTreeMap<Address, Json>> {
        let world = self.refresh()?;
        Ok(state
            .entries_for_provider(self.id())
            .filter_map(|(addr, e)| {
                let rr = world.resources.get(&key(&addr.typ, &e.remote))?;
                Some((addr, rr.attrs.clone()))
            })
            .collect())
    }

    /// Refresh: the world as Read returns it. A resource inside its chaos
    /// read-lag is not returned.
    fn refresh(&self) -> Result<RemoteState> {
        let mut world = self.load()?;
        let tick = world.tick;
        world
            .resources
            .retain(|_, rr| rr.hidden_until.is_none_or(|h| tick > h));
        Ok(world)
    }

    /// Plan: refresh, then the Z-set `desired − world` (`zset::deformation`),
    /// then this provider's per-resource plan (the diff, adopt, and replace
    /// when a `force_new` path changes) for each deformation. Actions come
    /// in dependency order; deletes last, in reverse dependency order (from
    /// the dependencies state recorded), with the objects deposed by a
    /// `create_before_destroy` replacement among them.
    pub fn plan_with_state(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
    ) -> Result<Plan> {
        let world = self.refresh()?;
        let inv = self.load_inventory()?;
        let adopt_map = state::adopt_map(adopts);

        // desired: the assembled documents, refs and nulls resolved as far
        // as the world allows.
        let mut resolved: BTreeMap<Address, Json> = BTreeMap::new();
        let mut order = Vec::new();
        for r in topo_sort(desired)? {
            let ctx = Ctx {
                world: &world,
                inv: &inv,
                state,
                adopts: &adopt_map,
                resolved: &resolved,
                strict: None,
            };
            let doc = self.resolve_doc(&ctx, &r)?;
            self.check_required(&r.addr, &doc)?;
            order.push(r.addr.clone());
            resolved.insert(r.addr.clone(), doc);
        }

        // world: through the identity mapping. A state entry whose resource
        // is gone is no row (a desired one is created again); one no longer
        // desired is deleted from state with nothing to delete.
        let mut before: BTreeMap<Address, Json> = BTreeMap::new();
        for (addr, entry) in state.entries_for_provider(self.id()) {
            if let Some(cur) = world.resources.get(&key(&addr.typ, &entry.remote)) {
                let doc = match resolved.get(&addr) {
                    Some(d) => self.world_doc(&addr.typ, cur, d),
                    None => cur.attrs.clone(),
                };
                before.insert(addr, doc);
            }
        }

        let flat = |docs: &BTreeMap<Address, Json>| {
            docs.iter()
                .map(|(a, d)| (a.clone(), self.flat_value(&a.typ, d)))
                .collect::<BTreeMap<Address, Value>>()
        };
        let deformations: BTreeMap<Address, zset::Deformation> =
            zset::deformation(&flat(&resolved), &flat(&before))
                .into_iter()
                .map(|d| (d.addr.clone(), d))
                .collect();

        let mut actions = Vec::new();
        for addr in order.iter().cloned() {
            let d = &deformations[&addr];
            let typ = addr.typ.as_str();
            let after = resolved.get(&addr);
            let prior = before.get(&addr);
            let (kind, changes) = match d.kind {
                zset::Kind::Create => match adopt_map.get(&addr) {
                    Some(remote_name) if state.get(&addr).is_none() => {
                        let inv_key = key(typ, remote_name);
                        let Some(inv_rr) = inv.resources.get(&inv_key) else {
                            bail!("adopt requested but inventory missing {inv_key}");
                        };
                        (
                            ActionKind::Adopt,
                            self.diff(typ, Some(&inv_rr.attrs), after),
                        )
                    }
                    _ => (ActionKind::Create, self.diff(typ, None, after)),
                },
                zset::Kind::Delete => unreachable!("a desired address is never deleted"),
                zset::Kind::Update | zset::Kind::Drift => {
                    let changes = self.diff(typ, prior, after);
                    let kind = if changes
                        .iter()
                        .any(|c| self.schema.forces_new(typ, &norm_path(&c.path)))
                    {
                        ActionKind::Replace {
                            create_first: lifecycle.create_before_destroy.contains(&addr),
                        }
                    } else if d.kind == zset::Kind::Drift {
                        ActionKind::Drift
                    } else {
                        ActionKind::Update
                    };
                    (kind, changes)
                }
                zset::Kind::Pending => (ActionKind::Pending, self.diff(typ, prior, after)),
                zset::Kind::Undeformed => (ActionKind::Noop, vec![]),
            };
            let on = match d.kind {
                zset::Kind::Pending => d.unresolved.clone(),
                _ => BTreeSet::new(),
            };
            actions.push(Action {
                kind,
                addr,
                changes,
                on,
            });
        }

        // Deletes: what is no longer desired, what state maps to a vanished
        // resource, and deposed objects. One goes before the deletes of what
        // it depends on.
        let mut deletes: Vec<(Action, &[String])> = Vec::new();
        for (addr, entry) in state.entries_for_provider(self.id()) {
            if resolved.contains_key(&addr) {
                continue;
            }
            // A vanished resource has no world document: nothing to show.
            let changes = self.diff(&addr.typ, before.get(&addr), None);
            deletes.push((
                Action {
                    kind: ActionKind::Delete,
                    addr,
                    changes,
                    on: BTreeSet::new(),
                },
                &entry.deps,
            ));
        }
        for (addr, entry) in state.deposed_for_provider(self.id()) {
            let prior = world.resources.get(&key(&addr.typ, &entry.remote));
            deletes.push((
                Action {
                    kind: ActionKind::DeleteDeposed,
                    changes: self.diff(&addr.typ, prior.map(|rr| &rr.attrs), None),
                    addr,
                    on: BTreeSet::new(),
                },
                &entry.deps,
            ));
        }
        deletes.sort_by(|a, b| a.0.addr.cmp(&b.0.addr));
        actions.extend(reverse_dependency_order(deletes));
        Ok(Plan { actions })
    }

    /// A document in the canonical form the Z-set compares: leaf path to
    /// leaf value, keyed lists by key, sets sorted (`flatten`), null and
    /// secret markers as labeled nulls of the schema's class.
    fn flat_value(&self, typ: &str, doc: &Json) -> Value {
        let mut leaves = BTreeMap::new();
        self.flatten(typ, doc, "", "", &mut leaves);
        Value::Obj(
            leaves
                .into_iter()
                .map(|(p, (v, _))| {
                    let v = match provider::marker(&v) {
                        Some((provider::NULL_KEY, label)) => Value::Null {
                            label: label.to_string(),
                            class: self.null_class(label),
                            ty: String::new(),
                        },
                        Some((_, label)) => Value::Null {
                            label: label.to_string(),
                            class: NullClass::Secret,
                            ty: String::new(),
                        },
                        None => json_to_value(&v),
                    };
                    (p, v)
                })
                .collect(),
        )
    }

    /// The class of the null labeled `T/A#P`: the schema's, else open (a
    /// ref to a configured attribute nobody set).
    fn null_class(&self, label: &str) -> NullClass {
        let Some((ta, path)) = label.split_once('#') else {
            return NullClass::Open;
        };
        let typ = ta.split_once('/').map(|x| x.0).unwrap_or(ta);
        self.schema
            .class_of(typ, path)
            .or_else(|| self.schema.optional_computed_class(typ, path))
            .unwrap_or(NullClass::Open)
    }

    /// Open one tick of the world for the executor's per-action Apply calls
    /// (`executor::run_tick`). The world is saved after every call.
    pub fn begin_tick<'a>(&'a self, desired: &'a [Resource], adopts: &[Adopt]) -> Result<Tick<'a>> {
        Ok(Tick {
            cloud: self,
            world: self.load()?,
            inv: self.load_inventory()?,
            adopt_map: state::adopt_map(adopts),
            desired: desired.iter().map(|r| (r.addr.clone(), r)).collect(),
            resolved: BTreeMap::new(),
            clock: 0,
        })
    }

    /// The tick ends: chaos mutations land, the clock advances, and a
    /// read-lag that has run out is forgotten.
    fn end_tick(&self, world: &mut RemoteState, state: &State) {
        for (addr, path, v) in &self.chaos.mutate {
            let at = format!("{}/{}", addr.typ, addr.name);
            let remote = state
                .get(addr)
                .map(|e| e.remote.clone())
                .unwrap_or_else(|| addr.name.clone());
            match world.resources.get_mut(&key(&addr.typ, &remote)) {
                Some(rr) => {
                    set_path(&mut rr.attrs, path, v.clone());
                    self.note(format!(
                        "mutate {at}: {path} = {v} after tick {}",
                        world.tick
                    ));
                }
                None => self.note(format!("mutate {at}: skipped, not in the world")),
            }
        }
        world.tick += 1;
        let tick = world.tick;
        for rr in world.resources.values_mut() {
            if rr.hidden_until.is_some_and(|h| tick > h) {
                rr.hidden_until = None;
            }
        }
    }

    /// What Apply returns for a new resource: every computed attribute of the
    /// type, and every Optional+Computed one the program left unset.
    fn mint(&self, typ: &str, name: &str, doc: &Json) -> Json {
        let mut out = json!({});
        // Optional+Computed first: a computed template may spell the picked
        // value (an IAM role's id is its name).
        for (attr, class) in self.schema.optional_computed_of(typ) {
            if get_path(doc, &attr).is_none() && !self.schema.in_list(typ, &attr) {
                let v = self.mint_value(typ, name, &attr, class, doc, &out);
                set_path(&mut out, &attr, v);
            }
        }
        for (attr, class) in self.schema.computed_of(typ) {
            if !self.schema.in_list(typ, &attr) {
                let v = self.mint_value(typ, name, &attr, class, doc, &out);
                set_path(&mut out, &attr, v);
            }
        }
        if !self.schema.knows_type(typ) {
            set_path(&mut out, "id", json!(format!("{typ}:{name}")));
        }
        out
    }

    fn mint_value(
        &self,
        typ: &str,
        name: &str,
        attr: &str,
        class: NullClass,
        doc: &Json,
        minted: &Json,
    ) -> Json {
        let hash = short_hash(&format!("{typ}/{name}#{attr}"));
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
            (NullClass::Fresh, _) if attr == "id" => json!(format!("{typ}:{name}")),
            (NullClass::Fresh, _) => json!(format!("{name}-{hash}")),
            (_, "int") => json!(n % 100),
            (_, "bool") => json!(true),
            (_, "list" | "set") => json!([]),
            (_, "map" | "object") => json!({}),
            _ => json!(format!("{name}.{}.fake", attr.replace('.', "-"))),
        }
    }

    /// The leaf-by-leaf changes from `before` to `after`, in the plan's
    /// canonical form (keyed lists by key, sets as sets).
    pub fn diff(&self, typ: &str, before: Option<&Json>, after: Option<&Json>) -> Vec<Change> {
        let mut a = BTreeMap::new();
        let mut b = BTreeMap::new();
        if let Some(v) = before {
            self.flatten(typ, v, "", "", &mut a);
        }
        if let Some(v) = after {
            self.flatten(typ, v, "", "", &mut b);
        }
        let paths: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        let mut out = Vec::new();
        for p in paths {
            let (av, bv) = (a.get(p), b.get(p));
            if av.map(|x| &x.0) == bv.map(|x| &x.0) {
                continue;
            }
            let norm = av.or(bv).map(|x| x.1.as_str()).unwrap_or("");
            out.push(Change {
                path: p.clone(),
                before: av.map(|x| x.0.clone()),
                after: bv.map(|x| x.0.clone()),
                sensitive: self.schema.is_sensitive(typ, norm),
            });
        }
        out
    }

    /// Flatten a document to leaf paths. `norm` is the schema path (dotted, no
    /// indices). A list with `type_list_key` merge keys is spelled by key,
    /// `containers[name=web]`, so reordering is not a change; a `set` is
    /// compared as a set. Null and secret markers are leaves.
    fn flatten(
        &self,
        typ: &str,
        v: &Json,
        prefix: &str,
        norm: &str,
        out: &mut BTreeMap<String, (Json, String)>,
    ) {
        if provider::marker(v).is_some() {
            out.insert(prefix.to_string(), (v.clone(), norm.to_string()));
            return;
        }
        match v {
            Json::Object(m) => {
                for (k, vv) in m {
                    let join = |p: &str| {
                        if p.is_empty() {
                            k.clone()
                        } else {
                            format!("{p}.{k}")
                        }
                    };
                    self.flatten(typ, vv, &join(prefix), &join(norm), out);
                }
            }
            Json::Array(xs) => {
                let keys = self.schema.list_key(typ, norm);
                let is_set = self.schema.attr(typ, norm).is_some_and(|a| a.ty == "set");
                let mut items: Vec<(String, &Json)> = Vec::new();
                for (i, vv) in xs.iter().enumerate() {
                    let by_key = keys.and_then(|ks| {
                        ks.iter()
                            .map(|k| {
                                vv.get(k).map(|x| {
                                    format!(
                                        "{k}={}",
                                        provider::fmt_value(Some(x)).trim_matches('"')
                                    )
                                })
                            })
                            .collect::<Option<Vec<_>>>()
                    });
                    let label = match by_key {
                        Some(parts) => parts.join(","),
                        None if is_set => serde_json::to_string(vv).unwrap_or_default(),
                        None => i.to_string(),
                    };
                    items.push((label, vv));
                }
                if is_set && keys.is_none() {
                    items.sort_by(|x, y| x.0.cmp(&y.0));
                    for (i, it) in items.iter_mut().enumerate() {
                        it.0 = i.to_string();
                    }
                }
                for (label, vv) in items {
                    self.flatten(typ, vv, &format!("{prefix}[{label}]"), norm, out);
                }
            }
            _ => {
                out.insert(prefix.to_string(), (v.clone(), norm.to_string()));
            }
        }
    }
}

/// One tick of the fake world, open for Apply calls. The executor decides
/// the order; each call is one provider Apply and the world is saved after
/// it (the cloud keeps what it did, whatever happens next).
pub struct Tick<'a> {
    cloud: &'a FakeCloud,
    world: RemoteState,
    inv: RemoteState,
    adopt_map: BTreeMap<Address, String>,
    desired: BTreeMap<Address, &'a Resource>,
    /// Documents applied so far this tick, for configured-attribute refs.
    resolved: BTreeMap<Address, Json>,
    /// Simulated time spent in Apply (chaos `latency`).
    clock: u64,
}

impl Tick<'_> {
    /// One Apply call for `a`. Identity is recorded in `state` when the call
    /// answers; a timed-out call takes effect in the world and records none.
    pub fn apply(&mut self, a: &Action, state: &mut State) -> Result<()> {
        let cloud = self.cloud;
        let addr = &a.addr;
        let at = format!("{}/{}", addr.typ, addr.name);
        let doc = match a.kind {
            ActionKind::Delete | ActionKind::DeleteDeposed => Json::Null,
            ActionKind::Noop | ActionKind::Pending => return Ok(()),
            _ => {
                let r = self
                    .desired
                    .get(addr)
                    .ok_or_else(|| anyhow!("apply {at}: no desired resource"))?;
                let ctx = Ctx {
                    world: &self.world,
                    inv: &self.inv,
                    state,
                    adopts: &self.adopt_map,
                    resolved: &self.resolved,
                    strict: Some(addr),
                };
                let doc = cloud.resolve_doc(&ctx, r)?;
                self.resolved.insert(addr.clone(), doc.clone());
                doc
            }
        };
        if cloud.chaos.crash.contains(addr) {
            eprintln!("chaos: crash during apply {at}: the process is killed");
            std::process::exit(137);
        }
        if cloud.chaos.fail.contains(addr) {
            bail!("apply {at}: injected failure (chaos fail={at})");
        }
        if let Some(ms) = cloud.chaos.latency.get(addr) {
            self.clock += ms;
            cloud.note(format!("latency {at}: {ms}ms (simulated, not slept)"));
        }
        // A timed-out call takes effect in the world, but dform never
        // hears back: no identity is recorded.
        let answered = !cloud.chaos.timeout.contains(addr);
        let deps = self.desired.get(addr).map(|r| r.deps.clone());
        match a.kind {
            ActionKind::Noop | ActionKind::Pending => {}
            ActionKind::Delete => {
                if let Some(entry) = state.get(addr) {
                    self.delete_object(&addr.typ, &entry.remote.clone());
                    if answered {
                        state.remove(addr);
                    }
                }
            }
            ActionKind::DeleteDeposed => {
                if let Some(entry) = state.deposed.get(&state::key(addr)) {
                    self.delete_object(&addr.typ, &entry.remote.clone());
                    if answered {
                        state.deposed.remove(&state::key(addr));
                    }
                }
            }
            ActionKind::Create => {
                let remote = addr.name.clone();
                if self.world.resources.contains_key(&key(&addr.typ, &remote)) {
                    bail!(
                        "apply {at}: create failed: {} already exists in the world",
                        key(&addr.typ, &remote)
                    );
                }
                self.create_object(addr, &remote, doc);
                if answered {
                    state.set(addr.clone(), cloud.id().to_string(), remote);
                }
            }
            ActionKind::Replace { create_first } => {
                let Some(old) = state.get(addr).map(|e| e.remote.clone()) else {
                    bail!("apply {at}: replace without a state entry");
                };
                if create_first {
                    // The old object stays, deposed, until it is deleted
                    // after what depends on it has moved to the new one.
                    state.depose(addr);
                } else {
                    self.delete_object(&addr.typ, &old);
                    state.remove(addr);
                }
                let remote = self.free_name(&addr.typ, &addr.name);
                self.create_object(addr, &remote, doc);
                if answered {
                    state.set(addr.clone(), cloud.id().to_string(), remote);
                }
            }
            ActionKind::Adopt => {
                let Some(remote_name) = self.adopt_map.get(addr) else {
                    bail!("apply {at}: adopt action missing adopt mapping");
                };
                let k = key(&addr.typ, remote_name);
                let Some(inv_rr) = self.inv.resources.get(&k) else {
                    bail!("apply {at}: adopt requested but inventory missing {k}");
                };
                let computed = inv_rr.computed.clone();
                self.world.resources.insert(
                    k,
                    RemoteResource {
                        typ: addr.typ.clone(),
                        name: remote_name.clone(),
                        attrs: doc,
                        computed,
                        hidden_until: None,
                    },
                );
                if answered {
                    state.set(addr.clone(), cloud.id().to_string(), remote_name.clone());
                }
            }
            ActionKind::Update | ActionKind::Drift => {
                let Some(entry) = state.get(addr) else {
                    bail!("apply {at}: update without a state entry");
                };
                let k = key(&addr.typ, &entry.remote);
                let computed = match self.world.resources.get(&k) {
                    Some(cur) => cur.computed.clone(),
                    None => cloud.mint(&addr.typ, &entry.remote, &doc),
                };
                self.world.resources.insert(
                    k,
                    RemoteResource {
                        typ: addr.typ.clone(),
                        name: entry.remote.clone(),
                        attrs: doc,
                        computed,
                        hidden_until: None,
                    },
                );
            }
        }
        if let Some(deps) = deps {
            state.set_deps(addr, deps);
        }
        cloud.save(&self.world)?;
        if !answered {
            bail!(
                "apply {at}: timed out waiting for the provider (chaos timeout={at}); \
                 the change may have taken effect"
            );
        }
        Ok(())
    }

    /// A new object at `remote`, its computed values minted.
    fn create_object(&mut self, addr: &Address, remote: &str, doc: Json) {
        let computed = self.cloud.mint(&addr.typ, remote, &doc);
        let hidden_until = self
            .cloud
            .chaos
            .read_lag
            .get(addr)
            .map(|k| self.world.tick + k);
        self.world.resources.insert(
            key(&addr.typ, remote),
            RemoteResource {
                typ: addr.typ.clone(),
                name: remote.to_string(),
                attrs: doc,
                computed,
                hidden_until,
            },
        );
    }

    fn delete_object(&mut self, typ: &str, remote: &str) {
        self.world.resources.remove(&key(typ, remote));
    }

    /// `name`, or the first `name-N` no object of `typ` has: a replacement
    /// created before its old object is deleted cannot take its name.
    fn free_name(&self, typ: &str, name: &str) -> String {
        std::iter::once(name.to_string())
            .chain((2..).map(|n| format!("{name}-{n}")))
            .find(|r| !self.world.resources.contains_key(&key(typ, r)))
            .unwrap()
    }

    /// The tick ends: state records every desired object's dependencies,
    /// chaos mutations land, the clock advances, the world is saved.
    pub fn end(mut self, state: &mut State) -> Result<()> {
        // What each object depends on, for ordering its delete later.
        for (addr, r) in &self.desired {
            state.set_deps(addr, r.deps.iter().cloned());
        }
        if self.clock > 0 {
            self.cloud
                .note(format!("simulated apply time: {}ms", self.clock));
        }
        self.cloud.end_tick(&mut self.world, state);
        self.cloud.save(&self.world)
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

/// A world value as the evaluator's value.
pub fn json_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Str("null".into()),
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or(Value::Str(n.to_string())),
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(xs) => Value::List(xs.iter().map(json_to_value).collect()),
        Json::Object(m) => Value::Obj(
            m.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
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

fn short_hash(s: &str) -> String {
    // FNV-1a, printed base 36: deterministic across runs and platforms.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::new();
    for _ in 0..5 {
        out.push(digits[(h % 36) as usize] as char);
        h /= 36;
    }
    out
}

pub fn set_path(v: &mut Json, path: &str, x: Json) {
    let mut cur = v;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let m = cur.as_object_mut().unwrap();
        if parts.peek().is_none() {
            m.insert(p.to_string(), x);
            return;
        }
        cur = m.entry(p.to_string()).or_insert_with(|| json!({}));
    }
}

/// The value at a keypath (`tags.owner`, `subnets[0].id`) in nested JSON, the
/// shape `ir::insert_keypath` builds and `flatten` spells.
pub fn get_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        let (key, mut rest) = match seg.find('[') {
            Some(i) => (&seg[..i], &seg[i..]),
            None => (seg, ""),
        };
        if !key.is_empty() {
            cur = cur.get(key)?;
        }
        while let Some(r) = rest.strip_prefix('[') {
            let (idx, tail) = r.split_once(']')?;
            cur = cur.get(idx.parse::<usize>().ok()?)?;
            rest = tail;
        }
        if !rest.is_empty() {
            return None;
        }
    }
    Some(cur)
}

fn flatten_json_facts(
    out: &mut Vec<Atom>,
    pred: &str,
    typ: &str,
    name: &str,
    prefix: &str,
    v: &serde_json::Value,
) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, vv) in m {
                let p = if prefix.is_empty() {
                    k.to_string()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_json_facts(out, pred, typ, name, &p, vv);
            }
        }
        serde_json::Value::Array(xs) => {
            for (i, vv) in xs.iter().enumerate() {
                let p = format!("{prefix}[{i}]");
                flatten_json_facts(out, pred, typ, name, &p, vv);
            }
        }
        other => {
            let val = match other {
                serde_json::Value::String(s) => Value::Str(s.clone()),
                serde_json::Value::Bool(b) => Value::Bool(*b),
                serde_json::Value::Number(n) => n
                    .as_i64()
                    .map(Value::Int)
                    .unwrap_or(Value::Str(n.to_string())),
                serde_json::Value::Null => Value::Str("null".to_string()),
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
            });
        }
    }
}

/// A change's path without list indices or keys: the schema's spelling.
fn norm_path(path: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in path.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Deletes in reverse dependency order: an object goes before every object
/// at an address it depended on. Ties keep address order.
fn reverse_dependency_order(mut deletes: Vec<(Action, &[String])>) -> Vec<Action> {
    let mut out = Vec::new();
    while !deletes.is_empty() {
        // Ready: nothing still to be deleted depends on it.
        let ready = (0..deletes.len())
            .find(|&i| {
                let k = state::key(&deletes[i].0.addr);
                !deletes
                    .iter()
                    .enumerate()
                    .any(|(j, (_, deps))| j != i && deps.contains(&k))
            })
            // A cycle cannot come from a DAG; break it in address order.
            .unwrap_or(0);
        out.push(deletes.remove(ready).0);
    }
    out
}

fn topo_sort(desired: &[Resource]) -> Result<Vec<Resource>> {
    let mut by_addr: BTreeMap<Address, &Resource> = BTreeMap::new();
    for r in desired {
        by_addr.insert(r.addr.clone(), r);
    }

    let mut pending: BTreeSet<Address> = by_addr.keys().cloned().collect();
    let mut done: BTreeSet<Address> = BTreeSet::new();
    let mut out = Vec::new();

    let mut guard = 0usize;
    while !pending.is_empty() {
        guard += 1;
        if guard > 10_000 {
            bail!("dependency resolution did not converge");
        }
        let mut progressed = false;
        let snapshot: Vec<Address> = pending.iter().cloned().collect();
        for a in snapshot {
            let r = by_addr.get(&a).unwrap();
            if r.deps
                .iter()
                .all(|d| done.contains(d) || !by_addr.contains_key(d))
            {
                pending.remove(&a);
                done.insert(a.clone());
                out.push((*r).clone());
                progressed = true;
            }
        }
        if !progressed {
            bail!("dependency cycle detected");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_path_walks_dots_and_indices() {
        let v = json!({"tags": {"owner": "team-a"}, "subnets": [{"id": "s-0"}, {"id": "s-1"}], "id": "x"});
        assert_eq!(get_path(&v, "id"), Some(&json!("x")));
        assert_eq!(get_path(&v, "tags.owner"), Some(&json!("team-a")));
        assert_eq!(get_path(&v, "subnets[1].id"), Some(&json!("s-1")));
        assert_eq!(get_path(&v, "subnets[2].id"), None);
        assert_eq!(get_path(&v, "tags.missing"), None);
    }
}
