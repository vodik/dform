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
///                                            (derived by `POLICY_RULES`)
///   lifecycle(T, A, create_before_destroy).  a replacement is created first
///                                            (where the schema's type_replace
///                                            allows either order)
///   moved(T, Old, New).                      state's identity for Old is New's
///   ignore_changes(T, A, Path).              Path is dropped from both sides
///                                            once T/A exists; a create sets it
///
/// And the refinements the engine does not check (F DR-13 revised): a
/// `type_refine(T, Path, C)` on a path the schema marks `sensitive`, for
/// every `attr(T, A, P, V)` whose value reaches `Path`, is an Apply
/// assertion on `T/A` the provider checks after materializing the secret.
#[derive(Debug, Clone, Default)]
pub struct Lifecycle {
    pub create_before_destroy: BTreeSet<Address>,
    /// (old, new), applied to state before the diff.
    pub moved: Vec<(Address, Address)>,
    pub ignore_changes: BTreeMap<Address, Vec<String>>,
    /// (path, refinement) per address, for its Apply `assertions`.
    pub assertions: BTreeMap<Address, Vec<(String, crate::lattice::Constraint)>>,
}

impl Lifecycle {
    /// The lifecycle facts, checked against the schema: a
    /// `create_before_destroy` on a `type_replace(T, destroy_first)` type is
    /// an error naming the type.
    pub fn from_facts<'a>(
        facts: impl IntoIterator<Item = &'a Atom>,
        schema: &Schema,
    ) -> Result<Lifecycle> {
        let facts: Vec<&Atom> = facts.into_iter().collect();
        let mut out = Lifecycle {
            assertions: provider_assertions(&facts, schema)?,
            ..Lifecycle::default()
        };
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
                // A deny the evaluator derives (`POLICY_RULES`).
                ("lifecycle", "prevent_destroy") => {}
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
}

/// The Apply assertions of `Lifecycle::assertions`.
fn provider_assertions(
    facts: &[&Atom],
    schema: &Schema,
) -> Result<BTreeMap<Address, Vec<(String, crate::lattice::Constraint)>>> {
    let mut refs = Vec::new();
    for f in facts
        .iter()
        .filter(|f| f.pred == crate::refine::TYPE_REFINE)
    {
        let r = crate::refine::Stated::of(f)
            .expect("a type_refine fact")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if schema.is_sensitive(&r.typ, &r.path) {
            refs.push(r);
        }
    }
    let mut out: BTreeMap<Address, Vec<(String, crate::lattice::Constraint)>> = BTreeMap::new();
    if refs.is_empty() {
        return Ok(out);
    }
    for f in facts.iter().filter(|f| f.pred == "attr") {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(a)),
            Term::Val(Value::Str(p)),
            Term::Val(v),
        ] = f.args.as_slice()
        else {
            continue;
        };
        let addr = Value::Str(a.clone());
        for r in refs.iter().filter(|r| r.applies(t, &addr, p)) {
            let rest = r.path[p.len()..].trim_start_matches('.');
            let reaches = rest.is_empty()
                || rest
                    .split('.')
                    .try_fold(v, |v, k| match v {
                        Value::Obj(m) => m.get(k),
                        _ => None,
                    })
                    .is_some();
            if reaches {
                out.entry(Address {
                    typ: t.clone(),
                    name: a.clone(),
                })
                .or_default()
                .push((r.path.clone(), r.constraint.clone()));
            }
        }
    }
    Ok(out)
}

