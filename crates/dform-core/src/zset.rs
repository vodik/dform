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
/// policy over the deformation; §3.4 for `moved`). `r` is a resource
/// reference (R-42): a resource in scope by its name, `T["A"]`, or a
/// variable a rule binds with `in`:
///
///   lifecycle(r, prevent_destroy).        a delete or replace of r is a deny
///                                         (derived by `POLICY_RULES`)
///   lifecycle(r, create_before_destroy).  a replacement is created first
///                                         (where the schema's type_replace
///                                         allows either order)
///   moved(T, Old, r).                     state's identity for the address
///                                         Old (text: it no longer exists)
///                                         is r's
///   ignore_changes(r, Path).              Path is dropped from both sides
///                                         once r exists; a create sets it
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

/// The program's copies (R-67): a component is a resource type the
/// program defines, and a copy an instance of it, addressed as a resource
/// is, `PATH["scope"]` (`network.vpc["blue"]`, `network.vpc["edge.left"]`
/// for a copy inside another). Each by its scope as an address writes it,
/// with its component's path: the `instance_of` facts of an evaluation,
/// and what state remembers of copies whose resources it still holds
/// (`state::State::instances`), so a removed copy's deletes are its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Instances {
    pub by_scope: BTreeMap<String, String>,
    /// The scopes the program still wants a resource in.
    live: BTreeSet<String>,
}

impl Instances {
    pub fn from_facts<'a>(facts: impl IntoIterator<Item = &'a Atom>) -> Instances {
        let mut by_scope = BTreeMap::new();
        let mut live = BTreeSet::new();
        for a in facts {
            if let ("want", [_, Term::Val(Value::Str(name))]) = (a.pred.as_str(), a.args.as_slice())
            {
                let mut name = name.as_str();
                while let Some((scope, _)) = crate::ir::scope_split(name) {
                    live.insert(scope.to_string());
                    name = scope;
                }
            }
            if a.pred != crate::modules::INSTANCE_OF {
                continue;
            }
            let [
                Term::Val(Value::Str(path)),
                Term::Val(Value::Str(user)),
                Term::Val(Value::Str(name)),
            ] = a.args.as_slice()
            else {
                continue;
            };
            let scope = match user.is_empty() {
                true => name.clone(),
                false => format!("{user}.{name}"),
            };
            by_scope.insert(scope, path.clone());
        }
        Instances { by_scope, live }
    }

    /// The kind of the copy `inst`'s own row, from its resources'
    /// deformations (`deformation_kind`'s names): `delete` when the program
    /// wants none of its resources any more, `create` when one is created
    /// (or adopted), else `update`.
    pub fn row_kind(&self, inst: &Address, kinds: &[&str]) -> &'static str {
        if !self.live.contains(&inst.name) {
            "delete"
        } else if kinds.iter().any(|k| matches!(*k, "create" | "adopt")) {
            "create"
        } else {
            "update"
        }
    }

    /// These, and the copies `kept` remembers that these do not name.
    pub fn with(mut self, kept: &BTreeMap<String, String>) -> Instances {
        for (scope, path) in kept {
            self.by_scope
                .entry(scope.clone())
                .or_insert_with(|| path.clone());
        }
        self
    }

    /// The copy `scope` as an address, `PATH["scope"]`.
    pub fn address(&self, scope: &str) -> Option<Address> {
        Some(Address {
            typ: self.by_scope.get(scope)?.clone(),
            name: scope.to_string(),
        })
    }

    /// Is `addr` a copy's address?
    pub fn is_instance(&self, addr: &Address) -> bool {
        self.by_scope.get(&addr.name) == Some(&addr.typ)
    }

    /// The copies a resource is in, innermost first: `edge.left.vpc` is in
    /// `edge.left` and `edge`.
    pub fn enclosing(&self, addr: &Address) -> Vec<Address> {
        let mut out = Vec::new();
        let mut name = addr.name.as_str();
        while let Some((scope, _)) = crate::ir::scope_split(name) {
            out.extend(self.address(scope));
            name = scope;
        }
        out
    }

    /// The resources of `wants` in the copy `inst`, its copies' included.
    pub fn members<'a>(
        &self,
        inst: &Address,
        wants: impl IntoIterator<Item = &'a Address>,
    ) -> Vec<Address> {
        wants
            .into_iter()
            .filter(|a| self.enclosing(a).contains(inst))
            .cloned()
            .collect()
    }

    /// What state keeps after an apply: the copies of these with a
    /// resource in `resources` (state's addresses).
    pub fn kept<'a>(
        &self,
        resources: impl IntoIterator<Item = &'a Address>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for a in resources {
            for i in self.enclosing(a) {
                out.insert(i.name, i.typ);
            }
        }
        out
    }
}

