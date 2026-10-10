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
//!   (`resources::compile_resources`); an `ignore_changes` path is dropped
//!   from both sides of an object that exists (the provider's plan). A
//!   steady-state stack therefore carries no nulls and cancels to the zero
//!   Z-set.
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

use crate::address::Address;
use crate::ast::{Atom, RuleStmt, Term};
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
///   lifecycle(r, create_first).           a replacement is created first
///                                         (where the schema's type_replace
///                                         allows either order, and where
///                                         the name is the identity, its
///                                         provider generates one, R-189)
///   lifecycle(r, retain).                 a delete of r is a forget: no
///                                         Delete call, state drops it and
///                                         the world keeps it (R-154); r
///                                         may be an address no rule wants
///   lifecycle(r, destroy).                a delete of r deletes it: the
///                                         default, written over a type's
///                                         `type_lifecycle`
///   moved(T, Old, r).                     state's identity for the address
///                                         Old (text: it no longer exists)
///                                         is r's
///   lifecycle(r, bootstrap, Path).        Path is given at creation only:
///                                         a create (and a replace) sends
///                                         it; once r exists a differing
///                                         value is kept, and said (R-198)
///   ignore_changes(r, Path).              Path is dropped from both sides
///                                         once r exists; a create sets it
///
/// And the refinements the engine does not check (F DR-13 revised): a
/// `type_refine(T, Path, C)` on a path the schema marks `sensitive`, for
/// every `attr(T, A, P, V)` whose value reaches `Path`, is an Apply
/// assertion on `T/A` the provider checks after materializing the secret.
#[derive(Debug, Clone, Default)]
pub struct Lifecycle {
    /// `lifecycle(r, "create_first")`: a replace of r creates first.
    pub created_first: BTreeSet<Address>,
    /// `lifecycle(r, "retain")`: a delete of r forgets it (R-154).
    pub retain: BTreeSet<Address>,
    /// The objects a row says what their removal means ([`REMOVAL`]):
    /// their type's `type_lifecycle` is not theirs.
    pub said: BTreeSet<Address>,
    /// type -> what removal means for it (`type_lifecycle`), for an
    /// object no row is about: one no rule wants any more.
    pub seeded: BTreeMap<String, String>,
    /// (old, new), applied to state before the diff.
    pub moved: Vec<(Address, Address)>,
    /// The attributes given at creation only, by address and path: once
    /// the object exists the path is dropped from both sides (R-198).
    pub at_create: BTreeMap<Address, BTreeMap<String, AtCreate>>,
    /// (path, refinement) per address, for its Apply `assertions`.
    pub assertions: BTreeMap<Address, Vec<(String, crate::lattice::Constraint)>>,
}

/// Why an attribute is given at creation only, which says whether a
/// difference kept once the object exists is printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtCreate {
    /// `lifecycle(r, "bootstrap", Path)`: the program's value, needed when
    /// the object is made (a first boot's user data); a difference is kept
    /// and the plan says so (R-198).
    Bootstrap,
    /// `ignore_changes(r, Path)`: another writer's once the object exists
    /// (a scanner's tag, an autoscaler's count); kept, silently.
    Ignored,
}

