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
use crate::ir::{Address, Adopt, Resource};
use crate::provider::{self, Action, ActionKind, Change, Plan, Provider};
use crate::schema::Schema;
use crate::state::{self, State};
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteState {
    pub resources: BTreeMap<String, RemoteResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResource {
    pub typ: String,
    pub name: String,
    pub attrs: Json,
    pub computed: Json,
}

pub struct FakeCloud {
    world: PathBuf,
    inventory: PathBuf,
    schema: Schema,
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

    fn plan(&self, desired: &[Resource], adopts: &[Adopt], state: &State) -> Result<Plan> {
        self.plan_with_state(desired, adopts, state)
    }

    fn apply(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        state: &mut State,
        plan: &Plan,
    ) -> Result<()> {
        self.apply_with_state(desired, plan, adopts, state)
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
        }
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

    /// The null class of `attr` on `typ`, or `None` for a configured attribute.
    /// An Optional+Computed attribute is computed only where the program does
    /// not set it. A type no loaded schema knows keeps the old convention of a
    /// fresh `id`.
    fn class_for(&self, typ: &str, attr: &str, configured: bool) -> Option<NullClass> {
        if let Some(c) = self.schema.class_of(typ, attr) {
            return Some(c);
        }
        if !configured && let Some(c) = self.schema.optional_computed_class(typ, attr) {
            return Some(c);
        }
        (attr == "id" && !self.schema.knows_type(typ)).then_some(NullClass::Fresh)
    }

    fn resolve_ref(&self, ctx: &Ctx, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let addr = Address {
            typ: typ.to_string(),
            name: name.to_string(),
        };
        let label = format!("{typ}/{name}#{attr}");
        let configured = ctx.resolved.get(&addr).and_then(|d| get_path(d, attr));
        let existing = ctx.existing(&addr);
        let unknown = |why: &str| -> Result<Json> {
            match ctx.strict {
                Some(at) => bail!(
                    "apply {}/{}: ?{label} is still unknown ({why})",
                    at.typ,
                    at.name
                ),
                None => Ok(provider::null_json(&label)),
            }
        };
        match self.class_for(typ, attr, configured.is_some()) {
            Some(NullClass::Secret) => {
                // Never the bytes: the label, which Apply materializes.
                if ctx.strict.is_some()
                    && existing
                        .and_then(|rr| get_path(&rr.computed, attr))
                        .is_none()
                {
                    return unknown(&format!("{typ}/{name} has not been created"));
                }
                Ok(provider::secret_json(&label))
            }
            Some(_) => match existing.and_then(|rr| get_path(&rr.computed, attr)) {
                Some(v) => Ok(v.clone()),
                None => unknown(&format!("{typ}/{name} has not been created")),
            },
            None => {
                match configured.or_else(|| existing.and_then(|rr| get_path(&rr.attrs, attr))) {
                    Some(v) => Ok(v.clone()),
                    None => unknown(&format!("{typ}/{name} does not set {attr}")),
                }
            }
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
            Value::Null { label, class, .. } => match class {
                NullClass::Secret => provider::secret_json(label),
                _ if ctx.strict.is_some() => bail!("unresolved null ?{label} reached the provider"),
                _ => provider::null_json(label),
            },
        })
    }

    /// The document dform wants for `r`, refs resolved, schema-computed paths
    /// dropped (the provider owns them; DR-11 revised).
    fn resolve_doc(&self, ctx: &Ctx, r: &Resource) -> Result<Json> {
        let mut doc = self.resolve_value(ctx, &r.attrs)?;
        for (attr, _) in self.schema.computed_of(&r.addr.typ) {
            remove_path(&mut doc, &attr);
        }
        Ok(doc)
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

    /// Refresh: the world as Read returns it.
    fn refresh(&self) -> Result<RemoteState> {
        self.load()
    }

    pub fn plan_with_state(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        state: &State,
    ) -> Result<Plan> {
        let world = self.refresh()?;
        let inv = self.load_inventory()?;
        let adopt_map = state::adopt_map(adopts);
        let desired_set: BTreeSet<Address> = desired.iter().map(|r| r.addr.clone()).collect();
        let mut resolved = BTreeMap::new();
        let mut actions = Vec::new();

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
            let typ = r.addr.typ.as_str();
            let (kind, changes) = match state.get(&r.addr) {
                None => match adopt_map.get(&r.addr) {
                    Some(remote_name) => {
                        let inv_key = key(typ, remote_name);
                        let Some(inv_rr) = inv.resources.get(&inv_key) else {
                            bail!("adopt requested but inventory missing {inv_key}");
                        };
                        (
                            ActionKind::Adopt,
                            self.diff(typ, Some(&inv_rr.attrs), Some(&doc)),
                        )
                    }
                    None => (ActionKind::Create, self.diff(typ, None, Some(&doc))),
                },
                Some(entry) => match world.resources.get(&key(typ, &entry.remote)) {
                    // Drift: state says it existed but the world doesn't.
                    None => (ActionKind::Create, self.diff(typ, None, Some(&doc))),
                    Some(cur) => {
                        let changes =
                            self.diff(typ, Some(&self.world_doc(typ, cur, &doc)), Some(&doc));
                        let kind = if changes.is_empty() {
                            ActionKind::Noop
                        } else {
                            ActionKind::Update
                        };
                        (kind, changes)
                    }
                },
            };
            resolved.insert(r.addr.clone(), doc);
            actions.push(Action {
                kind,
                addr: r.addr.clone(),
                changes,
            });
        }

        // Deletes for anything in state not in desired.
        for (addr, entry) in state.entries_for_provider("fakecloud") {
            if desired_set.contains(&addr) {
                continue;
            }
            let before = world
                .resources
                .get(&key(&addr.typ, &entry.remote))
                .map(|x| &x.attrs);
            let changes = self.diff(&addr.typ, before, None);
            actions.push(Action {
                kind: ActionKind::Delete,
                addr,
                changes,
            });
        }

        Ok(Plan { actions })
    }

    pub fn apply_with_state(
        &self,
        desired: &[Resource],
        plan: &Plan,
        adopts: &[Adopt],
        state: &mut State,
    ) -> Result<()> {
        let mut world = self.load()?;
        let inv = self.load_inventory()?;
        let adopt_map = state::adopt_map(adopts);
        let desired_by_addr: BTreeMap<Address, &Resource> =
            desired.iter().map(|r| (r.addr.clone(), r)).collect();
        let mut resolved: BTreeMap<Address, Json> = BTreeMap::new();

        for a in &plan.actions {
            let addr = &a.addr;
            if let ActionKind::Delete = a.kind {
                if let Some(entry) = state.get(addr) {
                    world.resources.remove(&key(&addr.typ, &entry.remote));
                    state.remove(addr);
                    self.save(&world)?;
                }
                continue;
            }
            let r = desired_by_addr
                .get(addr)
                .ok_or_else(|| anyhow!("apply {}/{}: no desired resource", addr.typ, addr.name))?;
            let doc = {
                let ctx = Ctx {
                    world: &world,
                    inv: &inv,
                    state,
                    adopts: &adopt_map,
                    resolved: &resolved,
                    strict: (!matches!(a.kind, ActionKind::Noop)).then_some(addr),
                };
                self.resolve_doc(&ctx, r)?
            };
            resolved.insert(addr.clone(), doc.clone());
            match a.kind {
                ActionKind::Noop | ActionKind::Delete => continue,
                ActionKind::Create => {
                    let remote_name = addr.name.clone();
                    let computed = self.mint(&addr.typ, &remote_name, &doc);
                    world.resources.insert(
                        key(&addr.typ, &remote_name),
                        RemoteResource {
                            typ: addr.typ.clone(),
                            name: remote_name.clone(),
                            attrs: doc,
                            computed,
                        },
                    );
                    state.set(addr.clone(), self.id().to_string(), remote_name);
                }
                ActionKind::Adopt => {
                    let Some(remote_name) = adopt_map.get(addr) else {
                        bail!("adopt action missing adopt mapping");
                    };
                    let k = key(&addr.typ, remote_name);
                    let Some(inv_rr) = inv.resources.get(&k) else {
                        bail!("adopt requested but inventory missing {k}");
                    };
                    let computed = inv_rr.computed.clone();
                    world.resources.insert(
                        k,
                        RemoteResource {
                            typ: addr.typ.clone(),
                            name: remote_name.clone(),
                            attrs: doc,
                            computed,
                        },
                    );
                    state.set(addr.clone(), self.id().to_string(), remote_name.clone());
                }
                ActionKind::Update => {
                    let Some(entry) = state.get(addr) else {
                        bail!(
                            "apply {}/{}: update without a state entry",
                            addr.typ,
                            addr.name
                        );
                    };
                    let k = key(&addr.typ, &entry.remote);
                    let computed = match world.resources.get(&k) {
                        Some(cur) => cur.computed.clone(),
                        None => self.mint(&addr.typ, &entry.remote, &doc),
                    };
                    world.resources.insert(
                        k,
                        RemoteResource {
                            typ: addr.typ.clone(),
                            name: entry.remote.clone(),
                            attrs: doc,
                            computed,
                        },
                    );
                }
            }
            // The cloud keeps what it did, whatever happens next.
            self.save(&world)?;
        }
        self.save(&world)?;
        Ok(())
    }

    /// What Apply returns for a new resource: every computed attribute of the
    /// type, and every Optional+Computed one the program left unset.
    fn mint(&self, typ: &str, name: &str, doc: &Json) -> Json {
        let mut out = json!({});
        for (attr, class) in self.schema.computed_of(typ) {
            set_path(&mut out, &attr, self.mint_value(typ, name, &attr, class));
        }
        for (attr, class) in self.schema.optional_computed_of(typ) {
            if get_path(doc, &attr).is_none() {
                set_path(&mut out, &attr, self.mint_value(typ, name, &attr, class));
            }
        }
        if !self.schema.knows_type(typ) {
            set_path(&mut out, "id", json!(format!("{typ}:{name}")));
        }
        out
    }

    fn mint_value(&self, typ: &str, name: &str, attr: &str, class: NullClass) -> Json {
        let hash = short_hash(&format!("{typ}/{name}#{attr}"));
        if let Some(tpl) = self.schema.mints.get(&(typ.to_string(), attr.to_string())) {
            let s = tpl
                .replace("{type}", typ)
                .replace("{name}", name)
                .replace("{attr}", attr)
                .replace("{hash}", &hash);
            return json!(s);
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
            (_, "int") => json!(u64::from_str_radix(&hash, 36).unwrap_or(0) % 100),
            (_, "bool") => json!(true),
            (_, "list" | "set") => json!([]),
            (_, "map" | "object") => json!({}),
            _ => json!(format!("{name}.{}.fake", attr.replace('.', "-"))),
        }
    }

    fn diff(&self, typ: &str, before: Option<&Json>, after: Option<&Json>) -> Vec<Change> {
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

fn remove_path(v: &mut Json, path: &str) {
    match path.split_once('.') {
        None => {
            if let Some(m) = v.as_object_mut() {
                m.remove(path);
            }
        }
        Some((head, rest)) => {
            if let Some(child) = v.get_mut(head) {
                remove_path(child, rest);
            }
        }
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
