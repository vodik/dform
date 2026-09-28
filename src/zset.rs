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
//!   mapping, and `assemble` has dropped schema-computed paths
//!   (`ir::compile_resources`); an `ignore_changes` path is dropped from both
//!   sides of an object that exists (the provider's plan). A steady-state
//!   stack therefore carries no nulls and cancels to the zero Z-set.
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

use crate::ast::{Atom, Term};
use crate::ir::Address;
use crate::lattice::{Truth, eq3, nulls_in};
use crate::schema::{ReplaceOrder, Schema};
use crate::value::{NullClass, Value};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// The lifecycle facts, plain facts the planner reads (E §2.8: filters and
/// policy over the deformation; §3.4 for `moved`):
///
///   lifecycle(T, A, prevent_destroy).        a delete or replace of T/A is a deny
///   lifecycle(T, A, create_before_destroy).  a replacement is created first
///                                            (where the schema's type_replace
///                                            allows either order)
///   moved(T, Old, New).                      state's identity for Old is New's
///   ignore_changes(T, A, Path).              Path is dropped from both sides
///                                            once T/A exists; a create sets it
#[derive(Debug, Clone, Default)]
pub struct Lifecycle {
    pub prevent_destroy: BTreeSet<Address>,
    pub create_before_destroy: BTreeSet<Address>,
    /// (old, new), applied to state before the diff.
    pub moved: Vec<(Address, Address)>,
    pub ignore_changes: BTreeMap<Address, Vec<String>>,
}