/// Policy over the plan (E §2.8: policy reads the deformation). The planner
/// hands the deformation back to the evaluator as facts for a second pass
/// (`deformation_facts`):
///
///   deformation(Kind, T, A, Before)  one per deformation: Kind is create,
///                                    adopt, update, drift, pending, replace,
///                                    delete, delete_deposed or remaining;
///                                    Before the digest of the world
///                                    document it was planned against
///                                    (`absent` for none)
///   world_digest(T, A, Now)          the world document's digest now
///
/// and these rules, appended to the program, derive the lifecycle denies
/// from them, so `why` explains them and a policy can read the same facts.
/// At a phase boundary the held deformations come back as `pending` with
/// the digest they were planned against, against the refreshed world; on
/// resuming an interrupted apply, its remaining ones come back as
/// `remaining`.
pub const POLICY_RULES: &str = r#"
deny(m) if {
  lifecycle(t, a, "prevent_destroy"), deformation("delete", t, a, _)
  m = format("lifecycle prevent_destroy: the plan would delete %s[\"%s\"]", t, a)
}
deny(m) if {
  lifecycle(t, a, "prevent_destroy"), deformation("replace", t, a, _)
  m = format("lifecycle prevent_destroy: the plan would replace %s[\"%s\"]", t, a)
}
deny(m) if {
  deformation("pending", t, a, before), world_digest(t, a, now), before != now
  m = format("the world changed under a pending deformation: %s[\"%s\"]", t, a)
}
deny(m) if {
  deformation("remaining", t, a, before), world_digest(t, a, now), before != now
  m = format("the world changed under a remaining action: %s[\"%s\"]", t, a)
}
"#;

/// The predicates a policy pass gives the program (`deformation_facts`).
pub const POLICY_INPUTS: &[&str] = &["deformation", "world_digest", crate::stuck::MAY_DERIVE];

/// The program with `POLICY_RULES` appended: what every evaluation runs.
pub fn with_policy_rules(mut program: crate::ast::Program) -> Result<crate::ast::Program> {
    let rules = crate::parser::parse_program(POLICY_RULES)?;
    program.statements.extend(rules.statements);
    Ok(program)
}

/// A world document's digest for `deformation/4` and `world_digest/3`.
pub fn doc_digest(doc: Option<&serde_json::Value>) -> String {
    match doc {
        Some(d) => file::fnv64(&serde_json::to_vec(d).unwrap_or_default()),
        None => "absent".to_string(),
    }
}

/// `deformation/4`'s kind for an action; `held` when it waits on a
/// boundary.
pub fn deformation_kind(k: &crate::provider::ActionKind, held: bool) -> Option<&'static str> {
    use crate::provider::ActionKind;
    Some(match k {
        ActionKind::Noop => return None,
        _ if held => "pending",
        ActionKind::Create => "create",
        ActionKind::Adopt => "adopt",
        ActionKind::Update => "update",
        ActionKind::Drift => "drift",
        ActionKind::Pending => "pending",
        ActionKind::Replace { .. } => "replace",
        ActionKind::Delete => "delete",
        ActionKind::DeleteDeposed => "delete_deposed",
    })
}

