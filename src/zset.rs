//! The plan as a Z-set (proposal E §2.8, DR-11 as F revises it).
//!
//!   desired(T, A, Doc) :- want(T, A), Doc = assemble(T, A)
//!   world(T, A, Doc)   :- identity(T, A, Rid), world_doc(T, Rid, Doc)
//!   deformation        = desired − world          (Z-set over (T, A, Doc))
//!
//! Per address the group is {+1} create, {−1} delete, {+1, −1} update, empty
//! undeformed. Equality is `eq3`, leaf by leaf over the provider's canonical
//! (flattened) documents:
//!
//! * `desired` is defined after round 0: the evaluator has already replaced
//!   every null whose resource exists in the world through the identity
//!   mapping, and `assemble` has dropped schema-computed and
//!   `ignore_changes` paths (`ir::compile_resources`). A steady-state stack
//!   therefore carries no nulls and cancels to the zero Z-set.
//! * An update whose desired document carries an *open* null against a world
//!   constant is pending: the comparison is a content position, decided at
//!   the next boundary.
//! * A *fresh* null against a world constant after round 0 means the
//!   identity mapping is stale (the resource that owns it is gone from the
//!   world): drift, not an ordinary update.
//! * A create whose document carries a null is still a create: the executor
//!   fills it in dependency order.
//!
//! The provider's per-resource `Plan` (the fake provider's diff) turns each
//! deformation into an action and decides replace.

use crate::ir::Address;
use crate::lattice::{Truth, eq3, nulls_in};
use crate::value::{NullClass, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Create,
    Delete,
    Update,
    /// An update against a stale identity: a fresh null where the world has
    /// a constant.
    Drift,
    /// An update that cannot be decided until a null resolves.
    Pending,
    Undeformed,
}

#[derive(Debug, Clone)]
pub struct Deformation {
    pub addr: Address,
    pub kind: Kind,
    /// The Z-set group after summation: `(+1, desired)`, `(-1, world)`.
    pub weights: Vec<(i32, Value)>,
    /// Pending: the nulls the comparison waits on. Otherwise the nulls the
    /// desired document carries for the executor to fill.
    pub unresolved: BTreeSet<String>,
}

/// desired − world. Documents are flat objects, path to leaf.
pub fn deformation(
    desired: &BTreeMap<Address, Value>,
    world: &BTreeMap<Address, Value>,
) -> Vec<Deformation> {
    let addrs: BTreeSet<&Address> = desired.keys().chain(world.keys()).collect();
    let mut out = Vec::new();
    for addr in addrs {
        let mut weights = Vec::new();
        if let Some(d) = desired.get(addr) {
            weights.push((1, d.clone()));
        }
        if let Some(w) = world.get(addr) {
            weights.push((-1, w.clone()));
        }
        let (kind, unresolved) = match (desired.get(addr), world.get(addr)) {
            (Some(d), None) => (Kind::Create, nulls_in(d)),
            (None, Some(_)) => (Kind::Delete, BTreeSet::new()),
            (Some(d), Some(w)) => compare(d, w),
            (None, None) => unreachable!(),
        };
        if kind == Kind::Undeformed {
            // Equal documents cancel.
            weights.clear();
        }
        out.push(Deformation {
            addr: addr.clone(),
            kind,
            weights,
            unresolved,
        });
    }
    out
}

