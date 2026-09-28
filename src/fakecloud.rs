use crate::ir::{Address, Resource};
use crate::provider::{Action, ActionKind, Change, Plan, Provider};
use crate::state::{self, State};
use crate::value::Value;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use crate::ast::{Atom, Term};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteState {
    pub resources: BTreeMap<String, RemoteResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResource {
    pub typ: String,
    pub name: String,
    pub attrs: serde_json::Value,
    pub computed: serde_json::Value,
}

pub struct FakeCloud {
    world: PathBuf,
    inventory: PathBuf,
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

    fn plan(&self, desired: &[Resource], adopts: &[crate::ir::Adopt], state: &State) -> Result<Plan> {
        self.plan_with_state(desired, adopts, state)
    }

    fn apply(
        &self,
        desired: &[Resource],
        adopts: &[crate::ir::Adopt],
        state: &mut State,
        plan: &Plan,
    ) -> Result<()> {
        self.apply_with_state(desired, plan, adopts, state)
    }
}

impl FakeCloud {
    /// A fake cloud whose world is `<root>/remote.json` and whose inventory is
    /// `<root>/inventory.json`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self::with_paths(root.join("remote.json"), root.join("inventory.json"))
    }

    pub fn with_paths(world: impl Into<PathBuf>, inventory: impl Into<PathBuf>) -> Self {
        Self {
            world: world.into(),
            inventory: inventory.into(),
        }
    }

    pub fn load(&self) -> Result<RemoteState> {
        let path = self.remote_path();
        if !path.exists() {
            return Ok(RemoteState {
                resources: BTreeMap::new(),
            });
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let st: RemoteState = serde_json::from_slice(&bytes).context("parse remote")?;
        Ok(st)
    }

    pub fn load_inventory(&self) -> Result<RemoteState> {
        let path = self.inventory_path();
        if !path.exists() {
            return Ok(RemoteState {
                resources: BTreeMap::new(),
            });
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let st: RemoteState = serde_json::from_slice(&bytes).context("parse inventory")?;
        Ok(st)
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
            flatten_json_facts(
                &mut out,
                "cloud_attr",
                &rr.typ,
                &rr.name,
                "",
                &rr.attrs,
            );
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

    pub fn catalog(&self) -> Result<Vec<Atom>> {
        // Provide simple type ownership + a few capability conventions.
        // In a real system, providers would return these from compiled metadata.
        let provider = "fakecloud";
        let types = [
            "net.vpc",
            "net.subnet",
            "net.vpc_peering",
            "compute.vm",
            "db.postgres",
            "k8s.cluster",
            "k8s.nodepool",
            "iam.role",
            "iam.policy",
            "iam.role_policy_attachment",
        ];

        let mut out = Vec::new();
        for t in types {
            out.push(Atom {
                pred: "type_provider".to_string(),
                args: vec![
                    Term::Val(Value::Str(t.to_string())),
                    Term::Val(Value::Str(provider.to_string())),
                ],
                record: None,
            });
            out.push(Atom {
                pred: "capability".to_string(),
                args: vec![
                    Term::Val(Value::Str(t.to_string())),
                    Term::Val(Value::Str("taggable".to_string())),
                ],
                record: None,
            });
            out.push(Atom {
                pred: "tag_path".to_string(),
                args: vec![
                    Term::Val(Value::Str(t.to_string())),
                    Term::Val(Value::Str("tags".to_string())),
                ],
                record: None,
            });
        }
        Ok(out)
    }

    pub fn save(&self, st: &RemoteState) -> Result<()> {
        if let Some(dir) = self.world.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(st)?;
        fs::write(&self.world, bytes).with_context(|| format!("write {}", self.world.display()))?;
        Ok(())
    }

    pub fn plan_with_state(
        &self,
        desired: &[Resource],
        adopts: &[crate::ir::Adopt],
        state: &State,
    ) -> Result<Plan> {
        let remote = self.load()?;
        let inv = self.load_inventory()?;
        let adopt_map: BTreeMap<Address, String> = state::adopt_map(adopts);
        let desired_set: BTreeSet<Address> = desired.iter().map(|r| r.addr.clone()).collect();

        let mut actions = Vec::new();

        // Create/update/noop from desired.
        for r in topo_sort(desired)? {
            let mut resolver = |kind: RefKind, typ: &str, name: &str, attr: &str| {
                match kind {
                    RefKind::Resource => {
                        // Prefer existing remote computed, then inventory if adopt says so, else fallback.
                        let addr = Address {
                            typ: typ.to_string(),
                            name: name.to_string(),
                        };
                        let rn = remote_name_for(&addr, state, &adopt_map);
                        if let Some(cur) = remote.resources.get(&format!("{}::{}", typ, rn)) {
                            if let Some(v) = cur.computed.get(attr) {
                                return Ok(v.clone());
                            }
                        }
                        if let Some(remote_name) = adopt_map.get(&addr) {
                            if let Some(cur) = inv.resources.get(&format!("{}::{}", typ, remote_name)) {
                                if let Some(v) = cur.computed.get(attr) {
                                    return Ok(v.clone());
                                }
                            }
                        }
                        Ok(json!(
                            computed_ref(typ, &rn, attr)
                                .unwrap_or_else(|| format!("{typ}:{name}"))
                        ))
                    }
                    RefKind::Cloud => {
                        let key = format!("{}::{}", typ, name);
                        let Some(cur) = inv.resources.get(&key) else {
                            bail!("cloud_ref missing inventory resource {key}");
                        };
                        if let Some(v) = cur.computed.get(attr) {
                            return Ok(v.clone());
                        }
                        if let Some(v) = cur.attrs.get(attr) {
                            return Ok(v.clone());
                        }
                        bail!("cloud_ref missing attribute {typ}.{name}.{attr}");
                    }
                }
            };
            let desired_attrs = resolve_json_with(&r.attrs, &mut resolver)?;

            match state.get(&r.addr) {
                None => {
                    if let Some(remote_name) = adopt_map.get(&r.addr) {
                        let inv_key = format!("{}::{}", r.addr.typ, remote_name);
                        let Some(inv_rr) = inv.resources.get(&inv_key) else {
                            bail!("adopt requested but inventory missing {inv_key}");
                        };
                        actions.push(Action {
                            kind: ActionKind::Adopt,
                            addr: r.addr.clone(),
                            changes: diff_attrs(Some(&inv_rr.attrs), Some(&desired_attrs)),
                        });
                    } else {
                        actions.push(Action {
                            kind: ActionKind::Create,
                            addr: r.addr.clone(),
                            changes: diff_attrs(None, Some(&desired_attrs)),
                        });
                    }
                }
                Some(entry) => {
                    let key = format!("{}::{}", r.addr.typ, entry.remote);
                    let cur = remote.resources.get(&key);
                    match cur {
                        None => {
                            // Drift: state says it existed but the world doesn't.
                            actions.push(Action {
                                kind: ActionKind::Create,
                                addr: r.addr.clone(),
                                changes: diff_attrs(None, Some(&desired_attrs)),
                            });
                        }
                        Some(cur) => {
                            if cur.attrs == desired_attrs {
                                actions.push(Action {
                                    kind: ActionKind::Noop,
                                    addr: r.addr.clone(),
                                    changes: Vec::new(),
                                });
                            } else {
                                actions.push(Action {
                                    kind: ActionKind::Update,
                                    addr: r.addr.clone(),
                                    changes: diff_attrs(Some(&cur.attrs), Some(&desired_attrs)),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Deletes for anything in state not in desired.
        for (addr, entry) in state.entries_for_provider("fakecloud") {
            if desired_set.contains(&addr) {
                continue;
            }
            let key = format!("{}::{}", addr.typ, entry.remote);
            let before = remote.resources.get(&key).map(|x| &x.attrs);
            actions.push(Action {
                kind: ActionKind::Delete,
                addr,
                changes: diff_attrs(before, None),
            });
        }

        Ok(Plan { actions })
    }

    pub fn apply_with_state(
        &self,
        desired: &[Resource],
        plan: &Plan,
        adopts: &[crate::ir::Adopt],
        state: &mut State,
    ) -> Result<()> {
        let mut remote = self.load()?;
        let inv = self.load_inventory()?;
        let adopt_map: BTreeMap<Address, String> = state::adopt_map(adopts);
        let desired_by_addr: BTreeMap<Address, &Resource> = desired.iter().map(|r| (r.addr.clone(), r)).collect();

        for a in &plan.actions {
            match a.kind {
                ActionKind::Noop => {}
                ActionKind::Create => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for create"))?;
                    // Create doesn't have inventory context; forbid cloud_ref.
                    let attrs = resolve_json(&r.attrs)?;
                    let remote_name = a.addr.name.clone();
                    let key = format!("{}::{}", a.addr.typ, remote_name);
                    let computed = computed_for(&a.addr.typ, &remote_name);
                    remote.resources.insert(
                        key,
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: remote_name.clone(),
                            attrs,
                            computed,
                        },
                    );
                    state.set(a.addr.clone(), "fakecloud".to_string(), remote_name);
                }
                ActionKind::Adopt => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for adopt"))?;
                    let mut resolver = |kind: RefKind, typ: &str, name: &str, attr: &str| {
                        match kind {
                            RefKind::Resource => Ok(json!(
                                computed_ref(typ, name, attr)
                                    .unwrap_or_else(|| format!("{typ}:{name}"))
                            )),
                            RefKind::Cloud => {
                                let key = format!("{}::{}", typ, name);
                                let Some(cur) = inv.resources.get(&key) else {
                                    bail!("cloud_ref missing inventory resource {key}");
                                };
                                if let Some(v) = cur.computed.get(attr) {
                                    return Ok(v.clone());
                                }
                                if let Some(v) = cur.attrs.get(attr) {
                                    return Ok(v.clone());
                                }
                                bail!("cloud_ref missing attribute {typ}.{name}.{attr}");
                            }
                        }
                    };
                    let attrs = resolve_json_with(&r.attrs, &mut resolver)?;

                    let Some(remote_name) = adopt_map.get(&a.addr) else {
                        bail!("adopt action missing adopt mapping");
                    };

                    let key = format!("{}::{}", a.addr.typ, remote_name);

                    // Adopt by copying computed fields from inventory.
                    let inv_key = format!("{}::{}", a.addr.typ, remote_name);
                    let Some(inv_rr) = inv.resources.get(&inv_key) else {
                        bail!("adopt requested but inventory missing {inv_key}");
                    };
                    remote.resources.insert(
                        key.clone(),
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: remote_name.clone(),
                            attrs,
                            computed: inv_rr.computed.clone(),
                        },
                    );
                    state.set(a.addr.clone(), "fakecloud".to_string(), remote_name.clone());
                }
                ActionKind::Update => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for update"))?;
                    let mut resolver = |kind: RefKind, typ: &str, name: &str, attr: &str| {
                        match kind {
                            RefKind::Resource => Ok(json!(
                                computed_ref(typ, name, attr)
                                    .unwrap_or_else(|| format!("{typ}:{name}"))
                            )),
                            RefKind::Cloud => {
                                let key = format!("{}::{}", typ, name);
                                let Some(cur) = inv.resources.get(&key) else {
                                    bail!("cloud_ref missing inventory resource {key}");
                                };
                                if let Some(v) = cur.computed.get(attr) {
                                    return Ok(v.clone());
                                }
                                if let Some(v) = cur.attrs.get(attr) {
                                    return Ok(v.clone());
                                }
                                bail!("cloud_ref missing attribute {typ}.{name}.{attr}");
                            }
                        }
                    };
                    let attrs = resolve_json_with(&r.attrs, &mut resolver)?;

                    let Some(entry) = state.get(&a.addr) else {
                        bail!("update missing state entry");
                    };
                    let key = format!("{}::{}", a.addr.typ, entry.remote);
                    let computed = remote
                        .resources
                        .get(&key)
                        .map(|x| x.computed.clone())
                        .unwrap_or_else(|| computed_for(&a.addr.typ, &entry.remote));
                    remote.resources.insert(
                        key.clone(),
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: entry.remote.clone(),
                            attrs,
                            computed,
                        },
                    );
                }
                ActionKind::Delete => {
                    let Some(entry) = state.get(&a.addr) else {
                        continue;
                    };
                    let key = format!("{}::{}", a.addr.typ, entry.remote);
                    remote.resources.remove(&key);
                    state.remove(&a.addr);
                }
            }
        }

        self.save(&remote)?;
        Ok(())
    }

    fn remote_path(&self) -> PathBuf {
        self.world.clone()
    }

    fn inventory_path(&self) -> PathBuf {
        self.inventory.clone()
    }
}

fn remote_name_for(addr: &Address, state: &State, adopts: &BTreeMap<Address, String>) -> String {
    if let Some(e) = state.get(addr) {
        return e.remote.clone();
    }
    if let Some(r) = adopts.get(addr) {
        return r.clone();
    }
    addr.name.clone()
}

fn computed_for(typ: &str, remote_name: &str) -> serde_json::Value {
    let id = computed_ref(typ, remote_name, "id").unwrap();
    match typ {
        "db.postgres" => json!({
            "id": id,
            "endpoint": computed_ref(typ, remote_name, "endpoint").unwrap(),
        }),
        "k8s.cluster" => json!({
            "id": id,
            "api_endpoint": computed_ref(typ, remote_name, "api_endpoint").unwrap(),
            "ca_cert": "FAKECERT",
        }),
        _ => json!({ "id": id }),
    }
}

fn computed_ref(typ: &str, name: &str, attr: &str) -> Option<String> {
    match attr {
        "id" => Some(format!("{typ}:{name}")),
        "endpoint" if typ == "db.postgres" => Some(format!("{name}.db.fake")),
        "api_endpoint" if typ == "k8s.cluster" => Some(format!("https://{name}.k8s.fake")),
        "ca_cert" if typ == "k8s.cluster" => Some("FAKECERT".to_string()),
        _ => None,
    }
}

#[derive(Debug, Copy, Clone)]
enum RefKind {
    Resource,
    Cloud,
}

fn resolve_json(v: &Value) -> Result<serde_json::Value> {
    let mut resolver = |kind: RefKind, typ: &str, name: &str, attr: &str| {
        match kind {
            RefKind::Resource => Ok(json!(
                computed_ref(typ, name, attr).unwrap_or_else(|| format!("{typ}:{name}"))
            )),
            RefKind::Cloud => bail!("cloud_ref requires a resolver"),
        }
    };
    resolve_json_with(v, &mut resolver)
}

fn resolve_json_with(
    v: &Value,
    resolve_ref: &mut dyn FnMut(RefKind, &str, &str, &str) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    match v {
        Value::Str(s) => Ok(json!(s)),
        Value::Int(i) => Ok(json!(i)),
        Value::Bool(b) => Ok(json!(b)),
        Value::List(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(resolve_json_with(x, resolve_ref)?);
            }
            Ok(json!(out))
        }
        Value::Obj(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                out.insert(k.clone(), resolve_json_with(x, resolve_ref)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        Value::Ip(n) => Ok(json!(crate::value::u32_to_ipv4(*n))),
        Value::IpNet { addr, prefix } => Ok(json!(crate::value::ipnet_to_string(*addr, *prefix))),
        Value::IpRange { start, end } => Ok(json!(format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        ))),
        Value::Ref { typ, name, attr } => {
            resolve_ref(RefKind::Resource, typ, name, attr)
        }
        Value::CloudRef { typ, name, attr } => {
            resolve_ref(RefKind::Cloud, typ, name, attr)
        }
        Value::Null { label, .. } => Err(anyhow::anyhow!("unresolved null ?{label} reached the provider")),
    }
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
                serde_json::Value::Number(n) => n.as_i64().map(Value::Int).unwrap_or(Value::Str(n.to_string())),
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

fn diff_attrs(
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
) -> Vec<Change> {
    let mut a = BTreeMap::<String, serde_json::Value>::new();
    let mut b = BTreeMap::<String, serde_json::Value>::new();

    if let Some(v) = before {
        flatten(v, "", &mut a);
    }
    if let Some(v) = after {
        flatten(v, "", &mut b);
    }

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(a.keys().cloned());
    paths.extend(b.keys().cloned());

    let mut out = Vec::new();
    for p in paths {
        let av = a.get(&p);
        let bv = b.get(&p);
        if av == bv {
            continue;
        }
        out.push(Change {
            path: p,
            before: av.cloned(),
            after: bv.cloned(),
        });
    }
    out
}

fn flatten(v: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, serde_json::Value>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, vv) in m {
                let p = if prefix.is_empty() {
                    k.to_string()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(vv, &p, out);
            }
        }
        serde_json::Value::Array(xs) => {
            for (i, vv) in xs.iter().enumerate() {
                let p = format!("{prefix}[{i}]");
                flatten(vv, &p, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
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
            if r.deps.iter().all(|d| done.contains(d) || !by_addr.contains_key(d)) {
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

// Silence dead_code warnings if used as a library later.
#[allow(dead_code)]
fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("mkdir {}", path.display()))
}