impl AtCreate {
    pub fn word(self) -> &'static str {
        match self {
            AtCreate::Bootstrap => "bootstrap",
            AtCreate::Ignored => "ignore_changes",
        }
    }
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
                while let Some((scope, _)) = crate::address::scope_split(name) {
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
    fn is_instance(&self, addr: &Address) -> bool {
        self.by_scope.get(&addr.name) == Some(&addr.typ)
    }

    /// The copies a resource is in, innermost first: `edge.left.vpc` is in
    /// `edge.left` and `edge`.
    pub fn enclosing(&self, addr: &Address) -> Vec<Address> {
        let mut out = Vec::new();
        let mut name = addr.name.as_str();
        while let Some((scope, _)) = crate::address::scope_split(name) {
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
        ("lifecycle", 3) => Some(0),
        _ => None,
    }
}

/// Whether `pred`, one of [`REF_RELATIONS`], takes `n` arguments; the
/// error names each shape it takes.
pub fn ref_arity(pred: &str, n: usize) -> Result<(), String> {
    let shapes: Vec<&(&str, usize, &str)> =
        REF_RELATIONS.iter().filter(|(p, ..)| *p == pred).collect();
    if shapes.iter().any(|(_, arity, _)| *arity == n) {
        return Ok(());
    }
    let or = |each: Vec<String>| each.join(" or ");
    Err(format!(
        "`{pred}` takes {} arguments: {} (R-42)",
        or(shapes.iter().map(|(_, a, _)| a.to_string()).collect()),
        or(shapes.iter().map(|(.., s)| format!("`{s}`")).collect()),
    ))
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
    (
        "lifecycle",
        3,
        "lifecycle(resource, \"bootstrap\", \"path\")",
    ),
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
    /// The lifecycle facts, checked against the schema: a `create_first`
    /// on a `type_replace(T, destroy_first)` type, or on one whose name is
    /// its identity and whose provider cannot generate one, is an error
    /// naming the resource. A flag on a copy (R-67) is on each of its
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
        // What each object's removal means, by the rows that say it.
        let mut removal: BTreeMap<Address, BTreeSet<String>> = BTreeMap::new();
        for f in facts {
            match (f.pred.as_str(), f.args.as_slice()) {
                ("lifecycle", [r, what, path]) => {
                    let Some(addr) = referenced(r) else {
                        bail!(
                            "{}: lifecycle takes the resource first, as its name in scope, \
                             `T[\"a\"]` or a variable (R-42)",
                            crate::spell::atom(f)
                        );
                    };
                    let (Some(what), Some(path)) = (text(what), text(path)) else {
                        bail!(
                            "{}: an attribute's lifecycle is a word and its path, \
                             lifecycle({addr}, \"bootstrap\", \"user_data\")",
                            crate::spell::atom(f)
                        );
                    };
                    let how = match what.as_str() {
                        "bootstrap" => AtCreate::Bootstrap,
                        other => bail!(
                            "lifecycle({addr}, {other:?}, {path:?}): unknown word for an \
                             attribute (expected bootstrap)"
                        ),
                    };
                    for addr in each(addr) {
                        out.given_at_create(addr, &path, how)?;
                    }
                }
                ("lifecycle", [r, what]) => {
                    let (Some(addr), Some(what)) = (referenced(r), text(what)) else {
                        bail!("lifecycle expects a resource and a flag, got {f:?}");
                    };
                    match what.as_str() {
                        // `prevent_destroy` a deny the evaluator derives
                        // (`POLICY_RULES`); a copy's: each of its
                        // resources it still has; an address the program
                        // no longer makes, itself.
                        w if REMOVAL.contains(&w) => {
                            for addr in each(addr) {
                                removal.entry(addr).or_default().insert(what.clone());
                            }
                        }
                        "create_first" => {
                            // On a copy: each of its resources whose type
                            // allows it.
                            let copy = instances.is_instance(&addr);
                            for addr in each(addr) {
                                let refused = create_first_refused(schema, &addr);
                                if copy && refused.is_some() {
                                    continue;
                                }
                                if let Some(why) = refused {
                                    bail!("{why}");
                                }
                                out.created_first.insert(addr);
                            }
                        }
                        "bootstrap" => bail!(
                            "lifecycle({addr}, \"bootstrap\"): bootstrap is said of an \
                             attribute; name it, lifecycle({addr}, \"bootstrap\", \"user_data\")"
                        ),
                        other => bail!(
                            "lifecycle({addr}, {other}): unknown flag \
                             (expected prevent_destroy, create_first, retain or destroy)"
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
                        out.given_at_create(addr, &path, AtCreate::Ignored)?;
                    }
                }
                _ => {}
            }
        }
        out.seeded = schema.lifecycle.clone();
        for (addr, words) in removal {
            // A delete of it would be two things at once: which is meant
            // is the program's to say.
            if let [a, b, ..] = words.iter().collect::<Vec<_>>().as_slice() {
                bail!(
                    "lifecycle({addr}, {a:?}) and lifecycle({addr}, {b:?}) are both written: a \
                     delete of it {} by the first and {} by the second; keep one",
                    removal_means(a),
                    removal_means(b)
                );
            }
            if words.contains("retain") {
                out.retain.insert(addr.clone());
            }
            out.said.insert(addr);
        }
        Ok(out)
    }

    /// Whether a delete of `addr` forgets it (R-154): a row says `retain`,
    /// or none says what its removal means and its type's lifecycle is
    /// `retain` (`type_lifecycle`).
    pub fn retains(&self, addr: &Address) -> bool {
        self.retain.contains(addr)
            || (!self.said.contains(addr)
                && self.seeded.get(&addr.typ).is_some_and(|w| w == "retain"))
    }

    /// `path` of `addr` is given at creation only, `how`; one path is said
    /// one way.
    fn given_at_create(&mut self, addr: Address, path: &str, how: AtCreate) -> Result<()> {
        let paths = self.at_create.entry(addr.clone()).or_default();
        match paths.insert(path.to_string(), how) {
            Some(was) if was != how => {
                let fact = |w: AtCreate| match w {
                    AtCreate::Bootstrap => format!("lifecycle({addr}, \"bootstrap\", {path:?})"),
                    AtCreate::Ignored => format!("ignore_changes({addr}, {path:?})"),
                };
                bail!(
                    "{} and {} are both written: a difference there is said by the first and \
                     silent by the second; keep one",
                    fact(AtCreate::Bootstrap),
                    fact(AtCreate::Ignored)
                )
            }
            _ => Ok(()),
        }
    }

    /// The paths of `addr` given at creation only, with why.
    pub fn at_create_of(&self, addr: &Address) -> impl Iterator<Item = (&String, AtCreate)> {
        self.at_create
            .get(addr)
            .into_iter()
            .flatten()
            .map(|(p, h)| (p, *h))
    }

    /// Whether a replacement of `addr` is created before the old object is
    /// deleted: the schema's `type_replace` decides, and for a type that
    /// allows either order, `lifecycle(r, "create_first")` (else destroy
    /// first).
    pub fn create_first(&self, schema: &Schema, addr: &Address) -> bool {
        match schema.replace_order(&addr.typ) {
            ReplaceOrder::CreateFirst => true,
            ReplaceOrder::DestroyFirst => false,
            ReplaceOrder::Either => self.created_first.contains(addr),
        }
    }
}

/// The words that say what removal from the program means for an object:
/// a delete of it deletes it, is refused, or forgets it. A type's
/// `type_lifecycle` seeds one of them, which a row the program writes
/// with another replaces.
pub const REMOVAL: [&str; 3] = ["destroy", "prevent_destroy", "retain"];

/// What a delete is under removal word `w`, for the error naming two.
fn removal_means(w: &str) -> &'static str {
    match w {
        "prevent_destroy" => "is refused",
        "retain" => "forgets it",
        _ => "is sent",
    }
}