/// `deformation(Kind, T, A, Before)` for each deformation, with its
/// `world_digest(T, A, Now)`. `before` is the document each was planned
/// against, `now` the world as it is (at plan time the same).
pub fn deformation_facts<'a>(
    deformations: impl IntoIterator<Item = (&'static str, &'a Address)>,
    before: &BTreeMap<Address, Option<serde_json::Value>>,
    now: &BTreeMap<Address, serde_json::Value>,
) -> Vec<Atom> {
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (kind, addr) in deformations {
        out.push(Atom {
            pred: "deformation".into(),
            args: vec![
                s(kind),
                s(&addr.typ),
                s(&addr.name),
                s(&doc_digest(before.get(addr).and_then(Option::as_ref))),
            ],
            record: None,
            span: Default::default(),
        });
        if seen.insert(addr) {
            out.push(Atom {
                pred: "world_digest".into(),
                args: vec![s(&addr.typ), s(&addr.name), s(&doc_digest(now.get(addr)))],
                record: None,
                span: Default::default(),
            });
        }
    }
    out
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
/// stored redacted, as the plan prints them; a sensitive value, and a
/// secret input's `--set` value, as `{"sensitive": label, "digest": HMAC}`,
/// keyed by the stack's own key ([`file::Key`]): a secret that changed
/// between plan and apply is a difference, and the file never carries its
/// bytes.
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

    pub const VERSION: u32 = 3;

    /// The stack's plan-file key: 32 random bytes in `state.key` beside
    /// the stack's state (it moves with the state on a handover), made on
    /// first use, readable by its owner only. It never leaves the state dir.
    pub struct Key([u8; 32]);

    impl Key {
        /// The key of the deployment whose objects are `store`'s, when it
        /// has one.
        pub fn load(store: &dyn crate::store::Store) -> Result<Option<Key>> {
            use crate::store::KEY;
            let Some(o) = store.get(KEY)? else {
                return Ok(None);
            };
            let key: [u8; 32] = o
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("plan key {}: not 32 bytes", store.locate(KEY)))?;
            Ok(Some(Key(key)))
        }

        /// The key of the deployment whose objects are `store`'s.
        pub fn load_or_create(store: &dyn crate::store::Store) -> Result<Key> {
            use crate::store::{Cond, KEY};
            let parse = |bytes: &[u8]| -> Result<Key> {
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("plan key {}: not 32 bytes", store.locate(KEY)))?;
                Ok(Key(key))
            };
            if let Some(o) = store.get(KEY)? {
                return parse(&o.bytes);
            }
            let mut key = [0u8; 32];
            {
                use std::io::Read;
                std::fs::File::open("/dev/urandom")
                    .and_then(|mut f| f.read_exact(&mut key))
                    .context("read /dev/urandom for the plan key")?;
            }
            if store
                .put(KEY, &key, &Cond::IfAbsent)
                .with_context(|| format!("write plan key {}", store.locate(KEY)))?
                .is_none()
            {
                // Made by another run meanwhile: that one is the key.
                let o = store
                    .get(KEY)?
                    .ok_or_else(|| anyhow::anyhow!("plan key {}: gone", store.locate(KEY)))?;
                return parse(&o.bytes);
            }
            Ok(Key(key))
        }

        /// A key derived from this one for `what`: its HMAC, so the derived
        /// key says nothing of this one. What a provider is given to digest
        /// a secret it holds (`Config::digest_key`).
        pub fn derive(&self, what: &str) -> Key {
            let hex = self.digest(what.as_bytes());
            Key::from_hex(&hex).expect("a digest is 32 bytes of hex")
        }

        /// The key's bytes, hex.
        pub fn to_hex(&self) -> String {
            self.0.iter().map(|b| format!("{b:02x}")).collect()
        }

        /// A key from 64 hex digits.
        pub fn from_hex(hex: &str) -> Option<Key> {
            if hex.len() != 64 || !hex.is_ascii() {
                return None;
            }
            let mut k = [0u8; 32];
            for (i, b) in k.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
            }
            Some(Key(k))
        }

        /// HMAC-SHA256 (RFC 2104) of `bytes`, hex.
        pub fn digest(&self, bytes: &[u8]) -> String {
            use sha2::{Digest, Sha256};
            let pad = |b: u8| -> Vec<u8> {
                let mut k = [0u8; 64];
                k[..32].copy_from_slice(&self.0);
                k.iter().map(|x| x ^ b).collect()
            };
            let inner = Sha256::new()
                .chain_update(pad(0x36))
                .chain_update(bytes)
                .finalize();
            Sha256::new()
                .chain_update(pad(0x5c))
                .chain_update(inner)
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }

        /// A redacted value as the file stores it: a sensitive one with the
        /// digest of `bytes`, anything else as shown.
        fn stored(&self, shown: plan_print::Shown, bytes: impl FnOnce() -> Vec<u8>) -> Json {
            match shown {
                plan_print::Shown::Sensitive(l) => {
                    serde_json::json!({ "sensitive": l, "digest": self.digest(&bytes()) })
                }
                s => s.json(),
            }
        }
    }

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
        /// The extern answers the plan read: apply asks none of these again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub externs: Vec<crate::externs::Answer>,
        /// The commit each `git` input relation's ref named (but the
        /// `approval` relation's, which carries tokens, not intent): apply
        /// refuses when one moved.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub git_commits: Vec<Pinned>,
        /// The policy pass's `requires_approval(D, Reason)` rows.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub needs_approval: Vec<NeedsApproval>,
        /// [`PlanFile::digest`], as written: what an approval signs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub digest: Option<String>,
    }

    /// A `git` input relation's source and the commit its ref named.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Pinned {
        pub source: String,
        pub commit: String,
    }

    /// A deformation that needs an approval, and why (`requires_approval`).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct NeedsApproval {
        pub deformation: String,
        pub reason: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Inputs {
        pub files: Vec<FileDigest>,
        /// `--input-file`s: stack inputs as facts.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub input_files: Vec<KeyedDigest>,
        /// `--set k=v`, a secret input's as `{"sensitive": label, "digest"}`.
        pub set: Vec<Json>,
        pub data: Vec<String>,
        pub providers: Vec<String>,
        pub world: Option<String>,
        pub inventory: Option<String>,
        /// The environment variables the program read (`env_var`), each
        /// `{"sensitive": "env_var/NAME", "digest"}` with its value's
        /// digest keyed with the plan key: never the value.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub env: Vec<Json>,
        /// Other stacks' published outputs the program reads
        /// (`stack_output`), each deployment with the digest of its
        /// outputs object as read (`absent` when it had none).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub stack_outputs: Vec<OutputsDigest>,
    }

    /// A deployment's published outputs as a plan read them.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct OutputsDigest {
        pub deployment: String,
        pub digest: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct FileDigest {
        pub path: String,
        pub fnv64: String,
    }

    /// A file that may hold a secret (an `--input-file`): its digest is
    /// keyed with the stack's plan key ([`Key::digest`]), as a sensitive
    /// leaf's is, so the file does not let its bytes be guessed.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct KeyedDigest {
        pub path: String,
        pub digest: String,
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
        key: &Key,
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
            .map(|a| entry(a, sections, &tick_of, schema, r, key))
            .collect()
    }

    fn entry(
        a: &Action,
        sections: &Sections,
        tick_of: &BTreeMap<&str, usize>,
        schema: &Schema,
        r: &Redactor,
        key: &Key,
    ) -> Entry {
        let side = |v: Option<&Json>, sensitive: bool| {
            key.stored(plan_print::shown(v, sensitive, schema, r), || {
                serde_json::to_vec(&v).unwrap_or_default()
            })
        };
        let name = a.addr.to_string();
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
                    before: side(c.before.as_ref(), c.sensitive),
                    after: side(c.after.as_ref(), c.sensitive),
                })
                .collect(),
            dependents: vec![],
        }
    }

    /// Round 0's resolutions, from the `resolve/2` facts, redacted.
    pub fn resolved(
        facts: &std::collections::BTreeSet<Atom>,
        r: &Redactor,
        key: &Key,
    ) -> Vec<Resolved> {
        facts
            .iter()
            .filter(|a| a.pred == "resolve")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(l)), Term::Val(v)] => Some(Resolved {
                    null: l.clone(),
                    value: key.stored(plan_print::shown_value(v, r), || {
                        serde_json::to_vec(&crate::engine::value_to_json(v)).unwrap_or_default()
                    }),
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
            let digests = |fs: &[FileDigest]| -> BTreeMap<String, String> {
                fs.iter()
                    .map(|f| (f.path.clone(), f.fnv64.clone()))
                    .collect()
            };
            let keyed = |fs: &[KeyedDigest]| -> BTreeMap<String, String> {
                fs.iter()
                    .map(|f| (f.path.clone(), f.digest.clone()))
                    .collect()
            };
            for (what, a, b) in [
                ("program", digests(&was.files), digests(&now.files)),
                (
                    "--input-file",
                    keyed(&was.input_files),
                    keyed(&now.input_files),
                ),
            ] {
                for (p, h) in &a {
                    match b.get(p) {
                        None => out.push(format!("{what} {p}: in the plan file, not given now")),
                        Some(h2) if h2 != h => {
                            out.push(format!("{what} {p}: changed since the plan"))
                        }
                        _ => {}
                    }
                }
                for p in b.keys().filter(|p| !a.contains_key(*p)) {
                    out.push(format!("{what} {p}: given now, not in the plan file"));
                }
            }
            let mut flag = |name: &str, x: String, y: String| {
                if x != y {
                    out.push(format!("--{name}: the plan file has [{x}], now [{y}]"));
                }
            };
            let set = |xs: &[Json]| -> String {
                xs.iter()
                    .map(|x| match x {
                        Json::String(s) => s.clone(),
                        x => x.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            flag("set", set(&was.set), set(&now.set));
            flag("data", was.data.join(" "), now.data.join(" "));
            flag("provider", was.providers.join(" "), now.providers.join(" "));
            let opt = |o: &Option<String>| o.clone().unwrap_or_default();
            flag("world", opt(&was.world), opt(&now.world));
            flag("inventory", opt(&was.inventory), opt(&now.inventory));
            // An `env_var` the plan read, by its label and keyed digest.
            let env = |xs: &[Json]| -> BTreeMap<String, Json> {
                xs.iter()
                    .filter_map(|x| {
                        Some((
                            x.get("sensitive")?.as_str()?.to_string(),
                            x["digest"].clone(),
                        ))
                    })
                    .collect()
            };
            let now_env = env(&now.env);
            for (label, digest) in env(&was.env) {
                match now_env.get(&label) {
                    None => out.push(format!("{label}: in the plan file, not set now")),
                    Some(d) if *d != digest => out.push(format!("{label}: changed since the plan")),
                    _ => {}
                }
            }
            let outputs = |xs: &[OutputsDigest]| -> BTreeMap<String, String> {
                xs.iter()
                    .map(|o| (o.deployment.clone(), o.digest.clone()))
                    .collect()
            };
            let now_outputs = outputs(&now.stack_outputs);
            for (d, digest) in outputs(&was.stack_outputs) {
                match now_outputs.get(&d) {
                    None => out.push(format!(
                        "stack_output of {d}: the plan read its outputs, this run does not"
                    )),
                    Some(n) if *n != digest => out.push(format!(
                        "stack_output of {d}: its published outputs changed since the plan \
                         ({digest} -> {n})"
                    )),
                    _ => {}
                }
            }
            out
        }

        /// The plan digest: sha256 over the canonical JSON (sorted keys,
        /// no whitespace) of the file without its `digest`: the delta, the
        /// inputs, the pinned git commits and the extern answers, with
        /// secrets as the stack's HMAC of them. `sha256:HEX`.
        pub fn digest(&self) -> String {
            let mut v = serde_json::to_value(self).unwrap_or_default();
            if let Json::Object(m) = &mut v {
                m.remove("digest");
            }
            crate::approval::digest_of(&v)
        }

        /// The pinned git commits that moved since the plan.
        pub fn commit_differences(&self, now: &[Pinned]) -> Vec<String> {
            let now: BTreeMap<&str, &str> = now
                .iter()
                .map(|p| (p.source.as_str(), p.commit.as_str()))
                .collect();
            self.git_commits
                .iter()
                .filter_map(|p| match now.get(p.source.as_str()) {
                    Some(c) if *c == p.commit => None,
                    Some(c) => Some(format!(
                        "input relation {}: the plan read commit {}, the ref names {c} now",
                        p.source, p.commit
                    )),
                    None => Some(format!(
                        "input relation {}: in the plan file, not read now",
                        p.source
                    )),
                })
                .collect()
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
                let at = crate::ir::Address {
                    typ: k.0.clone(),
                    name: k.1.clone(),
                }
                .to_string();
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
                        .any(|g| g.pattern == format!("{}[?]", k.0));
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
                let at = crate::ir::Address {
                    typ: k.0.clone(),
                    name: k.1.clone(),
                };
                out.push(format!(
                    "{} {at}: in the plan file, no longer a deformation",
                    s.action
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
        // A sensitive value by its label and the head of its digest.
        let text = |v: &Json| match (v.get("sensitive"), v.get("digest").and_then(Json::as_str)) {
            (Some(l), Some(d)) => {
                let d = &d[..d.len().min(8)];
                match l.as_str() {
                    Some(l) => format!("(sensitive {l}, digest {d})"),
                    None => format!("(sensitive, digest {d})"),
                }
            }
            _ => serde_json::to_string(v).unwrap_or_default(),
        };
        // The leaf `p` of the address `at`: `T["A"].p`.
        let leaf = |p: &str| format!("{at}{}", crate::ir::path_suffix(p));
        let mut out = Vec::new();
        for (p, l) in &n {
            let at = leaf(p);
            match s.get(p) {
                None => out.push(format!(
                    "{at}: not in the plan file ({} -> {})",
                    text(&l.before),
                    text(&l.after)
                )),
                Some(sl) => {
                    if sl.before != l.before {
                        out.push(format!(
                            "{at}: the plan saw {}, the world now has {}",
                            text(&sl.before),
                            text(&l.before)
                        ));
                    }
                    // A null the file carries matches what it resolved to.
                    if sl.after != l.after && !is_null(&sl.after) {
                        out.push(format!(
                            "{at}: the plan file sets {}, re-evaluation sets {}",
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
                    "{}: in the plan file ({} -> {}), no longer a change",
                    leaf(p),
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
}
