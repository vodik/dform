use crate::ir::{Address, Resource};
use crate::value::Value;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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

    pub fn save(&self, st: &RemoteState) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(|| format!("mkdir {}", self.root.display()))?;
        let bytes = serde_json::to_vec_pretty(st)?;
        fs::write(self.remote_path(), bytes)?;
        Ok(())
    }

    pub fn plan(&self, desired: &[Resource]) -> Result<Plan> {
        let remote = self.load()?;
        let desired_set: BTreeSet<Address> = desired.iter().map(|r| r.addr.clone()).collect();

        let mut actions = Vec::new();

        // Create/update/noop from desired.
        for r in topo_sort(desired)? {
            let key = addr_key(&r.addr);
            let desired_attrs = resolve_json(&r.attrs)?;
            match remote.resources.get(&key) {
                None => actions.push(Action {
                    kind: ActionKind::Create,
                    addr: r.addr.clone(),
                    changes: diff_attrs(None, Some(&desired_attrs)),
                }),
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

    pub fn apply(&self, desired: &[Resource], plan: &Plan) -> Result<RemoteState> {
        let mut remote = self.load()?;
        let desired_by_addr: BTreeMap<Address, &Resource> = desired.iter().map(|r| (r.addr.clone(), r)).collect();

        for a in &plan.actions {
            let key = addr_key(&a.addr);
            match a.kind {
                ActionKind::Noop => {}
                ActionKind::Create => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for create"))?;
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
                ActionKind::Update => {
                    let r = desired_by_addr
                        .get(&a.addr)
                        .ok_or_else(|| anyhow!("missing desired resource for update"))?;
                    let attrs = resolve_json(&r.attrs)?;
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

fn resolve_json(v: &Value) -> Result<serde_json::Value> {
    match v {
        Value::Str(s) => Ok(json!(s)),
        Value::Int(i) => Ok(json!(i)),
        Value::Bool(b) => Ok(json!(b)),
        Value::List(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(resolve_json(x)?);
            }
            Ok(json!(out))
        }
        Value::Obj(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                out.insert(k.clone(), resolve_json(x)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        Value::Ref { typ, name, attr } => {
            let s = computed_ref(typ, name, attr).ok_or_else(|| {
                anyhow!("unknown ref attribute: ref({typ},{name},{attr})")
            })?;
            Ok(json!(s))
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