/// A copy's deformation rows and its resources' membership, for the policy
/// pass (R-67): `deformation(Kind, i, "absent")` for each copy `i` a
/// deformation of one of its resources is in, of `Instances::row_kind`;
/// and `in_instance(r, i)` for each such resource `r`, which
/// carries a copy's `lifecycle` to its resources (`POLICY_RULES`). Held
/// and remaining deformations make no row: they came back from a plan.
pub fn instance_facts<'a>(
    deformations: impl IntoIterator<Item = (&'static str, &'a Address)>,
    instances: &Instances,
) -> Vec<Atom> {
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let r = |a: &Address| Term::Val(reference(a));
    let mut kinds: BTreeMap<Address, Vec<&str>> = BTreeMap::new();
    let mut out = Vec::new();
    for (kind, addr) in deformations {
        if !matches!(
            kind,
            "create" | "adopt" | "update" | "drift" | "replace" | "delete"
        ) {
            continue;
        }
        for i in instances.enclosing(addr) {
            out.push(Atom {
                pred: IN_INSTANCE.into(),
                args: vec![r(addr), r(&i)],
                record: None,
                span: Default::default(),
            });
            kinds.entry(i).or_default().push(kind);
        }
    }
    for (i, ks) in kinds {
        let kind = instances.row_kind(&i, &ks);
        out.push(Atom {
            pred: "deformation".into(),
            args: vec![s(kind), r(&i), s("absent")],
            record: None,
            span: Default::default(),
        });
    }
    out
}

/// `in_instance(r, i)`: the resource `r` a deformation is of is in the
/// copy `i` (`instance_facts`).
pub const IN_INSTANCE: &str = "in_instance";

/// The column of a plan or lifecycle relation that holds a resource
/// reference (R-42), by its arity: the resolver lowers a resource there
/// as a reference value, never its address text.
pub fn ref_column(pred: &str, arity: usize) -> Option<usize> {
    match (pred, arity) {
        ("deformation", 3) => Some(1),
        ("moved", 3) => Some(2),
        ("world_digest" | "requires_approval" | "lifecycle" | "adopt" | "ignore_changes", 2) => {
            Some(0)
        }
        _ => None,
    }
}

/// The arity of each relation `ref_column` knows, with its shape for the
/// error a wrong arity gets.
pub const REF_RELATIONS: &[(&str, usize, &str)] = &[
    ("deformation", 3, "deformation(kind, resource, before)"),
    ("world_digest", 2, "world_digest(resource, now)"),
    (
        "requires_approval",
        2,
        "requires_approval(resource, reason)",
    ),
    ("lifecycle", 2, "lifecycle(resource, \"prevent_destroy\")"),
    ("adopt", 2, "adopt(resource, \"remote-name\")"),
    ("ignore_changes", 2, "ignore_changes(resource, \"path\")"),
    ("moved", 3, "moved(T, \"old-address\", resource)"),
];

/// A head's term that names a resource's address, a content position
/// (Rule 2): `want(T, A)`'s and `arg(T, A, ..)`'s `A`, `adopt(r, _)`'s
/// reference.
pub fn address_arg(head: &Atom) -> Option<&Term> {
    match head.pred.as_str() {
        "want" | "arg" if head.args.len() >= 2 => Some(&head.args[1]),
        "adopt" => head.args.first(),
        _ => None,
    }
}

/// A reference value's address: `T["A"]` with no attribute path.
pub fn referenced(t: &Term) -> Option<Address> {
    match t {
        Term::Val(Value::Ref { typ, name, attr }) if attr.is_empty() => Some(Address {
            typ: typ.clone(),
            name: name.clone(),
        }),
        _ => None,
    }
}

/// A reference value for `addr`.
pub fn reference(addr: &Address) -> Value {
    Value::Ref {
        typ: addr.typ.clone(),
        name: addr.name.clone(),
        attr: String::new(),
    }
}