/// One address present on both sides, leaf by leaf.
fn compare(desired: &Value, world: &Value) -> (Kind, BTreeSet<String>) {
    let (Value::Obj(d), Value::Obj(w)) = (desired, world) else {
        return match eq3(desired, world) {
            Truth::True => (Kind::Undeformed, BTreeSet::new()),
            Truth::Unknown => (Kind::Pending, nulls_in(desired)),
            Truth::False => (Kind::Update, nulls_in(desired)),
        };
    };
    let paths: BTreeSet<&String> = d.keys().chain(w.keys()).collect();
    let mut changed = false;
    let mut unknown = BTreeSet::new();
    let mut stale = false;
    for p in paths {
        let (Some(dv), Some(wv)) = (d.get(p), w.get(p)) else {
            changed = true;
            continue;
        };
        match eq3(dv, wv) {
            Truth::True => {}
            Truth::Unknown => {
                unknown.extend(nulls_in(dv));
                unknown.extend(nulls_in(wv));
            }
            Truth::False => {
                changed = true;
                if matches!(
                    dv,
                    Value::Null {
                        class: NullClass::Fresh,
                        ..
                    }
                ) && nulls_in(wv).is_empty()
                {
                    stale = true;
                }
            }
        }
    }
    if !unknown.is_empty() {
        (Kind::Pending, unknown)
    } else if stale {
        (Kind::Drift, nulls_in(desired))
    } else if changed {
        (Kind::Update, nulls_in(desired))
    } else {
        (Kind::Undeformed, BTreeSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: &str) -> Address {
        Address {
            typ: "t".into(),
            name: n.into(),
        }
    }
    fn doc(kv: &[(&str, Value)]) -> Value {
        Value::Obj(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }
    fn null(label: &str, class: NullClass) -> Value {
        Value::Null {
            label: label.into(),
            class,
            ty: "string".into(),
        }
    }
    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn groups_by_address() {
        let desired = BTreeMap::from([
            (addr("same"), doc(&[("a", s("1"))])),
            (
                addr("new"),
                doc(&[("vpc", null("v/x#id", NullClass::Fresh))]),
            ),
            (addr("changed"), doc(&[("a", s("2"))])),
            (
                addr("open"),
                doc(&[("ep", null("db/x#endpoint", NullClass::Open))]),
            ),
            (
                addr("stale"),
                doc(&[("vpc", null("v/gone#id", NullClass::Fresh))]),
            ),
        ]);
        let world = BTreeMap::from([
            (addr("same"), doc(&[("a", s("1"))])),
            (addr("changed"), doc(&[("a", s("1"))])),
            (addr("open"), doc(&[("ep", s("db.fake"))])),
            (addr("stale"), doc(&[("vpc", s("vpc-1"))])),
            (addr("gone"), doc(&[("a", s("1"))])),
        ]);
        let got: BTreeMap<String, (Kind, BTreeSet<String>)> = deformation(&desired, &world)
            .into_iter()
            .map(|d| (d.addr.name, (d.kind, d.unresolved)))
            .collect();
        let set = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(got["same"], (Kind::Undeformed, set(&[])));
        assert_eq!(got["new"], (Kind::Create, set(&["v/x#id"])));
        assert_eq!(got["changed"], (Kind::Update, set(&[])));
        assert_eq!(got["open"], (Kind::Pending, set(&["db/x#endpoint"])));
        assert_eq!(got["stale"], (Kind::Drift, set(&["v/gone#id"])));
        assert_eq!(got["gone"], (Kind::Delete, set(&[])));
    }

    use crate::fakecloud::FakeCloud;
    use crate::provider::{ActionKind, Provider};
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn input(k: &str, v: &str) -> crate::ast::Atom {
        crate::ast::Atom {
            pred: "input".into(),
            args: vec![
                crate::ast::Term::Val(Value::Str(k.into())),
                crate::ast::Term::Val(Value::Str(v.into())),
            ],
            record: None,
        }
    }

    /// Plan `program` against a world file and its state, the way the CLI
    /// does: refresh facts feed round 0, then the Z-set.
    fn plan(
        program: &crate::ast::Program,
        extra: &[crate::ast::Atom],
        world: &Path,
        state: &Path,
    ) -> Vec<(String, ActionKind, BTreeSet<String>)> {
        let schema = crate::schema::fake();
        let backend = FakeCloud::with_paths(world, world.with_extension("inv"), schema.clone());
        let mut st = crate::state::State::load(state).unwrap();
        backend.bootstrap_state(&mut st).unwrap();
        let mut extra = extra.to_vec();
        extra.extend(schema.facts.clone());
        extra.extend(backend.world_facts(&st).unwrap());
        let (res, violations) = crate::engine::eval(program, &extra).unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        let desired = crate::ir::compile_resources(res.facts.iter().cloned(), &schema).unwrap();
        backend
            .plan(&desired, &[], &st)
            .unwrap()
            .actions
            .into_iter()
            .map(|a| (format!("{}.{}", a.addr.typ, a.addr.name), a.kind, a.on))
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dform-zset-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fixture(dir: &Path, edit: impl Fn(&mut serde_json::Value)) -> (PathBuf, PathBuf) {
        let mut w: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root().join("examples/world/dform.json")).unwrap(),
        )
        .unwrap();
        edit(&mut w);
        let world = dir.join("dform.json");
        std::fs::write(&world, serde_json::to_string(&w).unwrap()).unwrap();
        let state = dir.join("dform.state.json");
        std::fs::copy(root().join("examples/world/dform.state.json"), &state).unwrap();
        (world, state)
    }

    fn count(p: &[(String, ActionKind, BTreeSet<String>)], k: fn(&ActionKind) -> bool) -> usize {
        p.iter().filter(|x| k(&x.1)).count()
    }

    /// F 4.3: on the fixture, env=prod is eleven updates and three
    /// undeformed, the same eleven addresses the planner reported before the
    /// Z-set replaced its diff; nothing is pending (round 0).
    #[test]
    fn fixture_prod_is_eleven_updates() {
        let dir = scratch("prod");
        let (world, state) = fixture(&dir, |_| {});
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        let p = plan(&program, &[input("env", "prod")], &world, &state);
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Update)), 11);
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Noop)), 3);
        assert_eq!(p.len(), 14);
        let noop: BTreeSet<&str> = p
            .iter()
            .filter(|x| matches!(x.1, ActionKind::Noop))
            .map(|x| x.0.as_str())
            .collect();
        assert_eq!(
            noop,
            BTreeSet::from([
                "iam.role.iam.main::app_role",
                "iam.role_policy_attachment.iam.main::attach",
                "net.vpc_peering.peer-main-peer",
            ])
        );
        let p = plan(&program, &[], &world, &state);
        assert!(p.iter().all(|x| matches!(x.1, ActionKind::Noop)), "{p:?}");
    }

    /// F6: without the world's computed values round 0 resolves nothing, and
    /// a fresh null against the world's constant is a stale identity: drift,
    /// not ten spurious updates.
    #[test]
    fn a_fresh_null_against_a_world_constant_is_drift() {
        let dir = scratch("drift");
        let (world, state) = fixture(&dir, |w| {
            for r in w["resources"].as_object_mut().unwrap().values_mut() {
                r["computed"] = serde_json::json!({});
            }
        });
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        let p = plan(&program, &[], &world, &state);
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Drift)), 10, "{p:?}");
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Update)), 0, "{p:?}");
    }

    /// E §2.8: an update whose desired document carries an open null against
    /// a world constant is pending on that null.
    #[test]
    fn an_open_null_against_a_world_constant_is_pending() {
        let dir = scratch("open");
        let world = dir.join("w.json");
        std::fs::write(
            &world,
            serde_json::json!({"resources": {"compute.vm::app": {
                "typ": "compute.vm", "name": "app",
                "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}})
            .to_string(),
        )
        .unwrap();
        let program = crate::parser::parse_program(
            "resource db.postgres main { size = 1 }.
             resource compute.vm app { db_host = ref(db.postgres, main, endpoint) }.",
        )
        .unwrap();
        let p = plan(&program, &[], &world, &dir.join("w.state.json"));
        let app = p.iter().find(|x| x.0 == "compute.vm.app").unwrap();
        assert!(matches!(app.1, ActionKind::Pending), "{p:?}");
        assert_eq!(
            app.2,
            BTreeSet::from(["db.postgres/main#endpoint".to_string()])
        );
    }
}
