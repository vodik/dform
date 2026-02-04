use crate::ir::{Address, Resource};
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

#[derive(Debug, Clone)]
pub enum ActionKind {
    Create,
    Adopt,
    Update,
    Delete,
    Noop,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: ActionKind,
    pub addr: Address,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub actions: Vec<Action>,
}

pub struct FakeCloud {
    root: PathBuf,
}

impl FakeCloud {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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

    pub fn save(&self, st: &RemoteState) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(|| format!("mkdir {}", self.root.display()))?;
        let bytes = serde_json::to_vec_pretty(st)?;
        fs::write(self.remote_path(), bytes)?;
        Ok(())
    }

    pub fn plan(&self, desired: &[Resource], adopts: &[crate::ir::Adopt]) -> Result<Plan> {
        let remote = self.load()?;
        let inv = self.load_inventory()?;
        let adopt_map: BTreeMap<Address, String> = adopts
            .iter()
            .map(|a| (a.addr.clone(), a.remote.clone()))
            .collect();
        let desired_set: BTreeSet<Address> = desired.iter().map(|r| r.addr.clone()).collect();

        let mut actions = Vec::new();

        // Create/update/noop from desired.
        for r in topo_sort(desired)? {
            let key = addr_key(&r.addr);
            let mut resolver = |kind: RefKind, typ: &str, name: &str, attr: &str| {
                match kind {
                    RefKind::Resource => {
                        // Prefer existing remote computed, then inventory if adopt says so, else fallback.
                        if let Some(cur) = remote
                            .resources
                            .get(&addr_key(&Address { typ: typ.to_string(), name: name.to_string() }))
                        {
                            if let Some(v) = cur.computed.get(attr) {
                                return Ok(v.clone());
                            }
                        }
                        if let Some(remote_name) =
                            adopt_map.get(&Address { typ: typ.to_string(), name: name.to_string() })
                        {
                            if let Some(cur) = inv.resources.get(&format!("{}::{}", typ, remote_name)) {
                                if let Some(v) = cur.computed.get(attr) {
                                    return Ok(v.clone());
                                }
                            }
                        }
                        Ok(json!(
                            computed_ref(typ, name, attr)
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
            match remote.resources.get(&key) {
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

        // Deletes for anything in remote not in desired.
        for rr in remote.resources.values() {
            let addr = Address {
                typ: rr.typ.clone(),
                name: rr.name.clone(),
            };
            if !desired_set.contains(&addr) {
                actions.push(Action {
                    kind: ActionKind::Delete,
                    addr,
                    changes: diff_attrs(Some(&rr.attrs), None),
                });
            }
        }

        Ok(Plan { actions })
    }

    pub fn apply(
        &self,
        desired: &[Resource],
        plan: &Plan,
        adopts: &[crate::ir::Adopt],
    ) -> Result<RemoteState> {
        let mut remote = self.load()?;
        let inv = self.load_inventory()?;
        let adopt_map: BTreeMap<Address, String> = adopts
            .iter()
            .map(|a| (a.addr.clone(), a.remote.clone()))
            .collect();
        let desired_by_addr: BTreeMap<Address, &Resource> = desired.iter().map(|r| (r.addr.clone(), r)).collect();

        for a in &plan.actions {
            let key = addr_key(&a.addr);
            match a.kind {
                ActionKind::Noop => {}
                ActionKind::Create => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for create"))?;
                    // Create doesn't have inventory context; forbid cloud_ref.
                    let attrs = resolve_json(&r.attrs)?;
                    let computed = computed_for(&a.addr);
                    remote.resources.insert(
                        key,
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: a.addr.name.clone(),
                            attrs,
                            computed,
                        },
                    );
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

                    // Adopt by copying computed fields from inventory.
                    let inv_key = format!("{}::{}", a.addr.typ, remote_name);
                    let Some(inv_rr) = inv.resources.get(&inv_key) else {
                        bail!("adopt requested but inventory missing {inv_key}");
                    };
                    remote.resources.insert(
                        key,
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: a.addr.name.clone(),
                            attrs,
                            computed: inv_rr.computed.clone(),
                        },
                    );
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
                    let computed = remote
                        .resources
                        .get(&key)
                        .map(|x| x.computed.clone())
                        .unwrap_or_else(|| computed_for(&a.addr));
                    remote.resources.insert(
                        key,
                        RemoteResource {
                            typ: a.addr.typ.clone(),
                            name: a.addr.name.clone(),
                            attrs,
                            computed,
                        },
                    );
                }
                ActionKind::Delete => {
                    remote.resources.remove(&key);
                }
            }
        }

        self.save(&remote)?;
        Ok(remote)
    }

    fn remote_path(&self) -> PathBuf {
        self.root.join("remote.json")
    }

    fn inventory_path(&self) -> PathBuf {
        self.root.join("inventory.json")
    }
}

fn addr_key(a: &Address) -> String {
    format!("{}::{}", a.typ, a.name)
}

fn computed_for(addr: &Address) -> serde_json::Value {
    let id = computed_ref(&addr.typ, &addr.name, "id").unwrap();
    match addr.typ.as_str() {
        "db.postgres" => json!({
            "id": id,
            "endpoint": computed_ref(&addr.typ, &addr.name, "endpoint").unwrap(),
        }),
        "k8s.cluster" => json!({
            "id": id,
            "api_endpoint": computed_ref(&addr.typ, &addr.name, "api_endpoint").unwrap(),
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
        Value::Ref { typ, name, attr } => {
            resolve_ref(RefKind::Resource, typ, name, attr)
        }
        Value::CloudRef { typ, name, attr } => {
            resolve_ref(RefKind::Cloud, typ, name, attr)
        }
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