/// The relation the program's own `lifecycle/2` rows are, where a type's
/// lifecycle is seeded ([`lifecycle_written`]): `lifecycle` is then
/// these rows and the seeded ones.
pub const WRITTEN: &str = "__lifecycle_written";

/// `__lifecycle_said(T, A)`: the program gives `T[A]` a removal word
/// ([`REMOVAL`]), by its own row or its copy's.
pub const SAID: &str = "__lifecycle_said";

/// The program's `lifecycle/2` rows as [`WRITTEN`]'s, when the schema
/// seeds a type's lifecycle ([`lifecycle_prelude`] makes `lifecycle` of
/// them again); `rules` and `facts` as they are otherwise. A program that
/// writes [`WRITTEN`] or [`SAID`] by name is refused: they are dform's.
pub fn lifecycle_written(
    mut rules: Vec<RuleStmt>,
    mut facts: Vec<Atom>,
    schema: &Schema,
) -> Result<(Vec<RuleStmt>, Vec<Atom>)> {
    if let Some(a) = rules
        .iter()
        .map(|r| &r.head)
        .chain(&facts)
        .find(|a| a.pred == WRITTEN || a.pred == SAID)
    {
        let at = crate::diag::place(a.span).map_or(String::new(), |p| format!("{p}: "));
        bail!(
            "{at}{} is dform's own, not a relation a program writes; a resource's lifecycle is \
             `lifecycle(r, \"retain\")`",
            a.pred
        );
    }
    if schema.lifecycle.is_empty() {
        return Ok((rules, facts));
    }
    let rename = |a: &mut Atom| {
        if a.pred == "lifecycle" && a.args.len() == 2 {
            a.pred = WRITTEN.into();
        }
    };
    rules.iter_mut().for_each(|r| rename(&mut r.head));
    facts.iter_mut().for_each(rename);
    Ok((rules, facts))
}

