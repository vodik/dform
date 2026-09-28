//! The plan as a Z-set (proposal E §2.8, DR-11), computed on the naive engine.
//!
//!   desired(T, A, Doc) :- want(T, A), Doc = assemble(T, A)
//!   world(T, A, Doc)   :- identity(T, A, Rid), world_doc(T, Rid, Doc)
//!   deformation        = desired − world          (Z-set over (T, A, Doc))
//!
//! Per address the group is {+1} create, {−1} delete, {+1, −1} update, empty
//! undeformed. Refs in desired documents become classed nulls and are
//! resolved at round 0 from the world's computed attributes through the
//! identity mapping (E Rule 4, "after refresh"). An update whose desired doc
//! still carries a null that the world side has a constant for is *pending*.
//!
//! The test compares the result with the current planner
//! (`FakeCloud::plan_with_state`) on the same inputs.

use crate::fakecloud::FakeCloud;
use crate::ir::{self, Address};
use crate::lattice::{eq3, nulls_in, Truth};
use crate::schema::Schema;
use crate::state::State;
use crate::value::{NullClass, Value};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Desired document with refs turned into nulls and provider-side value
/// spellings (ip, inet) normalized to what the fake provider stores.
pub fn desired_doc(v: &Value, schema: &Schema) -> Value {
    match v {
        Value::Ref { typ, name, attr } => Value::Null {
            label: format!("{typ}/{name}#{attr}"),
            class: schema.class_of(typ, attr).unwrap_or(NullClass::Open),
            ty: "any".into(),
        },
        Value::CloudRef { typ, name, attr } => Value::Null { label: format!("{typ}/{name}#{attr}"), class: NullClass::Open, ty: "any".into() },
        Value::Ip(n) => Value::Str(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => Value::Str(crate::value::ipnet_to_string(*addr, *prefix)),
        Value::IpRange { start, end } => Value::Str(format!("{}-{}", crate::value::u32_to_ipv4(*start), crate::value::u32_to_ipv4(*end))),
        Value::List(xs) => Value::List(xs.iter().map(|x| desired_doc(x, schema)).collect()),
        Value::Obj(m) => Value::Obj(m.iter().map(|(k, x)| (k.clone(), desired_doc(x, schema))).collect()),
        other => other.clone(),
    }
}

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Str("null".into()),
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n.as_i64().map(Value::Int).unwrap_or(Value::Str(n.to_string())),
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => Value::List(xs.iter().map(json_to_value).collect()),
        serde_json::Value::Object(m) => Value::Obj(m.iter().map(|(k, v)| (k.clone(), json_to_value(v))).collect()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Create,
    Delete,
    Update,
    Pending,
    Undeformed,
}

#[derive(Debug, Clone)]
pub struct Deformation {
    pub addr: Address,
    pub kind: Kind,
    pub weights: Vec<(i32, Value)>,
    pub unresolved: BTreeSet<String>,
}

pub struct World {
    pub identity: BTreeMap<Address, String>,
    pub docs: BTreeMap<(String, String), Value>,
    pub computed: BTreeMap<(String, String), Value>,
}

pub fn load_world(root: &Path) -> Result<World> {
    let st = State::load(&crate::state::state_path(root))?;
    let remote = FakeCloud::new(root.to_path_buf()).load()?;
    let mut identity = BTreeMap::new();
    for (k, e) in &st.resources {
        let Some((t, n)) = k.split_once("::") else { continue };
        identity.insert(Address { typ: t.into(), name: n.into() }, e.remote.clone());
    }
    let mut docs = BTreeMap::new();
    let mut computed = BTreeMap::new();
    for rr in remote.resources.values() {
        docs.insert((rr.typ.clone(), rr.name.clone()), json_to_value(&rr.attrs));
        computed.insert((rr.typ.clone(), rr.name.clone()), json_to_value(&rr.computed));
    }
    Ok(World { identity, docs, computed })
}

/// Round-0 resolution: `resolve(N, V) :- null_of(T, A, P, N), identity(T, A, Rid), world_attr(T, Rid, P, V)`.
pub fn resolutions(world: &World, nulls: &BTreeSet<String>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for label in nulls {
        let Some((ta, attr)) = label.split_once('#') else { continue };
        let Some((t, a)) = ta.split_once('/') else { continue };
        let addr = Address { typ: t.into(), name: a.into() };
        let Some(rid) = world.identity.get(&addr) else { continue };
        let Some(Value::Obj(c)) = world.computed.get(&(t.to_string(), rid.clone())) else { continue };
        if let Some(v) = c.get(attr) {
            out.insert(label.clone(), v.clone());
        }
    }
    out
}

pub fn subst_all(v: &Value, res: &BTreeMap<String, Value>) -> Value {
    let mut out = v.clone();
    for (l, c) in res {
        out = crate::lattice::subst(&out, l, c);
    }
    out
}

/// desired − world.
pub fn deformation(desired: &[ir::Resource], world: &World, schema: &Schema) -> Vec<Deformation> {
    // desired rows, resolved at round 0
    let mut rows: BTreeMap<Address, Vec<(i32, Value)>> = BTreeMap::new();
    let mut all_nulls = BTreeSet::new();
    let desired_docs: Vec<(Address, Value)> = desired
        .iter()
        .map(|r| {
            let d = desired_doc(&r.attrs, schema);
            all_nulls.extend(nulls_in(&d));
            (r.addr.clone(), d)
        })
        .collect();
    let res = resolutions(world, &all_nulls);
    for (addr, d) in desired_docs {
        rows.entry(addr).or_default().push((1, subst_all(&d, &res)));
    }
    // world rows, keyed by the identity mapping
    for (addr, rid) in &world.identity {
        if let Some(doc) = world.docs.get(&(addr.typ.clone(), rid.clone())) {
            rows.entry(addr.clone()).or_default().push((-1, doc.clone()));
        }
    }
    let mut out = Vec::new();
    for (addr, ws) in rows {
        // Z-set sum: equal documents cancel. Equality is eq3 (nulls).
        let mut kept: Vec<(i32, Value)> = Vec::new();
        let mut unresolved = BTreeSet::new();
        let mut pending = false;
        'outer: for (w, v) in ws {
            for k in kept.iter_mut() {
                match eq3(&k.1, &v) {
                    Truth::True => {
                        k.0 += w;
                        continue 'outer;
                    }
                    Truth::Unknown => {
                        pending = true;
                        unresolved.extend(nulls_in(&k.1));
                        unresolved.extend(nulls_in(&v));
                    }
                    Truth::False => {}
                }
            }
            kept.push((w, v));
        }
        kept.retain(|(w, _)| *w != 0);
        let plus = kept.iter().filter(|(w, _)| *w > 0).count();
        let minus = kept.iter().filter(|(w, _)| *w < 0).count();
        let kind = match (plus, minus, pending) {
            (0, 0, _) => Kind::Undeformed,
            (_, _, true) => Kind::Pending,
            (1, 0, _) => Kind::Create,
            (0, 1, _) => Kind::Delete,
            _ => Kind::Update,
        };
        for (_, v) in &kept {
            unresolved.extend(nulls_in(v));
        }
        out.push(Deformation { addr, kind, weights: kept, unresolved });
    }
    out
}