impl Lifecycle {
    /// The lifecycle facts, checked against the schema: a
    /// `create_before_destroy` on a `type_replace(T, destroy_first)` type is
    /// an error naming the type.
    pub fn from_facts<'a>(
        facts: impl IntoIterator<Item = &'a Atom>,
        schema: &Schema,
    ) -> Result<Lifecycle> {
        let mut out = Lifecycle::default();
        for f in facts {
            if !matches!(f.pred.as_str(), "lifecycle" | "moved" | "ignore_changes") {
                continue;
            }
            let strs: Option<Vec<&str>> = f
                .args
                .iter()
                .map(|t| match t {
                    Term::Val(Value::Str(s)) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            let Some([typ, a, b]) = strs.as_deref() else {
                bail!("{}/3 expects three symbols or strings, got {f:?}", f.pred);
            };
            let addr = |name: &str| Address {
                typ: typ.to_string(),
                name: name.to_string(),
            };
            match (f.pred.as_str(), *b) {
                ("lifecycle", "prevent_destroy") => {
                    out.prevent_destroy.insert(addr(a));
                }
                ("lifecycle", "create_before_destroy") => {
                    if schema.replace_order(typ) == ReplaceOrder::DestroyFirst {
                        bail!(
                            "lifecycle({typ}, {a}, create_before_destroy): type {typ} is \
                             type_replace destroy_first; its old object must be deleted \
                             before the replacement is created"
                        );
                    }
                    out.create_before_destroy.insert(addr(a));
                }
                ("lifecycle", other) => bail!(
                    "lifecycle({typ}, {a}, {other}): unknown flag \
                     (expected prevent_destroy or create_before_destroy)"
                ),
                ("moved", _) => out.moved.push((addr(a), addr(b))),
                _ => out
                    .ignore_changes
                    .entry(addr(a))
                    .or_default()
                    .push(b.to_string()),
            }
        }
        Ok(out)
    }

    /// Whether a replacement of `addr` is created before the old object is
    /// deleted: the schema's `type_replace` decides, and for a type that
    /// allows either order, `create_before_destroy` (else destroy first).
    pub fn create_first(&self, schema: &Schema, addr: &Address) -> bool {
        match schema.replace_order(&addr.typ) {
            ReplaceOrder::CreateFirst => true,
            ReplaceOrder::DestroyFirst => false,
            ReplaceOrder::Either => self.create_before_destroy.contains(addr),
        }
    }

    /// `prevent_destroy` as a deny over the plan: every delete or replace
    /// of a protected address.
    pub fn denies(&self, actions: &[crate::provider::Action]) -> Vec<String> {
        use crate::provider::ActionKind;
        actions
            .iter()
            .filter(|a| self.prevent_destroy.contains(&a.addr))
            .filter_map(|a| {
                let what = match a.kind {
                    ActionKind::Delete => "delete",
                    ActionKind::Replace { .. } => "replace",
                    _ => return None,
                };
                Some(format!(
                    "lifecycle prevent_destroy: the plan would {what} {}.{}",
                    a.addr.typ, a.addr.name
                ))
            })
            .collect()
    }
}

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

/// The plan file (`plan --out PLAN.json`, `apply PLAN.json`; E §2.8):
/// the inputs, a digest of the world the plan was taken against, and the
/// deformation delta with the nulls it resolved and the ones it still
/// carries, each deformation with the tick it runs in.
///
/// Terraform's stale-plan rule, stated for Z-sets: `apply PLAN` refreshes
/// and re-evaluates, and refuses unless the delta it computes is the
/// file's. Every deformation now must be in the file with the same action,
/// the same before-state and the same desired values (a null the file
/// carries matches the value it has resolved to since); every deformation
/// in the file that has not run yet must still be one. A new address is
/// allowed only where the file has a pending group of its type. Values are
/// stored redacted, as the plan prints them, so a sensitive value is
/// compared by presence only.
pub mod file {
    use crate::ast::{Atom, Term};
    use crate::plan_print::{self, Report};
    use crate::provider::{Action, ActionKind, Plan};
    use crate::query::Redactor;
    use crate::schema::Schema;
    use crate::stuck::Sections;
    use crate::value::Value;
    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use serde_json::Value as Json;
    use std::collections::BTreeMap;
    use std::path::Path;

    pub const VERSION: u32 = 1;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PlanFile {
        pub version: u32,
        pub stack: String,
        pub inputs: Inputs,
        /// FNV-1a over the refreshed world facts: what the plan saw.
        pub world_digest: String,
        pub deformations: Vec<Entry>,
        pub pending_groups: Vec<Group>,
        pub nulls: Nulls,
        pub ticks: Vec<Tick>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Inputs {
        pub files: Vec<FileDigest>,
        pub set: Vec<String>,
        pub data: Vec<String>,
        pub providers: Vec<String>,
        pub world: Option<String>,
        pub inventory: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct FileDigest {
        pub path: String,
        pub fnv64: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Entry {
        #[serde(rename = "type")]
        pub typ: String,
        pub name: String,
        pub action: String,
        /// The tick it runs in; `None` when the plan cannot schedule it.
        pub tick: Option<usize>,
        /// The nulls it is held on, empty when definite.
        pub on: Vec<String>,
        pub changes: Vec<Leaf>,
        /// A replace: the resources whose documents reference this one.
        /// The replacement's new identity updates them a tick later.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub dependents: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Leaf {
        pub path: String,
        pub before: Json,
        pub after: Json,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Group {
        pub pattern: String,
        pub on: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Nulls {
        /// Resolved in round 0 from the world: label and value.
        pub resolved: Vec<Resolved>,
        /// Carried by the delta, filled at apply.
        pub unresolved: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Resolved {
        pub null: String,
        pub value: Json,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Tick {
        pub tick: usize,
        pub addresses: Vec<String>,
    }

    pub fn fnv64(bytes: &[u8]) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    pub fn world_digest(world_facts: &[Atom]) -> String {
        let mut lines: Vec<String> = world_facts.iter().map(crate::partition::fmt_atom).collect();
        lines.sort();
        fnv64(lines.join("\n").as_bytes())
    }

    fn action_name(k: &ActionKind) -> &'static str {
        match k {
            ActionKind::Create => "create",
            ActionKind::Adopt => "adopt",
            // A pending update is an update whose comparison waits.
            ActionKind::Update | ActionKind::Pending => "update",
            ActionKind::Drift => "drift",
            ActionKind::Delete => "delete",
            ActionKind::Replace {
                create_first: false,
            } => "replace",
            ActionKind::Replace { create_first: true } => "replace_create_first",
            ActionKind::DeleteDeposed => "delete_deposed",
            ActionKind::Noop => "no-op",
        }
    }

    /// The delta of one plan: every deformation, definite or held, with
    /// its tick from the report's schedule. Paths are the provider's own
    /// (a keyless set element by content), values redacted.
    pub fn delta(
        plan: &Plan,
        sections: &Sections,
        report: &Report,
        schema: &Schema,
        r: &Redactor,
    ) -> Vec<Entry> {
        let mut tick_of: BTreeMap<&str, usize> = BTreeMap::new();
        for (t, xs) in &report.ticks {
            for x in xs {
                tick_of.insert(x, *t);
            }
        }
        plan.actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop))
            .map(|a| entry(a, sections, &tick_of, schema, r))
            .collect()
    }

    fn entry(
        a: &Action,
        sections: &Sections,
        tick_of: &BTreeMap<&str, usize>,
        schema: &Schema,
        r: &Redactor,
    ) -> Entry {
        let name = format!("{}.{}", a.addr.typ, a.addr.name);
        Entry {
            typ: a.addr.typ.clone(),
            name: a.addr.name.clone(),
            action: action_name(&a.kind).into(),
            tick: tick_of.get(name.as_str()).copied(),
            on: plan_print::waits_on(a, sections).unwrap_or_default(),
            changes: a
                .changes
                .iter()
                .map(|c| Leaf {
                    path: c.path.clone(),
                    before: plan_print::shown(c.before.as_ref(), c.sensitive, schema, r).json(),
                    after: plan_print::shown(c.after.as_ref(), c.sensitive, schema, r).json(),
                })
                .collect(),
            dependents: vec![],
        }
    }

    /// Round 0's resolutions, from the `resolve/2` facts, redacted.
    pub fn resolved(facts: &std::collections::BTreeSet<Atom>, r: &Redactor) -> Vec<Resolved> {
        facts
            .iter()
            .filter(|a| a.pred == "resolve")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(l)), Term::Val(v)] => Some(Resolved {
                    null: l.clone(),
                    value: plan_print::shown_value(v, r).json(),
                }),
                _ => None,
            })
            .collect()
    }

    impl PlanFile {
        pub fn load(path: &Path) -> Result<PlanFile> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read plan file {}", path.display()))?;
            let f: PlanFile = serde_json::from_str(&text)
                .with_context(|| format!("parse plan file {}", path.display()))?;
            if f.version != VERSION {
                anyhow::bail!(
                    "plan file {}: version {} (this dform writes {VERSION})",
                    path.display(),
                    f.version
                );
            }
            Ok(f)
        }

        /// What differs between the inputs the file records and `now`.
        pub fn input_differences(&self, now: &Inputs) -> Vec<String> {
            let was = &self.inputs;
            let mut out = Vec::new();
            let files = |i: &Inputs| -> BTreeMap<String, String> {
                i.files
                    .iter()
                    .map(|f| (f.path.clone(), f.fnv64.clone()))
                    .collect()
            };
            let (a, b) = (files(was), files(now));
            for (p, h) in &a {
                match b.get(p) {
                    None => out.push(format!("--file {p}: in the plan file, not given now")),
                    Some(h2) if h2 != h => out.push(format!("--file {p}: changed since the plan")),
                    _ => {}
                }
            }
            for p in b.keys().filter(|p| !a.contains_key(*p)) {
                out.push(format!("--file {p}: given now, not in the plan file"));
            }
            let mut flag = |name: &str, x: String, y: String| {
                if x != y {
                    out.push(format!("--{name}: the plan file has [{x}], now [{y}]"));
                }
            };
            flag("set", was.set.join(" "), now.set.join(" "));
            flag("data", was.data.join(" "), now.data.join(" "));
            flag("provider", was.providers.join(" "), now.providers.join(" "));
            let opt = |o: &Option<String>| o.clone().unwrap_or_default();
            flag("world", opt(&was.world), opt(&now.world));
            flag("inventory", opt(&was.inventory), opt(&now.inventory));
            out
        }

        pub fn save(&self, path: &Path) -> Result<()> {
            std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
                .with_context(|| format!("write plan file {}", path.display()))
        }

        /// The differences between this file's delta and `current`, the
        /// delta re-evaluated at the start of `tick`; empty when the file's
        /// delta is reproduced.
        pub fn stale(&self, current: &[Entry], tick: usize) -> Vec<String> {
            let key = |e: &Entry| (e.typ.clone(), e.name.clone());
            let saved: BTreeMap<(String, String), &Entry> =
                self.deformations.iter().map(|e| (key(e), e)).collect();
            let now: BTreeMap<(String, String), &Entry> =
                current.iter().map(|e| (key(e), e)).collect();
            let mut out = Vec::new();
            for (k, c) in &now {
                let at = format!("{}.{}", k.0, k.1);
                // The object a create_before_destroy replacement deposed
                // is deleted the tick after.
                let deposed = c.action == "delete_deposed"
                    && saved
                        .get(k)
                        .is_some_and(|s| s.action == "replace_create_first");
                if deposed {
                    continue;
                }
                let Some(s) = saved.get(k) else {
                    let grouped = self
                        .pending_groups
                        .iter()
                        .any(|g| g.pattern == format!("{}.?", k.0));
                    // A dependent of an earlier replace follows its new
                    // identity.
                    let follows = c.action == "update"
                        && self.deformations.iter().any(|e| {
                            e.tick.is_some_and(|t| t < tick) && e.dependents.contains(&at)
                        });
                    if !grouped && !follows {
                        out.push(format!("{} {at}: not in the plan file", c.action));
                    }
                    continue;
                };
                if s.tick.is_some_and(|t| t < tick) {
                    out.push(format!(
                        "{} {at}: deformed again at tick {tick}; the plan file ran it in tick {}",
                        c.action,
                        s.tick.unwrap_or_default()
                    ));
                    continue;
                }
                if s.action != c.action {
                    out.push(format!(
                        "{at}: the plan file has {}, re-evaluation has {}",
                        s.action, c.action
                    ));
                    continue;
                }
                out.extend(leaf_differences(&at, s, c));
            }
            for (k, s) in &saved {
                // Deformations of earlier ticks have run.
                if s.tick.is_some_and(|t| t < tick) || now.contains_key(k) {
                    continue;
                }
                out.push(format!(
                    "{} {}.{}: in the plan file, no longer a deformation",
                    s.action, k.0, k.1
                ));
            }
            out
        }
    }

    fn is_null(v: &Json) -> bool {
        matches!(v, Json::Object(m) if m.len() == 2 && m.contains_key("null") && m.contains_key("class"))
    }

    fn leaf_differences(at: &str, saved: &Entry, now: &Entry) -> Vec<String> {
        let s: BTreeMap<&str, &Leaf> = saved.changes.iter().map(|l| (l.path.as_str(), l)).collect();
        let n: BTreeMap<&str, &Leaf> = now.changes.iter().map(|l| (l.path.as_str(), l)).collect();
        let text = |v: &Json| serde_json::to_string(v).unwrap_or_default();
        let mut out = Vec::new();
        for (p, l) in &n {
            match s.get(p) {
                None => out.push(format!(
                    "{at} {p}: not in the plan file ({} -> {})",
                    text(&l.before),
                    text(&l.after)
                )),
                Some(sl) => {
                    if sl.before != l.before {
                        out.push(format!(
                            "{at} {p}: the plan saw {}, the world now has {}",
                            text(&sl.before),
                            text(&l.before)
                        ));
                    }
                    // A null the file carries matches what it resolved to.
                    if sl.after != l.after && !is_null(&sl.after) {
                        out.push(format!(
                            "{at} {p}: the plan file sets {}, re-evaluation sets {}",
                            text(&sl.after),
                            text(&l.after)
                        ));
                    }
                }
            }
        }
        for (p, sl) in &s {
            if !n.contains_key(p) {
                out.push(format!(
                    "{at} {p}: in the plan file ({} -> {}), no longer a change",
                    text(&sl.before),
                    text(&sl.after)
                ));
            }
        }
        out
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
            .plan(&desired, &[], &Lifecycle::default(), &st)
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

    /// F 4.3: on the fixture, env=prod is eleven updates (six of them
    /// replacements, the fake schema's cidrs being force_new) and three
    /// undeformed, the same eleven addresses the planner reported before the
    /// Z-set replaced its diff; nothing is pending (round 0).
    #[test]
    fn fixture_prod_is_eleven_updates() {
        let dir = scratch("prod");
        let (world, state) = fixture(&dir, |_| {});
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        let p = plan(&program, &[input("env", "prod")], &world, &state);
        // Six of them change a force_new cidr: replacements.
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Update)), 5);
        assert_eq!(count(&p, |k| matches!(k, ActionKind::Replace { .. })), 6);
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