/// The rules that make `lifecycle` where the schema seeds a type's
/// (`type_lifecycle(T, W)`): the program's rows ([`WRITTEN`]), and for
/// each resource of a seeded type the program gives no removal word
/// ([`SAID`]: its own row or its copy's), the row `lifecycle(r, W)`. The
/// program's word wins as a `set` wins over a schema default. A body
/// reads the rows as the program's own, and `why` explains a seeded one
/// by the rule's doc comment.
pub fn lifecycle_prelude(schema: &Schema) -> Result<Vec<RuleStmt>> {
    if schema.lifecycle.is_empty() {
        return Ok(Vec::new());
    }
    let mut src = format!("#| the program's lifecycle\nlifecycle(r, w) where {WRITTEN}(r, w)\n");
    for (t, w) in &schema.lifecycle {
        let by = match schema.provider_of.get(t) {
            Some(p) => format!("the schema of provider {p}"),
            None => "the schema".into(),
        };
        src.push_str(&format!(
            "#| {by}: each {t} is {w:?} (type_lifecycle), unless the program writes another\n\
             lifecycle(r, w) where {{ type_lifecycle(\"{t}\", w), r in {t}, \
             not {SAID}(\"{t}\", r) }}\n"
        ));
    }
    let lowered = crate::transform::lower(&crate::parser::parse_program(&src)?)?;
    let mut out: Vec<RuleStmt> = lowered
        .program
        .statements
        .into_iter()
        .filter_map(|s| match s {
            crate::ast::Stmt::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    out.extend(said_rules());
    Ok(out)
}

/// [`SAID`]'s rules, in the core: the program's removal word for `T[A]`
/// itself, or for a copy whose scope `A` is in (`S`, or `U.N` for the
/// copy `N` in `U`).
///
/// ```text
/// __lifecycle_said(T, A) :- __lifecycle_written(__ref(T, A, ""), W), member(REMOVAL, W).
/// __lifecycle_said(T, A) :- __lifecycle_written(__ref(C, S, ""), W), member(REMOVAL, W),
///     instance_of(C, U, N), S = scope(U, N), want(T, A), str.starts_with(A, "S.").
/// ```
fn said_rules() -> Vec<RuleStmt> {
    use crate::ast::{Lit, str_term};
    let var = |v: &str| Term::Var(v.into());
    let atom = |pred: &str, args: Vec<Term>| Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let reference = |t: &str, a: &str| Term::Func {
        name: crate::address::REF.into(),
        args: vec![var(t), var(a), str_term("")],
    };
    let format = |f: &str, args: Vec<Term>| Term::Func {
        name: crate::address::FORMAT.into(),
        args: std::iter::once(str_term(f)).chain(args).collect(),
    };
    let removal = Lit::Pos(atom(
        "member",
        vec![
            Term::Val(Value::List(
                REMOVAL.iter().map(|w| Value::Str(w.to_string())).collect(),
            )),
            var("W"),
        ],
    ));
    let head = atom(SAID, vec![var("T"), var("A")]);
    let own = RuleStmt::new(
        head.clone(),
        vec![
            Lit::Pos(atom(WRITTEN, vec![reference("T", "A"), var("W")])),
            removal.clone(),
        ],
    );
    let copy = |scope: Vec<Lit>| {
        let mut body = vec![
            Lit::Pos(atom(WRITTEN, vec![reference("C", "S"), var("W")])),
            removal.clone(),
        ];
        body.extend(scope);
        body.push(Lit::Pos(atom("want", vec![var("T"), var("A")])));
        body.push(Lit::Pos(atom(
            "str.starts_with",
            vec![var("A"), format("%s.", vec![var("S")])],
        )));
        RuleStmt::new(head.clone(), body)
    };
    vec![
        own,
        // A copy at the top: its scope is its name.
        copy(vec![Lit::Pos(atom(
            crate::modules::INSTANCE_OF,
            vec![var("C"), str_term(""), var("S")],
        ))]),
        // A copy inside another: `U.N`.
        copy(vec![
            Lit::Pos(atom(
                crate::modules::INSTANCE_OF,
                vec![var("C"), var("U"), var("N")],
            )),
            Lit::Neq(var("U"), str_term("")),
            Lit::Eq(var("S"), format("%s.%s", vec![var("U"), var("N")])),
        ]),
    ]
}

/// Why a replacement of `addr` cannot be created before its old object is
/// deleted, if it cannot (R-189): its name is its identity (two objects
/// cannot share it) and its provider cannot generate another
/// (`type_remote_name`), or its provider replaces it destroy-first
/// (`type_replace`). An object whose identity is an id the provider
/// assigns can: the two share a name for a moment.
pub fn create_first_refused(schema: &Schema, addr: &Address) -> Option<String> {
    let at = crate::report::address(addr);
    if let Some(p) = schema.named_identity(&addr.typ)
        && schema.remote_name_of(&addr.typ).is_none()
    {
        return Some(format!(
            "{at}: create_first is not possible: {p} is its identity, and its provider cannot \
             generate one; give the replacement another name, or let it be replaced \
             destroy-first"
        ));
    }
    (schema.replace_order(&addr.typ) == ReplaceOrder::DestroyFirst).then(|| {
        format!(
            "{at}: create_first is not possible: type {} is type_replace destroy_first; its \
             old object must be deleted before the replacement is created",
            addr.typ
        )
    })
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
///                                 delete, delete_deposed, forget or remaining;
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
fn doc_digest(doc: Option<&serde_json::Value>) -> String {
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
        ActionKind::Forget => "forget",
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
    // A set's element is labeled by its content (`policies[#k3j2d]`):
    // an element only one side has is a change, but a fresh null the
    // program holds where the world has another element of the same
    // set is a stale identity, as a fresh null against a constant is.
    let set_of = |p: &str| p.find("[#").map(|i| p[..i].to_string());
    let mut fresh_only = Vec::new();
    let mut world_only = BTreeSet::new();
    for p in paths {
        let (Some(dv), Some(wv)) = (d.get(p), w.get(p)) else {
            changed = true;
            match (d.get(p), set_of(p)) {
                (
                    Some(Value::Null {
                        class: NullClass::Fresh,
                        ..
                    }),
                    Some(list),
                ) => fresh_only.push(list),
                (None, Some(list)) => {
                    world_only.insert(list);
                }
                _ => {}
            }
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
    if fresh_only.iter().any(|l| world_only.contains(l)) {
        stale = true;
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
    /// The deepest condition `why` names (`input one.namespace is not
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
                            Term::Func { name, args } if name == crate::address::SCOPED => {
                                match (args.as_slice(), v) {
                                    (
                                        [Term::Val(Value::Str(n)), Term::Val(Value::Str(a))],
                                        Term::Val(Value::Str(v)),
                                    ) => crate::address::scoped(n, a) == *v,
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
            let reason = crate::why::not::reason(&addr.typ, &addr.name, res, redact)
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
                    .any(|a| crate::address::parse_resource(a).is_ok_and(|a| a.typ == *n))
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

    /// The plan file's format (R-200 after: a sequence of deployments,
    /// one for a deployment that reads none).
    pub const VERSION: u32 = 5;

    /// The deployment's master (`custody`): 32 random bytes, the key of
    /// every digest of a secret dform keeps and the root of `random.*`
    /// and the memo seal. It never leaves the state's backend in the
    /// clear but as the key file `state.key`.
    #[derive(Clone)]
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
                .map_err(|_| anyhow::anyhow!("the key {}: not 32 bytes", store.locate(KEY)))?;
            Ok(Some(Key(key)))
        }

        /// Its 32 bytes.
        pub fn bytes(&self) -> [u8; 32] {
            self.0
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
    }

    impl From<[u8; 32]> for Key {
        fn from(bytes: [u8; 32]) -> Key {
            Key(bytes)
        }
    }

    /// A redacted value as the file stores it: a sensitive one with the
    /// digest of its bytes keyed with the deployment's master (`key`); in a
    /// run that does not hold it, a derived one with its derivation digest
    /// (`secrets::standin`, R-164) and any other with its label alone, never
    /// an unkeyed digest. Anything else as shown.
    pub fn stored(key: Option<&Key>, shown: report::Shown, value: impl FnOnce() -> Json) -> Json {
        match (shown, key) {
            (report::Shown::Sensitive(l), Some(k)) => serde_json::json!({
                "sensitive": l,
                "digest": k.digest(&serde_json::to_vec(&value()).unwrap_or_default()),
            }),
            (report::Shown::Sensitive(l), None) => {
                match crate::secrets::standin::digest(&value()) {
                    Some(d) => serde_json::json!({ "sensitive": l, "derived": d }),
                    None => serde_json::json!({ "sensitive": l }),
                }
            }
            (s, _) => s.json(),
        }
    }

    /// A plan file (`plan TARGET --out FILE`): the deployments of the
    /// target's closure in apply order, the target last, each its own
    /// plan; one deployment's file is the sequence of one. `apply FILE`
    /// applies them in turn, each checked against its plan at its turn.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Sequence {
        pub version: u32,
        pub deployments: Vec<Step>,
    }

    /// One deployment of a [`Sequence`]: its full name (R-200), those of
    /// the sequence it is applied after (what it reads), and its plan.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Step {
        pub deployment: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub after: Vec<String>,
        #[serde(flatten)]
        pub plan: PlanFile,
    }

    impl Sequence {
        /// The file at `path`, of this dform's format.
        pub fn load(path: &Path) -> Result<Sequence> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read plan file {}", path.display()))?;
            let v: Json = serde_json::from_str(&text)
                .with_context(|| format!("parse plan file {}", path.display()))?;
            let version = v["version"].as_u64().unwrap_or_default();
            if version != VERSION as u64 {
                anyhow::bail!(
                    "plan file {}: version {version} (this dform writes {VERSION}): plan again",
                    path.display(),
                );
            }
            let f: Sequence = serde_json::from_value(v)
                .with_context(|| format!("parse plan file {}", path.display()))?;
            if f.deployments.is_empty() {
                anyhow::bail!("plan file {}: it plans no deployment", path.display());
            }
            Ok(f)
        }

        pub fn save(&self, path: &Path) -> Result<()> {
            crate::store::write_atomic(
                path,
                (serde_json::to_string_pretty(self)? + "\n").as_bytes(),
            )
            .with_context(|| format!("write plan file {}", path.display()))
        }
    }

    /// One deployment's plan in a plan file ([`Step`]).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PlanFile {
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
        /// Written by a run that does not hold the deployment's master
        /// (R-164): its sensitive values carry a derivation digest or
        /// their label alone, never a keyed digest ([`stored`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub unkeyed: bool,
        /// Each value given at an object's creation only that differs
        /// from what it was made with (R-198): kept, no change; values
        /// redacted as a change's are.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub kept: Vec<Kept>,
    }

    /// A value given at creation only that the plan keeps (R-198).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Kept {
        #[serde(rename = "type")]
        pub typ: String,
        pub name: String,
        pub path: String,
        pub before: Json,
        pub after: Json,
        /// Why it differs, where dform can tell (`(the key was replaced)`,
        /// R-218).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub note: Option<String>,
    }

    /// The values `plan` keeps of objects given at their creation only
    /// (R-198), redacted as a delta's.
    pub fn kept(plan: &Plan, schema: &Schema, r: &Redactor, key: Option<&Key>) -> Vec<Kept> {
        let side = |v: Option<&Json>, sensitive: bool| {
            stored(key, report::shown(v, sensitive, schema, r), || {
                v.cloned().unwrap_or(Json::Null)
            })
        };
        plan.actions
            .iter()
            .flat_map(|a| {
                a.kept().iter().map(|c| Kept {
                    typ: a.addr.typ.clone(),
                    name: a.addr.name.clone(),
                    path: c.path.clone(),
                    before: side(c.before.as_ref(), c.sensitive),
                    after: side(c.after.as_ref(), c.sensitive),
                    note: c.note.clone(),
                })
            })
            .collect()
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
        /// Empty in a run that does not hold the master.
        #[serde(default, skip_serializing_if = "String::is_empty")]
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
        /// Planned against its provider's offline schema while the
        /// provider's connection waits (R-193): its re-plan against what
        /// the connection reaches may differ.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub provisional: bool,
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
        let mut lines: Vec<String> = world_facts.iter().map(crate::spell::atom).collect();
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
            ActionKind::Forget => "forget",
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
        key: Option<&Key>,
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
        key: Option<&Key>,
    ) -> Entry {
        let side = |v: Option<&Json>, sensitive: bool| {
            stored(key, report::shown(v, sensitive, schema, r), || {
                v.cloned().unwrap_or(Json::Null)
            })
        };
        let name = a.addr.to_string();
        let on = report::waits_on(a, sections).unwrap_or_default();
        Entry {
            typ: a.addr.typ.clone(),
            name: a.addr.name.clone(),
            action: action_name(&a.kind).into(),
            tick: tick_of.get(name.as_str()).copied(),
            provisional: !on.is_empty() && on.iter().all(|l| sections.provisional.contains(l)),
            on,
            changes: a
                .sent()
                .into_iter()
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
    pub fn groups(res: &EvalResult, r: &Redactor, key: Option<&Key>) -> Vec<Group> {
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
                head: crate::spell::atom(head),
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
        key: Option<&Key>,
    ) -> BTreeMap<String, Json> {
        bindings
            .filter(|(_, v)| !crate::stuck::has_null(v))
            .map(|(k, v)| {
                let j = stored(key, report::shown_value(v, r), || {
                    crate::spell::value_to_json(v)
                });
                (k.clone(), j)
            })
            .collect()
    }

    /// Round 0's resolutions, from the `resolve/2` facts, redacted.
    pub fn resolved(
        facts: &std::collections::BTreeSet<Atom>,
        r: &Redactor,
        key: Option<&Key>,
    ) -> Vec<Resolved> {
        facts
            .iter()
            .filter(|a| a.pred == "resolve")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(l)), Term::Val(v)] => Some(Resolved {
                    null: l.clone(),
                    value: stored(key, report::shown_value(v, r), || {
                        crate::spell::value_to_json(v)
                    }),
                }),
                _ => None,
            })
            .collect()
    }

    impl PlanFile {
        /// The plan of the one deployment the file at `path` plans; a
        /// file of several is an error naming them.
        pub fn load(path: &Path) -> Result<PlanFile> {
            let mut f = Sequence::load(path)?;
            if f.deployments.len() > 1 {
                let names: Vec<&str> = f
                    .deployments
                    .iter()
                    .map(|s| s.deployment.as_str())
                    .collect();
                anyhow::bail!(
                    "plan file {}: it plans {}, not one deployment",
                    path.display(),
                    names.join(", then ")
                );
            }
            Ok(f.deployments.remove(0).plan)
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

        /// The differences between this file's delta and `current`, the
        /// delta re-evaluated at the start of tick 1; empty when the
        /// file's delta is reproduced. A later tick is the boundary's
        /// ([`tick_differences`]): it stops before a tick that differs.
        pub fn stale(&self, current: &[Entry]) -> Vec<String> {
            let key = |e: &Entry| (e.typ.clone(), e.name.clone());
            let saved: BTreeMap<(String, String), &Entry> =
                self.deformations.iter().map(|e| (key(e), e)).collect();
            let now: BTreeMap<(String, String), &Entry> =
                current.iter().map(|e| (key(e), e)).collect();
            let mut out = Vec::new();
            for (k, c) in &now {
                let addr = crate::address::Address {
                    typ: k.0.clone(),
                    name: k.1.clone(),
                };
                let at = report::address(&addr);
                let Some(s) = saved.get(k) else {
                    out.push(format!("{} {at}: not in the plan file", c.action));
                    continue;
                };
                // What the file planned against the offline schema
                // differs as the re-plan against the cluster says.
                let provisional = |d: String| match s.provisional {
                    true => format!("{d}  (planned provisionally, against the offline schema)"),
                    false => d,
                };
                if s.action != c.action {
                    out.push(provisional(format!(
                        "{at}: the plan file has {}, re-evaluation has {}",
                        s.action, c.action
                    )));
                    continue;
                }
                out.extend(leaf_differences(&addr, s, c).into_iter().map(provisional));
            }
            for (k, s) in &saved {
                if now.contains_key(k) {
                    continue;
                }
                let at = report::address(&crate::address::Address {
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

    /// One way a tick re-planned at its boundary differs from the tick
    /// as the plan the apply showed (or a plan file, an approval) had it
    /// (After R-156): a change added to the tick (`+`), one gone from it
    /// (`-`), or the same change with another action or value (`~`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Difference {
        pub mark: char,
        pub addr: crate::address::Address,
        /// The attribute a value difference is of.
        pub path: Option<String>,
        /// What differs, after the address: `cidr = "a" → "b"`, `not in
        /// the plan shown`.
        pub what: String,
    }

    impl Difference {
        /// Its line under `tick N differs from the plan shown:`.
        pub fn line(&self) -> String {
            format!(
                "  {} {}  {}",
                self.mark,
                report::address(&self.addr),
                self.what
            )
        }

        /// What it is about, as an error names it: the address, or the
        /// attribute.
        pub fn name(&self) -> String {
            match &self.path {
                Some(p) => report::attribute(&self.addr, p),
                None => report::address(&self.addr),
            }
        }
    }

    /// What differs between tick `tick` as `shown` has it (the delta of
    /// the plan the apply showed, or of the plan file it applies) and as
    /// `now` has it (the delta re-planned at the tick's boundary): the
    /// same changes, the same values where the shown plan knew them; a
    /// value it did not know (a null) is not a difference whatever it
    /// became, nor is a leaf the re-plan adds whose value is still
    /// unknown, nor a change the re-plan holds for a later tick (it
    /// waits). `ran`: what earlier ticks of this apply ran, which changes
    /// again if it is in this tick. `key` the digests are keyed with: a
    /// value shown in the clear that the re-plan holds as a secret, or
    /// one shown as a secret that the re-plan holds in the clear (its
    /// schema arrived at the boundary, R-215), is the same value when the
    /// one digest is the other's. Empty when the tick is as shown.
    pub fn tick_differences(
        shown: &[Entry],
        now: &[Entry],
        tick: usize,
        ran: &std::collections::BTreeSet<(String, String)>,
        key: Option<&Key>,
    ) -> Vec<Difference> {
        let digest = |v: &Json| key.map(|k| k.digest(&serde_json::to_vec(v).unwrap_or_default()));
        let held = |v: &Json| v.get("digest").and_then(Json::as_str).map(str::to_string);
        let same = |was: &Json, is: &Json| {
            was == is
                || match (sensitive(was), sensitive(is)) {
                    (false, true) => held(is).is_some_and(|d| digest(was) == Some(d)),
                    (true, false) => held(was).is_some_and(|d| digest(is) == Some(d)),
                    _ => false,
                }
        };
        // Either side a secret: both said as one.
        let pair = |was: &Json, is: &Json| match sensitive(was) || sensitive(is) {
            true => (sensitive_text(was), sensitive_text(is)),
            false => (leaf_text(was), leaf_text(is)),
        };
        let key = |e: &Entry| (e.typ.clone(), e.name.clone());
        let shown: BTreeMap<(String, String), &Entry> = shown.iter().map(|e| (key(e), e)).collect();
        let now: BTreeMap<(String, String), &Entry> = now.iter().map(|e| (key(e), e)).collect();
        let address = |k: &(String, String)| crate::address::Address {
            typ: k.0.clone(),
            name: k.1.clone(),
        };
        let mut out = Vec::new();
        let mut push = |mark: char, k: &(String, String), path: Option<String>, what: String| {
            out.push(Difference {
                mark,
                addr: address(k),
                path,
                what,
            })
        };
        for (k, c) in now.iter().filter(|(_, c)| c.tick == Some(tick)) {
            let s = shown.get(k);
            // The object a create-first replacement deposed is
            // deleted the tick after; the plan showed it so.
            if c.action == "delete_deposed" && s.is_some_and(|s| s.action == "replace_create_first")
            {
                continue;
            }
            let Some(s) = s else {
                push('+', k, None, format!("{}, not in the plan shown", c.action));
                continue;
            };
            if ran.contains(k) {
                let when = match s.tick {
                    Some(t) => format!("tick {t}"),
                    None => "an earlier tick".into(),
                };
                push(
                    '+',
                    k,
                    None,
                    format!("{} again; it ran in {when}", c.action),
                );
                continue;
            }
            if s.action != c.action {
                push(
                    '~',
                    k,
                    None,
                    format!("{}, the plan shown has {}", c.action, s.action),
                );
                continue;
            }
            let was: BTreeMap<&str, &Leaf> =
                s.changes.iter().map(|l| (l.path.as_str(), l)).collect();
            let is: BTreeMap<&str, &Leaf> =
                c.changes.iter().map(|l| (l.path.as_str(), l)).collect();
            for (p, l) in &is {
                let leaf = Some(p.to_string());
                match was.get(p) {
                    None if is_null(&l.after) => {}
                    None => push(
                        '~',
                        k,
                        leaf,
                        format!("{p} = {}  (not in the plan shown)", leaf_text(&l.after)),
                    ),
                    Some(w) if !is_null(&w.after) && !same(&w.after, &l.after) => {
                        let (a, b) = pair(&w.after, &l.after);
                        push('~', k, leaf, format!("{p} = {a} → {b}"))
                    }
                    Some(w) if !same(&w.before, &l.before) => {
                        let (a, b) = pair(&w.before, &l.before);
                        push(
                            '~',
                            k,
                            leaf,
                            format!("{p} was {a} in the plan shown, is {b} now"),
                        )
                    }
                    Some(_) => {}
                }
            }
            for (p, w) in was.iter().filter(|(p, _)| !is.contains_key(*p)) {
                push(
                    '~',
                    k,
                    Some(p.to_string()),
                    format!("{p} = {}  (no longer changed)", leaf_text(&w.after)),
                );
            }
        }
        // A change still to come, in a later tick or `later`, waits on
        // what this boundary did not make known yet: a wait, not a
        // difference.
        for (k, s) in shown.iter().filter(|(_, s)| s.tick == Some(tick)) {
            if !ran.contains(k) && !now.contains_key(k) {
                push('-', k, None, format!("{}, no longer a change", s.action));
            }
        }
        out
    }

    fn sensitive(v: &Json) -> bool {
        v.get("sensitive").is_some()
    }

    /// A value beside a secret, said as one: never in the clear.
    fn sensitive_text(v: &Json) -> String {
        match sensitive(v) {
            true => leaf_text(v),
            false => "(sensitive)".into(),
        }
    }

    /// A leaf's value as a tick's difference says it: a null as what it
    /// stands for (`?db.postgres main.endpoint`), a secret as
    /// `(sensitive)` with the head of its digest, else its JSON.
    fn leaf_text(v: &Json) -> String {
        if is_null(v) {
            let n = v["null"].as_str().unwrap_or_default();
            return match crate::address::parse(n) {
                Ok((a, Some(p))) => format!("?{}", report::attribute(&a, &p)),
                Ok((a, None)) => format!("?{}", report::address(&a)),
                Err(_) => format!("?{n}"),
            };
        }
        if sensitive(v) {
            return match v.get("digest").and_then(Json::as_str) {
                Some(d) => format!("(sensitive, digest {})", &d[..d.len().min(8)]),
                None => "(sensitive)".into(),
            };
        }
        match v {
            Json::Null => "(none)".into(),
            v => serde_json::to_string(v).unwrap_or_default(),
        }
    }

    fn is_null(v: &Json) -> bool {
        matches!(v, Json::Object(m) if m.len() == 2 && m.contains_key("null") && m.contains_key("class"))
    }

    fn leaf_differences(at: &crate::address::Address, saved: &Entry, now: &Entry) -> Vec<String> {
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

    /// A set's elements are by content (R-158): a fresh null the program
    /// adds is an element more; one in place of an element the world has
    /// is a stale identity.
    #[test]
    fn a_fresh_element_of_a_set_is_an_update_or_a_stale_identity() {
        let fresh = || null("p/new#id", NullClass::Fresh);
        let desired = BTreeMap::from([
            (
                addr("added"),
                doc(&[("ps[#a]", s("p-1")), ("ps[#n]", fresh())]),
            ),
            (addr("stale"), doc(&[("ps[#n]", fresh())])),
        ]);
        let world = BTreeMap::from([
            (addr("added"), doc(&[("ps[#a]", s("p-1"))])),
            (addr("stale"), doc(&[("ps[#a]", s("p-1"))])),
        ]);
        let got: BTreeMap<String, Kind> = deformation(&desired, &world)
            .into_iter()
            .map(|d| (d.addr.name, d.kind))
            .collect();
        assert_eq!(got["added"], Kind::Update);
        assert_eq!(got["stale"], Kind::Drift);
    }

    /// A tick re-planned at its boundary (After R-156): a value the plan
    /// did not know is not a difference whatever it became; a value it
    /// knew that is another now is; a clear value the re-plan holds as a
    /// secret with the same digest is the same, as is a secret shown that
    /// the re-plan holds in the clear (R-215), and one with another digest
    /// is said without its bytes.
    #[test]
    fn a_tick_differs_where_a_known_value_does() {
        use file::{Entry, Leaf, tick_differences};
        let key = file::Key::from([7; 32]);
        let digest = |v: &serde_json::Value| key.digest(&serde_json::to_vec(v).unwrap());
        let entry = |tick, leaves: &[(&str, serde_json::Value)]| Entry {
            typ: "t".into(),
            name: "a".into(),
            action: "create".into(),
            tick: Some(tick),
            on: vec![],
            changes: leaves
                .iter()
                .map(|(p, v)| Leaf {
                    path: p.to_string(),
                    before: serde_json::Value::Null,
                    after: v.clone(),
                })
                .collect(),
            dependents: vec![],
            provisional: false,
        };
        let null = serde_json::json!({"null": "t[\"b\"].id", "class": "fresh"});
        let secret =
            |v: &str| serde_json::json!({"sensitive": "t/a#x", "digest": digest(&v.into())});
        let shown = [entry(
            2,
            &[
                ("id", null),
                ("rv", "115".into()),
                ("token", "T".into()),
                ("pw", "P".into()),
                ("name", secret("n")),
            ],
        )];
        let now = [entry(
            2,
            &[
                ("id", "b-1".into()),
                ("rv", "200".into()),
                ("token", secret("T")),
                ("pw", secret("Q")),
                ("name", "n".into()),
            ],
        )];
        let ran = BTreeSet::new();
        let lines: Vec<String> = tick_differences(&shown, &now, 2, &ran, Some(&key))
            .iter()
            .map(|d| d.line())
            .collect();
        let q = &digest(&"Q".into())[..8];
        assert_eq!(
            lines,
            [
                format!("  ~ t a  pw = (sensitive) → (sensitive, digest {q})"),
                "  ~ t a  rv = \"115\" → \"200\"".to_string(),
            ]
        );
        assert!(tick_differences(&shown, &shown, 2, &ran, Some(&key)).is_empty());
        // Held for a later tick, or `later`: it waits, it does not differ.
        for tick in [Some(3), None] {
            let held = Entry {
                tick,
                ..now[0].clone()
            };
            assert!(tick_differences(&shown, &[held], 2, &ran, Some(&key)).is_empty());
        }
        // Gone from the tick, and a change it did not show.
        let other = Entry {
            name: "c".into(),
            ..now[0].clone()
        };
        let lines: Vec<String> = tick_differences(&shown, &[other], 2, &ran, Some(&key))
            .iter()
            .map(|d| d.line())
            .collect();
        assert_eq!(
            lines,
            [
                "  + t c  create, not in the plan shown",
                "  - t a  create, no longer a change"
            ]
        );
    }

    /// A plan file's change planned against its provider's offline
    /// schema (R-193) is marked so, and a re-plan against the cluster
    /// that differs says it was.
    #[test]
    fn a_provisional_change_that_differs_says_it_was_provisional() {
        let file: file::PlanFile = serde_json::from_value(serde_json::json!({
            "stack": "apps", "world_digest": "",
            "inputs": {"files": [], "set": [], "data": [], "providers": [],
                       "world": null, "inventory": null},
            "deformations": [{
                "type": "k8s.namespace", "name": "apps", "action": "create", "tick": null,
                "on": ["provider k8s  kubeconfig = platform[env].kubeconfig"],
                "changes": [{"path": "metadata.name", "before": null, "after": "apps"}],
                "provisional": true,
            }],
            "pending_groups": [], "nulls": {"resolved": [], "unresolved": []}, "ticks": [],
        }))
        .unwrap();
        let mut now = file.deformations.clone();
        now[0].changes[0].after = "web".into();
        now[0].provisional = false;
        assert_eq!(
            file.stale(&now),
            [
                "k8s.namespace apps.metadata.name: the plan file sets \"apps\", re-evaluation \
                 sets \"web\"  (planned provisionally, against the offline schema)"
            ]
        );
        let json = serde_json::to_value(&now[0]).unwrap();
        assert!(json.get("provisional").is_none(), "{json}");
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