pub fn summary(ds: &[Deformation]) -> BTreeMap<Kind, Vec<String>> {
    let mut m: BTreeMap<Kind, Vec<String>> = BTreeMap::new();
    for d in ds {
        m.entry(d.kind.clone()).or_default().push(format!("{}.{}", d.addr.typ, d.addr.name));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Atom, Term};
    use crate::provider::{ActionKind, Provider};
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
    fn input(k: &str, v: &str) -> Atom {
        Atom { pred: "input".into(), args: vec![Term::Val(Value::Str(k.into())), Term::Val(Value::Str(v.into()))], record: None }
    }

    fn desired(extra: &[Atom]) -> Vec<ir::Resource> {
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        let (res, _viol) = crate::engine::eval(&program, extra).unwrap();
        ir::compile_resources(res.facts.iter().cloned()).unwrap()
    }

    fn planner_counts(world_root: &Path, desired: &[ir::Resource]) -> (usize, usize, usize, BTreeSet<String>) {
        let backend = FakeCloud::new(world_root.to_path_buf());
        let mut st = State::load(&crate::state::state_path(world_root)).unwrap();
        backend.bootstrap_state(&mut st).unwrap();
        let plan = backend.plan(desired, &[], &st).unwrap();
        let mut c = (0, 0, 0);
        let mut updated = BTreeSet::new();
        for a in &plan.actions {
            match a.kind {
                ActionKind::Create => c.0 += 1,
                ActionKind::Update => {
                    c.1 += 1;
                    updated.insert(format!("{}.{}", a.addr.typ, a.addr.name));
                }
                ActionKind::Delete => c.2 += 1,
                _ => {}
            }
        }
        (c.0, c.1, c.2, updated)
    }

    #[test]
    fn zset_matches_planner_on_empty_world_14_creates() {
        let d = desired(&[]);
        let empty = root().join("examples/world-empty");
        std::fs::create_dir_all(&empty).unwrap();
        let world = load_world(&empty).unwrap();
        let ds = deformation(&d, &world, &crate::schema::fake());
        let s = summary(&ds);
        println!("empty world: {:?}", s.iter().map(|(k, v)| (format!("{k:?}"), v.len())).collect::<Vec<_>>());
        for x in ds.iter().filter(|d| !d.unresolved.is_empty()) {
            println!("  create {}.{} carries {:?}", x.addr.typ, x.addr.name, x.unresolved);
        }
        assert_eq!(s.get(&Kind::Create).map(|v| v.len()).unwrap_or(0), 14);
        assert!(s.get(&Kind::Pending).is_none());
        let (pc, pu, pd, _) = planner_counts(&empty, &d);
        assert_eq!((pc, pu, pd), (14, 0, 0));
    }

    #[test]
    fn zset_matches_planner_on_prod_plan_11_updates() {
        let d = desired(&[input("env", "prod")]);
        let w = root().join("examples/world");
        let world = load_world(&w).unwrap();
        let ds = deformation(&d, &world, &crate::schema::fake());
        let s = summary(&ds);
        println!("prod plan vs world: {:?}", s.iter().map(|(k, v)| (format!("{k:?}"), v.clone())).collect::<Vec<_>>());
        for x in &ds {
            if x.kind != Kind::Undeformed {
                println!("  {:?} {}.{}  weights {:?} unresolved {:?}", x.kind, x.addr.typ, x.addr.name, x.weights.iter().map(|(w, _)| *w).collect::<Vec<_>>(), x.unresolved);
            }
        }
        let (pc, pu, pd, planner_updated) = planner_counts(&w, &d);
        assert_eq!((pc, pu, pd), (0, 11, 0));
        let zset_updated: BTreeSet<String> = s.get(&Kind::Update).cloned().unwrap_or_default().into_iter().collect();
        assert_eq!(zset_updated, planner_updated);
        assert_eq!(s.get(&Kind::Undeformed).map(|v| v.len()).unwrap_or(0), 3);
        assert!(s.get(&Kind::Pending).is_none(), "round-0 resolution leaves no null on a steady-state stack");
    }

    /// Without round-0 resolution every update whose desired doc carries a
    /// fresh null against a world constant would be a spurious update (UNA
    /// says fresh != constant). This pins why Rule 4's first clause matters.
    #[test]
    fn without_round0_resolution_fresh_nulls_make_spurious_updates() {
        let d = desired(&[]);
        let w = root().join("examples/world");
        let mut world = load_world(&w).unwrap();
        world.computed.clear();
        let ds = deformation(&d, &world, &crate::schema::fake());
        let s = summary(&ds);
        println!("no round-0: {:?}", s.iter().map(|(k, v)| (format!("{k:?}"), v.len())).collect::<Vec<_>>());
        assert!(s.get(&Kind::Update).map(|v| v.len()).unwrap_or(0) >= 8);
    }
}