impl Lifecycle {
    /// The lifecycle facts, checked against the schema: a
    /// `create_before_destroy` on a `type_replace(T, destroy_first)` type is
    /// an error naming the type. A flag on a copy (R-67) is on each of its
    /// resources.
    pub fn from_facts<'a>(
        facts: impl IntoIterator<Item = &'a Atom>,
        schema: &Schema,
    ) -> Result<Lifecycle> {
        let facts: Vec<&Atom> = facts.into_iter().collect();
        let mut out = Lifecycle {
            assertions: provider_assertions(&facts, schema)?,
            ..Lifecycle::default()
        };
        let text = |t: &Term| match t {
            Term::Val(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let instances = Instances::from_facts(facts.iter().copied());
        let wants: Vec<Address> = facts
            .iter()
            .filter_map(|f| match (f.pred.as_str(), f.args.as_slice()) {
                ("want", [t, a]) => Some(Address {
                    typ: text(t)?,
                    name: text(a)?,
                }),
                _ => None,
            })
            .collect();
        // A copy's resources, or the resource itself.
        let each = |addr: Address| match instances.is_instance(&addr) {
            true => instances.members(&addr, &wants),
            false => vec![addr],
        };
        for f in facts {
            match (f.pred.as_str(), f.args.as_slice()) {
                ("lifecycle", [r, what]) => {
                    let (Some(addr), Some(what)) = (referenced(r), text(what)) else {
                        bail!("lifecycle expects a resource and a flag, got {f:?}");
                    };
                    match what.as_str() {
                        // A deny the evaluator derives (`POLICY_RULES`).
                        "prevent_destroy" => {}
                        "create_before_destroy" => {
                            // On a copy: each of its resources whose type
                            // allows it.
                            let copy = instances.is_instance(&addr);
                            for addr in each(addr) {
                                let first = schema.replace_order(&addr.typ);
                                if copy && first == ReplaceOrder::DestroyFirst {
                                    continue;
                                }
                                if first == ReplaceOrder::DestroyFirst {
                                    bail!(
                                        "lifecycle({addr}, create_before_destroy): type {} is \
                                         type_replace destroy_first; its old object must be \
                                         deleted before the replacement is created",
                                        addr.typ
                                    );
                                }
                                out.create_before_destroy.insert(addr);
                            }
                        }
                        other => bail!(
                            "lifecycle({addr}, {other}): unknown flag \
                             (expected prevent_destroy or create_before_destroy)"
                        ),
                    }
                }
                ("moved", [typ, old, new]) => {
                    let (Some(typ), Some(old), Some(new)) = (text(typ), text(old), referenced(new))
                    else {
                        bail!("moved expects a type, the old address and a resource, got {f:?}");
                    };
                    if new.typ != typ {
                        bail!("moved({typ}, {old:?}, {new}): {new} is not a {typ}");
                    }
                    out.moved.push((Address { typ, name: old }, new));
                }
                ("ignore_changes", [r, path]) => {
                    let (Some(addr), Some(path)) = (referenced(r), text(path)) else {
                        bail!("ignore_changes expects a resource and a path, got {f:?}");
                    };
                    for addr in each(addr) {
                        out.ignore_changes
                            .entry(addr)
                            .or_default()
                            .push(path.clone());
                    }
                }
                _ => {}
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
/// (`deformation_facts`), the resource as a reference (R-42):
///
///   deformation(Kind, r, Before)  one per deformation: Kind is create,
///                                 adopt, update, drift, pending, replace,
///                                 delete, delete_deposed or remaining;
///                                 Before the digest of the world document
///                                 it was planned against (`absent` for
///                                 none)
///   world_digest(r, Now)          the world document's digest now
///   in_instance(r, i)             r is a resource of the copy i, which
///                                 has a deformation row of its own
///                                 (`instance_facts`, R-67)
///
/// and these rules, appended to the program, derive the lifecycle denies
/// from them, so `why` explains them and a policy can read the same facts.
/// At a phase boundary the held deformations come back as `pending` with
/// the digest they were planned against, against the refreshed world; on
/// resuming an interrupted apply, its remaining ones come back as
/// `remaining`.
pub const POLICY_RULES: &str = r#"
#| the lifecycle rule prevent_destroy, against a delete
deny(m) where {
  lifecycle(r, "prevent_destroy"), deformation("delete", r, _)
  m = "lifecycle prevent_destroy: the plan would delete ${r}"
}
#| the lifecycle rule prevent_destroy, against a replace
deny(m) where {
  lifecycle(r, "prevent_destroy"), deformation("replace", r, _)
  m = "lifecycle prevent_destroy: the plan would replace ${r}"
}
#| the lifecycle rule prevent_destroy on a copy, against a delete of one of its resources
deny(m) where {
  lifecycle(i, "prevent_destroy"), in_instance(r, i), deformation("delete", r, _)
  m = "lifecycle prevent_destroy on ${i}: the plan would delete ${r}"
}
#| the lifecycle rule prevent_destroy on a copy, against a replace of one of its resources
deny(m) where {
  lifecycle(i, "prevent_destroy"), in_instance(r, i), deformation("replace", r, _)
  m = "lifecycle prevent_destroy on ${i}: the plan would replace ${r}"
}
#| the world rule: a held deformation's resource moved since its plan
deny(m) where {
  deformation("pending", r, before), world_digest(r, now), before != now
  m = "the world changed under a pending change: ${r}"
}
#| the world rule: an interrupted apply's resource moved since its plan
deny(m) where {
  deformation("remaining", r, before), world_digest(r, now), before != now
  m = "the world changed under a remaining action: ${r}"
}
"#;

/// The predicates a policy pass gives the program (`deformation_facts`).
pub const POLICY_INPUTS: &[&str] = &[
    "deformation",
    DERIVED_AT_LAST_APPLY,
    "world_digest",
    IN_INSTANCE,
    crate::stuck::MAY_DERIVE,
];

/// The program with `POLICY_RULES` appended: what every evaluation runs.
/// Their doc comments name them where `why` prints them; they are not the
/// program's docs.
pub fn with_policy_rules(mut program: crate::ast::Program) -> Result<crate::ast::Program> {
    let rules = crate::parser::parse_program(POLICY_RULES)?;
    program.statements.extend(
        rules
            .statements
            .into_iter()
            .filter(|s| !matches!(s, crate::ast::Stmt::Fact(a) if a.pred == "doc")),
    );
    Ok(program)
}

/// A world document's digest for `deformation/3` and `world_digest/2`.
pub fn doc_digest(doc: Option<&serde_json::Value>) -> String {
    match doc {
        Some(d) => file::fnv64(&serde_json::to_vec(d).unwrap_or_default()),
        None => "absent".to_string(),
    }
}

/// `deformation/3`'s kind for an action; `held` when it waits on a
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

/// `deformation(Kind, r, Before)` for each deformation, with its
/// `world_digest(r, Now)`. `before` is the document each was planned
/// against, `now` the world as it is (at plan time the same).
pub fn deformation_facts<'a>(
    deformations: impl IntoIterator<Item = (&'static str, &'a Address)>,
    before: &BTreeMap<Address, Option<serde_json::Value>>,
    now: &BTreeMap<Address, serde_json::Value>,
) -> Vec<Atom> {
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let r = |a: &Address| Term::Val(reference(a));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (kind, addr) in deformations {
        out.push(Atom {
            pred: "deformation".into(),
            args: vec![
                s(kind),
                r(addr),
                s(&doc_digest(before.get(addr).and_then(Option::as_ref))),
            ],
            record: None,
            span: Default::default(),
        });
        if seen.insert(addr) {
            out.push(Atom {
                pred: "world_digest".into(),
                args: vec![r(addr), s(&doc_digest(now.get(addr)))],
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

// --- the emptied-relation guardrail (R-80) -------------------------------

/// The policy pass's record of the last apply (R-80): one row per rule
/// that derived resources then, by where it is written (`FILE:LINE`),
/// with how many; one per relation of the program, by its name, with its
/// rows. A deny may read it beside `deformation/3`.
pub const DERIVED_AT_LAST_APPLY: &str = "derived_at_last_apply";

/// What an apply derived (R-80), as the audit log's `derived` entry
/// keeps it: each rule with variables (a statement with a `where` that
/// binds something; a resource stated once is not one) by where it is
/// written, its statement and the resources it derived; each relation of
/// the program with its rows. A later plan that deletes every resource of
/// a rule, or empties a relation, says so ([`emptied`]).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Derived {
    pub rules: BTreeMap<String, DerivedRule>,
    pub rows: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DerivedRule {
    /// The statement on one line, as a site prints it.
    pub statement: String,
    /// The resources it derived, as the plan prints their addresses.
    pub addresses: Vec<String>,
}

impl Derived {
    /// What `res` derives: each `want`'s site (relative to `top`, the
    /// project's root, when it is under it), and the rows of each of
    /// `relations`.
    pub fn of(
        res: &crate::engine::EvalResult,
        redact: &crate::query::Redactor,
        relations: &BTreeSet<String>,
        top: Option<&std::path::Path>,
    ) -> Derived {
        let p = crate::report::tree::Printer {
            circuit: &res.circuit,
            redact,
            all: false,
        };
        let mut out = Derived::default();
        for f in res.facts.iter().filter(|f| f.pred == "want") {
            let [Term::Val(Value::Str(t)), Term::Val(Value::Str(a))] = f.args.as_slice() else {
                continue;
            };
            let addr = Address {
                typ: t.clone(),
                name: a.clone(),
            };
            let Some(site) = p.want_site(&res.rules, &addr) else {
                continue;
            };
            if site.with.is_empty() || site.at.is_empty() {
                continue;
            }
            let at = top
                .and_then(|top| relative_place(top, &site.at))
                .unwrap_or(site.at);
            let r = out.rules.entry(at).or_default();
            r.statement = site.statement;
            r.addresses.push(addr.to_string());
        }
        for rel in relations {
            let n = res.facts.iter().filter(|f| &f.pred == rel).count();
            out.rows.insert(rel.clone(), n);
        }
        out
    }

    /// The record of the last apply in the audit log `entries` that ended
    /// well; `None` when there is none, or it kept no record.
    pub fn last(entries: &[serde_json::Value]) -> Option<Derived> {
        let (mut last, mut open) = (None, None);
        for e in entries {
            match e["kind"].as_str() {
                Some("apply_start") => open = Some(None),
                Some("derived") => {
                    if let Some(o) = open.as_mut() {
                        *o = serde_json::from_value::<Derived>(e["record"].clone()).ok();
                    }
                }
                Some("apply_end") => {
                    if let Some(d) = open.take()
                        && e["result"] == "ok"
                    {
                        last = d;
                    }
                }
                _ => {}
            }
        }
        last
    }

    /// `derived_at_last_apply(rule, n)`: each rule by its place with the
    /// resources it derived, each relation by its name with its rows.
    pub fn facts(&self) -> Vec<Atom> {
        let row = |name: &str, n: usize| Atom {
            pred: DERIVED_AT_LAST_APPLY.into(),
            args: vec![
                Term::Val(Value::Str(name.to_string())),
                Term::Val(Value::Int(n as i64)),
            ],
            record: None,
            span: Default::default(),
        };
        self.rules
            .iter()
            .map(|(at, r)| row(at, r.addresses.len()))
            .chain(self.rows.iter().map(|(rel, n)| row(rel, *n)))
            .collect()
    }
}

/// `FILE:LINE` relative to `top` when the file is under it.
pub fn relative_place(top: &std::path::Path, at: &str) -> Option<String> {
    let prefix = format!("{}/", top.display());
    let (file, line) = at.rsplit_once(':')?;
    if file.starts_with('<') {
        return None;
    }
    let abs = std::path::absolute(file).ok()?;
    let rest = abs.to_str()?.strip_prefix(&prefix)?;
    Some(format!("{rest}:{line}"))
}

/// A resource statement whose own clause holds and that derives no
/// resource (R-120, `transform::not_planned_report`): the plan lists it
/// with why, on one line, rather than leaving it out.
#[derive(Debug, Clone, PartialEq)]
pub struct NotPlanned {
    pub addr: Address,
    /// The deepest condition `why-not` names (`input one.namespace is not
    /// set`).
    pub reason: String,
    /// Its report's rule (`r12`), whose place is the statement's.
    pub rule: Option<String>,
    /// Where the statement is written ([`crate::report::Report::explain`]).
    pub site: Option<crate::report::tree::Site>,
}

/// The statements `res` derives no resource of, each with why.
pub fn not_planned(
    res: &crate::engine::EvalResult,
    redact: &crate::query::Redactor,
) -> Vec<NotPlanned> {
    let s = |t: &Term| match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    res.facts
        .iter()
        .filter(|f| f.pred == crate::transform::NOT_PLANNED)
        .filter_map(|f| {
            let [t, a] = f.args.as_slice() else {
                return None;
            };
            let addr = Address {
                typ: s(t)?,
                name: s(a)?,
            };
            // Its statement's report: the one that names it, else one
            // whose name a variable binds.
            let report = |exact: bool| {
                res.rules.iter().position(|r| {
                    r.head.pred == f.pred
                        && r.head.args.iter().zip(&f.args).all(|(h, v)| match h {
                            Term::Var(_) => !exact,
                            // A copy's own resource, `scoped(n, name)`.
                            Term::Func { name, args } if name == "scoped" => {
                                match (args.as_slice(), v) {
                                    (
                                        [Term::Val(Value::Str(n)), Term::Val(Value::Str(a))],
                                        Term::Val(Value::Str(v)),
                                    ) => crate::ir::scoped(n, a) == *v,
                                    _ => !exact,
                                }
                            }
                            h => h == v,
                        })
                })
            };
            let rule = report(true)
                .or_else(|| report(false))
                .map(|i| format!("r{i}"));
            let reason = crate::whynot::reason(&addr.typ, &addr.name, res, redact)
                .unwrap_or_else(|| "no rule derives it".into());
            Some(NotPlanned {
                addr,
                reason,
                rule,
                site: None,
            })
        })
        .collect()
}

/// A rule the plan deletes every resource of, or a relation it empties,
/// since the last apply (R-80): what the plan's `warning` section names,
/// and what `apply` asks for on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Emptied {
    /// A rule's place (`FILE:LINE`), or a relation's name.
    pub name: String,
    /// A rule's statement; `None` for a relation.
    pub statement: Option<String>,
    /// A rule's resources, every one deleted by the plan.
    pub deleted: Vec<String>,
    /// A relation's rows at the last apply.
    pub rows: usize,
    /// The leaf that changed since the last apply (R-79's `because`), of
    /// the first deleted resource that says.
    pub because: Option<String>,
}

impl Emptied {
    /// `--allow-empty NAME` (or `[stacks.NAME] allow_empty`) names it: its
    /// place, a resource type it deletes, or the relation.
    pub fn allowed(&self, names: &[String]) -> bool {
        names.iter().any(|n| {
            *n == self.name
                || *n == self.flag()
                || self
                    .deleted
                    .iter()
                    .any(|a| crate::ir::parse_resource_address(a).is_ok_and(|a| a.typ == *n))
        })
    }

    /// What `--allow-empty` names it by: its place, or the relation as
    /// the source reads it, a copy's own by its path (`green.vpc_net`).
    pub fn flag(&self) -> String {
        match &self.statement {
            Some(_) => self.name.clone(),
            None => self.name.replace("::", "."),
        }
    }

    /// The question `apply` asks of it, and the line it refuses with.
    pub fn what(&self) -> String {
        match &self.statement {
            Some(_) => format!(
                "the plan deletes all {} resources the rule at {} derived at the last apply",
                self.deleted.len(),
                self.name
            ),
            None => format!(
                "the plan empties the relation {}, which had {} at the last apply",
                crate::report::relation_name(&self.name),
                count_rows(self.rows)
            ),
        }
    }
}

fn count_rows(n: usize) -> String {
    match n {
        1 => "1 row".into(),
        n => format!("{n} rows"),
    }
}

/// What the plan empties since the last apply `then`: each rule all of
/// whose resources are in `deleted`, and each relation with rows then and
/// none now (`rows_now`). `because` is each address's leaf, when R-79
/// found one.
pub fn emptied(
    then: &Derived,
    deleted: &BTreeSet<String>,
    rows_now: &dyn Fn(&str) -> usize,
    because: &dyn Fn(&str) -> Option<String>,
) -> Vec<Emptied> {
    let mut out = Vec::new();
    for (at, r) in &then.rules {
        if r.addresses.is_empty() || !r.addresses.iter().all(|a| deleted.contains(a)) {
            continue;
        }
        out.push(Emptied {
            name: at.clone(),
            statement: Some(r.statement.clone()),
            deleted: r.addresses.clone(),
            rows: 0,
            because: r.addresses.iter().find_map(|a| because(a)),
        });
    }
    for (rel, n) in &then.rows {
        if *n > 0 && rows_now(rel) == 0 {
            out.push(Emptied {
                name: rel.clone(),
                statement: None,
                deleted: Vec::new(),
                rows: *n,
                because: None,
            });
        }
    }
    out
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
    use crate::engine::EvalResult;
    use crate::provider::{Action, ActionKind, Plan};
    use crate::query::Redactor;
    use crate::report::{self, Report};
    use crate::schema::Schema;
    use crate::stuck::Sections;
    use crate::value::Value;
    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use serde_json::Value as Json;
    use std::collections::BTreeMap;
    use std::path::Path;

    pub const VERSION: u32 = 4;

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
            use hmac::{Hmac, Mac};
            let mut mac = <Hmac<sha2::Sha256>>::new_from_slice(&self.0)
                .expect("HMAC takes a key of any length");
            mac.update(bytes);
            mac.finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }

        /// A redacted value as the file stores it: a sensitive one with the
        /// digest of `bytes`, anything else as shown.
        fn stored(&self, shown: report::Shown, bytes: impl FnOnce() -> Vec<u8>) -> Json {
            match shown {
                report::Shown::Sensitive(l) => {
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
        /// The policy pass's `requires_approval(D, Reason)` rows.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub needs_approval: Vec<NeedsApproval>,
        /// [`PlanFile::digest`], as written: what an approval signs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub digest: Option<String>,
        /// Every name the program declares more than once, each under a
        /// clause (R-104): a review sees a pair a refactor enabled.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub guarded: Vec<Guarded>,
    }

    /// A name declared more than once, each under a clause (R-104), as
    /// the plan file lists it: `{"name": "db", "declarations": 2}`; a
    /// copy's own as `copy.name`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Guarded {
        pub name: String,
        pub declarations: usize,
    }

    /// The guarded groups of an evaluation's rules: each name's
    /// `__declared(name, i)` rows (`modules::exclusive`), those of more
    /// than one declaration.
    pub fn guarded(res: &EvalResult) -> Vec<Guarded> {
        let mut by: BTreeMap<String, std::collections::BTreeSet<i64>> = BTreeMap::new();
        for r in res.rules.iter() {
            let (scope, pred) = match r.head.pred.rsplit_once("::") {
                Some((s, p)) => (Some(s), p),
                None => (None, r.head.pred.as_str()),
            };
            let (crate::modules::DECLARED, [Term::Val(Value::Str(n)), Term::Val(Value::Int(i))]) =
                (pred, r.head.args.as_slice())
            else {
                continue;
            };
            let name = match scope {
                Some(s) => format!("{s}.{n}"),
                None => n.clone(),
            };
            by.entry(name).or_default().insert(*i);
        }
        by.into_iter()
            .filter(|(_, is)| is.len() > 1)
            .map(|(name, is)| Guarded {
                name,
                declarations: is.len(),
            })
            .collect()
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
        /// `{"sensitive": "env.var/NAME", "digest"}` with its value's
        /// digest keyed with the plan key: never the value.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub env: Vec<Json>,
        /// The secret answers of dform's own externs the program read
        /// (`ssh.read`), each `{"sensitive": "PRED/INPUTS#N", "digest"}`
        /// keyed as `env`'s: never the bytes. Apply reads them again and
        /// refuses the plan when one changed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub answers: Vec<Json>,
        /// Other stacks' published outputs the program reads (a keyed
        /// read of a deployment), each deployment with the digest of its
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

    /// A pending group (a stuck resource rule, or one that may derive
    /// after a boundary): what the plan could not name yet, by its `head`,
    /// `rule` and bindings.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Group {
        pub pattern: String,
        pub on: Vec<String>,
        /// The head pattern, as `stuck/4` and `may_derive/3` print it.
        pub head: String,
        /// The rule, by its provenance id (`r{i}`).
        pub rule: String,
        /// The instance's bound, null-free variables, redacted; a
        /// may-derive group has none.
        pub bindings: BTreeMap<String, Json>,
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
            key.stored(report::shown(v, sensitive, schema, r), || {
                serde_json::to_vec(&v).unwrap_or_default()
            })
        };
        let name = a.addr.to_string();
        Entry {
            typ: a.addr.typ.clone(),
            name: a.addr.name.clone(),
            action: action_name(&a.kind).into(),
            tick: tick_of.get(name.as_str()).copied(),
            on: report::waits_on(a, sections).unwrap_or_default(),
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

    /// The pending groups of one evaluation: its stuck resource rules and
    /// those that may derive after a boundary, each with its head, rule
    /// and null-free bindings, redacted.
    pub fn groups(res: &EvalResult, r: &Redactor, key: &Key) -> Vec<Group> {
        let stuck = res
            .stuck
            .iter()
            .filter(|s| s.head.pred == "want")
            .filter_map(|s| Some((s.rule?, &s.head, &s.nulls, Some(&s.bindings))));
        let may = res
            .may_derive
            .iter()
            .filter(|m| m.head.pred == "want")
            .map(|m| (m.rule, &m.head, &m.nulls, None));
        let mut out: Vec<Group> = Vec::new();
        for (rule, head, nulls, bindings) in stuck.chain(may) {
            let g = Group {
                pattern: report::group_pattern(head),
                on: nulls.iter().cloned().collect(),
                head: crate::partition::fmt_atom(head),
                rule: format!("r{rule}"),
                bindings: bindings
                    .map(|b| redacted(b.iter(), r, key))
                    .unwrap_or_default(),
            };
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }

    /// Bindings as a group records them: the null-free ones, redacted.
    fn redacted<'a>(
        bindings: impl Iterator<Item = (&'a String, &'a Value)>,
        r: &Redactor,
        key: &Key,
    ) -> BTreeMap<String, Json> {
        bindings
            .filter(|(_, v)| !crate::stuck::has_null(v))
            .map(|(k, v)| {
                let j = key.stored(report::shown_value(v, r), || {
                    serde_json::to_vec(&crate::engine::value_to_json(v)).unwrap_or_default()
                });
                (k.clone(), j)
            })
            .collect()
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
                    value: key.stored(report::shown_value(v, r), || {
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
                        "the outputs of {d}: the plan read them, this run does not"
                    )),
                    Some(n) if *n != digest => out.push(format!(
                        "the outputs of {d}: published again with other values since the plan \
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

        pub fn save(&self, path: &Path) -> Result<()> {
            std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
                .with_context(|| format!("write plan file {}", path.display()))
        }

        /// The differences between this file's delta and `current`, the
        /// delta re-evaluated at the start of `tick`; empty when the file's
        /// delta is reproduced. An address the file does not list is stale
        /// at tick 1; at a later tick the stop rule stops before it.
        pub fn stale(&self, current: &[Entry], tick: usize) -> Vec<String> {
            let key = |e: &Entry| (e.typ.clone(), e.name.clone());
            let saved: BTreeMap<(String, String), &Entry> =
                self.deformations.iter().map(|e| (key(e), e)).collect();
            let now: BTreeMap<(String, String), &Entry> =
                current.iter().map(|e| (key(e), e)).collect();
            let mut out = Vec::new();
            for (k, c) in &now {
                let addr = crate::ir::Address {
                    typ: k.0.clone(),
                    name: k.1.clone(),
                };
                let at = report::address(&addr);
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
                    // An address the file does not list, at a later tick, is
                    // the stop rule's (R-30): an unattended apply stops
                    // before the tick that adds it, its state consistent.
                    if tick == 1 {
                        out.push(format!("{} {at}: not in the plan file", c.action));
                    }
                    continue;
                };
                if s.tick.is_some_and(|t| t < tick) {
                    out.push(format!(
                        "{} {at}: changed again at tick {tick}; the plan file ran it in tick {}",
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
                out.extend(leaf_differences(&addr, s, c));
            }
            for (k, s) in &saved {
                // Deformations of earlier ticks have run.
                if s.tick.is_some_and(|t| t < tick) || now.contains_key(k) {
                    continue;
                }
                let at = report::address(&crate::ir::Address {
                    typ: k.0.clone(),
                    name: k.1.clone(),
                });
                out.push(format!(
                    "{} {at}: in the plan file, no longer a change",
                    s.action
                ));
            }
            out
        }
    }

    fn is_null(v: &Json) -> bool {
        matches!(v, Json::Object(m) if m.len() == 2 && m.contains_key("null") && m.contains_key("class"))
    }

    fn leaf_differences(at: &crate::ir::Address, saved: &Entry, now: &Entry) -> Vec<String> {
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
        // The leaf `p` of the address `at` as a diagnostic names it (R-111).
        let leaf = |p: &str| report::attribute(at, p);
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

    /// The plan key's digest is HMAC-SHA256 (RFC 2104): the value Python's
    /// `hmac.new(bytes(range(32)), b"Hi There", hashlib.sha256)` gives.
    #[test]
    fn a_keys_digest_is_hmac_sha256() {
        let key = file::Key::from_hex(&(0u8..32).map(|b| format!("{b:02x}")).collect::<String>())
            .unwrap();
        assert_eq!(
            key.digest(b"Hi There"),
            "278639ec02309d3afded1b273f1349ba63b9089c12476d716bee3ecc94673e9e"
        );
    }
}
