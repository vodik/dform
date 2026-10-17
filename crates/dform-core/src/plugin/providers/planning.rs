//! The plan: refresh, every desired document resolved in dependency
//! order, the Z-set `desired − world`, and each deformation's Plan call.

use super::written::Holders;
use super::{Object, Providers, World, key, not_sent};
use crate::address::Address;
use crate::plugin::backend::{CallError, Ticket};
use crate::plugin::pb;
use crate::plugin::wire;
use crate::provider::{
    self, Action, ActionKind, Change, Plan, Planned, get_path, remove_path, set_path,
};
use crate::resources::{Adopt, Resource};
use crate::state::{self, State, StateEntry};
use crate::value::{NullClass, Value};
use crate::zset::{self, Lifecycle};
use anyhow::{Result, bail};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// One Plan call: the address, its remote id (empty for a create), the
/// world's document and the desired one.
type Ask = (Address, String, Option<Json>, Option<Json>);

impl Providers {
    /// Of an object that exists, each attribute `lifecycle` says is given
    /// at its creation only is dropped from both sides, `want` the
    /// program's and `doc` the world's (R-198): not compared, so neither a
    /// change nor, at a `force_new` path, a replace. A create (no world
    /// side) sends it. Of a `bootstrap` one, a value that differs from
    /// what the object was made with is kept: the plan says so, with the
    /// note `notes` has of its path ([`Providers::written_before`]).
    fn at_create(
        &self,
        addr: &Address,
        lifecycle: &Lifecycle,
        want: &mut Json,
        doc: &mut Json,
        notes: &BTreeMap<String, &'static str>,
    ) -> Vec<Change> {
        // One attribute's leaves in the canonical form the Z-set compares.
        let flat = |d: &Json, p: &str| {
            let mut only = json!({});
            if let Some(v) = get_path(d, p) {
                set_path(&mut only, p, v.clone());
            }
            self.flat_value(&addr.typ, &only)
        };
        let mut kept = Vec::new();
        for (p, how) in lifecycle.at_create_of(addr) {
            if how == zset::AtCreate::Bootstrap && flat(want, p) != flat(doc, p) {
                kept.push(Change {
                    path: p.clone(),
                    before: get_path(doc, p).cloned(),
                    after: get_path(want, p).cloned(),
                    sensitive: self.schema().is_sensitive(&addr.typ, p),
                    note: notes.get(p).map(|n| n.to_string()),
                });
            }
            remove_path(doc, p);
            remove_path(want, p);
        }
        kept
    }

    /// The leaf-by-leaf changes from `before` to `after` (`provider::diff`).
    pub fn diff(&self, typ: &str, before: Option<&Json>, after: Option<&Json>) -> Vec<Change> {
        provider::diff(self.schema(), typ, before, after)
    }

    /// The owning providers' Plan for each `(address, remote, prior, desired)`:
    /// its changes, and whether they replace it. Every call is submitted
    /// before any answer is taken, so a backend that can runs them at once;
    /// the first failure, in order, is the error.
    #[tracing::instrument(
        target = "dform",
        level = "trace",
        name = "plan.phase",
        skip_all,
        fields(otel.name = "Plan calls", count = asks.len())
    )]
    fn plan_many(&self, asks: Vec<Ask>) -> Result<Vec<Planned>> {
        let mut out: Vec<Option<Result<Planned>>> = Vec::new();
        let mut submitted: Vec<(usize, usize, Ticket, Address)> = Vec::new();
        for (i, (addr, remote, prior, desired)) in asks.into_iter().enumerate() {
            if prior.is_none() && desired.is_none() {
                out.push(Some(Ok(Planned::default())));
                continue;
            }
            out.push(None);
            let req = pb::PlanRequest {
                r#type: addr.typ.clone(),
                name: addr.name.clone(),
                prior: prior.as_ref().map(wire::doc),
                desired: desired.as_ref().map(wire::doc),
                remote,
                ..Default::default()
            };
            let l = self.route(&addr.typ);
            submitted.push((i, l, self.submit(l, req)?, addr));
        }
        for l in &self.links {
            l.borrow_mut().flush();
        }
        for (i, l, t, addr) in submitted {
            let mut link = self.links[l].borrow_mut();
            let r = link
                .wait(t)
                .and_then(|r| link.expect::<pb::PlanResponse>("Plan", r));
            // A refusal in R-109's shape: the change as the plan prints it,
            // then the provider's message (`deployment::plan` adds the site).
            let r = r.map_err(|e| {
                let happened = match &e {
                    CallError::Refused(_) => "refused",
                    CallError::MaybeApplied(_) => "no answer",
                    CallError::Crashed(_) => "the provider died",
                };
                anyhow::Error::new(crate::report::Failure::of(
                    "plan",
                    &addr,
                    happened,
                    &e.to_string(),
                ))
            });
            out[i] = Some(r.and_then(|r| Planned::try_from(&r)));
        }
        out.into_iter()
            .map(|r| r.expect("every ask is answered"))
            .collect()
    }

    /// `ref(T, N, Attr)` to a configured attribute (a ref to a computed one
    /// is the evaluator's null): the program's value for N, else the world's.
    fn resolve_ref(&self, ctx: &Ctx, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let addr = Address {
            typ: typ.to_string(),
            name: name.to_string(),
        };
        let label = crate::value::null_label(typ, name, attr);
        if ctx.renaming.contains(&addr) && self.schema().remote_name_of(typ) == Some(attr) {
            return Ok(provider::null_json(&label));
        }
        if let Some(v) = ctx.resolved.get(&addr).and_then(|d| get_path(d, attr)) {
            return Ok(v.clone());
        }
        let existing = ctx.existing(&addr)?;
        match existing.as_ref().and_then(|o| get_path(&o.attrs, attr)) {
            Some(v) => Ok(v.clone()),
            None => match ctx.strict {
                Some(at) => Err(not_sent(
                    at,
                    &format!(
                        "{} is still unknown ({} does not set {attr})",
                        crate::report::attribute_label(&label),
                        crate::report::address(&addr)
                    ),
                )),
                None => Ok(provider::null_json(&label)),
            },
        }
    }

    /// A labeled null in a desired document. The executor fills a fresh or
    /// open one at Apply from its owner's Apply earlier in the dependency
    /// order (E §2.7); a secret travels as its label and the provider
    /// materializes it. Plan shows what is still unknown as `?label`.
    fn resolve_null(&self, ctx: &Ctx, label: &str, class: NullClass) -> Result<Json> {
        if class == NullClass::Secret {
            return Ok(match self.held.borrow().get(label) {
                Some(h) => provider::held_json(label, h),
                None => provider::secret_json(label),
            });
        }
        let mut found = None;
        if let Some((typ, name, path)) = crate::value::null_parts(label) {
            let owner = Address { typ, name };
            if !ctx.retracted.contains(&owner)
                && let Some(o) = ctx.existing(&owner)?
            {
                found = get_path(&o.computed, &path).cloned();
            }
        }
        match (found, ctx.strict) {
            (Some(v), _) => Ok(v),
            (None, Some(at)) => Err(not_sent(
                at,
                &format!(
                    "{} is still unknown (its resource has not been created)",
                    crate::report::attribute_label(label)
                ),
            )),
            (None, None) => Ok(provider::null_json(label)),
        }
    }

    fn resolve_cloud_ref(&self, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let k = key(typ, name);
        let Some(cur) = self.import(typ, name)? else {
            bail!("cloud_ref missing inventory resource {k}");
        };
        if let Some(v) = get_path(&cur.computed, attr).or_else(|| get_path(&cur.attrs, attr)) {
            return Ok(v.clone());
        }
        bail!(
            "cloud_ref missing attribute {}",
            Address {
                typ: typ.to_string(),
                name: name.to_string()
            }
            .attr(attr)
        );
    }

    /// A secret output of another stack in a resource's document must be
    /// held by a provider (`Config::held`), or sealed to this deployment
    /// and opened (its value then, not a null): otherwise nothing can read
    /// it here.
    /// `verb` and `addr` name the resource, `path` the attribute.
    fn check_held(&self, verb: &str, addr: &Address, path: &str, v: &Value) -> Result<()> {
        let join = |k: &str| crate::address::path_join(path, k);
        match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } if !self.held.borrow().contains_key(label) => {
                if let Some((name, _)) = crate::stack::deployment_output(label) {
                    bail!(
                        "{verb} {}: {} is a secret output of {name} that no provider holds, \
                         and it is not sealed to this deployment: apply {name} again, which \
                         seals it to each deployment of the project that reads it (R-166)",
                        addr.attr(path),
                        crate::address::label(label)
                    );
                }
                Ok(())
            }
            Value::List(xs) => xs
                .iter()
                .enumerate()
                .try_for_each(|(i, x)| self.check_held(verb, addr, &join(&i.to_string()), x)),
            Value::Obj(m) => m
                .iter()
                .try_for_each(|(k, x)| self.check_held(verb, addr, &join(k), x)),
            _ => Ok(()),
        }
    }

    /// The program leaves no attribute the schema requires unset (R-184):
    /// an error at the resource's site, before its provider is asked
    /// (`deployment::plan` adds the site); what the schema does not know
    /// stays the provider's refusal.
    fn check_required(&self, addr: &Address, doc: &Json) -> Result<()> {
        match self.schema().unset_message(&addr.typ, doc) {
            Some(m) => Err(crate::report::Failure::located(addr, m).into()),
            None => Ok(()),
        }
    }

    /// A desired resource's document as its provider takes it: quantities
    /// and times in the schema's render form (R-66), references and nulls
    /// resolved.
    pub(super) fn desired_doc(&self, ctx: &Ctx, r: &Resource) -> Result<Json> {
        let attrs = self
            .schema()
            .render(&r.addr.typ, &r.attrs)
            .map_err(|(path, why)| {
                crate::report::Failure::located(&r.addr, format!("{path} {why}"))
            })?;
        self.resolve_value(ctx, &attrs)
    }

    fn resolve_value(&self, ctx: &Ctx, v: &Value) -> Result<Json> {
        Ok(match v {
            Value::Str(s) => json!(s),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f.get()),
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
            Value::Range(r) => json!(r.to_string()),
            Value::Ref { typ, name, attr } => self.resolve_ref(ctx, typ, name, attr)?,
            Value::CloudRef { typ, name, attr } => self.resolve_cloud_ref(typ, name, attr)?,
            Value::Null { label, class, .. } => self.resolve_null(ctx, label, *class)?,
            // Where the schema renders one, it already has (`render`).
            // A uri's host in its A-labels: the provider boundary (R-134).
            Value::Quantity(_)
            | Value::Time(_)
            | Value::Uri(_)
            | Value::Oci(_)
            | Value::Semver(_) => {
                json!(v.wire_text())
            }
        })
    }

    /// The world's side of the comparison: configured attributes, plus the
    /// value the provider picked for an Optional+Computed path the program now
    /// sets.
    fn world_doc(&self, typ: &str, cur: &Object, desired: &Json) -> Json {
        let mut doc = cur.attrs.clone();
        for (attr, _) in self.schema().optional_computed_of(typ) {
            if get_path(desired, &attr).is_some()
                && get_path(&doc, &attr).is_none()
                && let Some(v) = get_path(&cur.computed, &attr)
            {
                set_path(&mut doc, &attr, v.clone());
            }
        }
        doc
    }

    /// The remote names of a create-first replacement of `addr` whose
    /// type's remote name is its provider's (`type_remote_name`, R-189):
    /// the old object's and the one the replacement is given, the next
    /// generation of the program's, when the program's `want` would
    /// collide with the old object's (`was`, its world document, or what
    /// state recorded dform gave it). None when it would not, or the type
    /// has none.
    pub(super) fn renamed(
        &self,
        addr: &Address,
        want: &Json,
        was: Option<&Json>,
        entry: Option<&StateEntry>,
    ) -> Option<(String, String)> {
        let path = self.schema().remote_name_of(&addr.typ)?;
        let want = get_path(want, path)?.as_str()?;
        let generated = entry.and_then(|e| e.name.as_deref());
        let was = was
            .and_then(|d| get_path(d, path))
            .and_then(Json::as_str)
            .or(generated)
            .unwrap_or(want);
        let new = state::next_name(want, was, generated)?;
        Some((was.to_string(), new))
    }

    /// The name state recorded dform gave `addr`'s object, when `doc`
    /// keeps it (`StateEntry::name`): a destroy-first replacement of one
    /// made under a generation is made under it again, and an update
    /// keeps it; one where the program writes another name drops it.
    pub(super) fn kept_name(&self, addr: &Address, doc: &Json, state: &State) -> Option<String> {
        let path = self.schema().remote_name_of(&addr.typ)?;
        let sent = get_path(doc, path)?.as_str()?;
        state
            .get(addr)
            .and_then(|e| e.name.clone())
            .filter(|n| n == sent)
    }

    /// Plan: refresh, then the Z-set `desired − world` (`zset::deformation`),
    /// then the owning provider's Plan (the diff, and replace when a
    /// `force_new` path changes) for each deformation. Actions come in
    /// dependency order; deletes last, in reverse dependency order (from
    /// the dependencies state recorded), with the objects deposed by a
    /// create-first replacement among them.
    pub fn plan(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
    ) -> Result<Plan> {
        self.plan_retracting(desired, adopts, lifecycle, state, &BTreeSet::new())
    }

    /// `plan`, leaving every null owned by a `retracted` address
    /// unresolved: those addresses are being replaced (`executor`).
    pub fn plan_retracting(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
        retracted: &BTreeSet<Address>,
    ) -> Result<Plan> {
        let mut p = Planning::new(self, lifecycle, state, adopts)?;
        p.resolve(desired, retracted)?;
        p.read_before(retracted);
        let deformations = p.deformations();
        let mut asks = p.asks(&deformations)?;
        let deleting = p.deleting(&mut asks);
        let mut answers = self.plan_many(asks)?.into_iter();
        let mut actions = Vec::new();
        for (addr, answer) in p.order.clone().into_iter().zip(answers.by_ref()) {
            let d = &deformations[&addr];
            actions.push(p.action(addr, d, answer)?);
        }
        actions.extend(deletes(deleting, answers));
        Ok(Plan { actions })
    }

    /// A document in the canonical form the Z-set compares: leaf path to
    /// leaf value, keyed lists by key, sets by content (`provider::flatten`),
    /// null and secret markers as labeled nulls of the schema's class. A
    /// secret held by another stack's object is where it is held and its
    /// digest: what the reader knows of it, definite, so a changed one
    /// updates and an unchanged one is no change.
    fn flat_value(&self, typ: &str, doc: &Json) -> Value {
        let mut leaves = BTreeMap::new();
        // A set's elements by content: one added beside the world's is
        // an element more, whatever place it sorts to (R-158).
        provider::flatten(self.schema(), typ, doc, "", "", true, &mut leaves);
        Value::Obj(
            leaves
                .into_iter()
                .map(|(p, (v, _))| {
                    let v = match provider::marker(&v) {
                        Some((provider::NULL_KEY, label)) => Value::Null {
                            label: label.to_string(),
                            class: self.null_class(label),
                            ty: String::new(),
                        },
                        Some((_, label)) => match provider::held(&v) {
                            Some(h) => Value::Str(format!(
                                "(sensitive {label} held by {} {} {}/{}#{} {})",
                                h.provider, h.deployment, h.typ, h.remote, h.path, h.digest
                            )),
                            None => Value::Null {
                                label: label.to_string(),
                                class: NullClass::Secret,
                                ty: String::new(),
                            },
                        },
                        None => provider::json_to_value(&v),
                    };
                    (p, v)
                })
                .collect(),
        )
    }

    /// Where each secret another deployment holds that `r`'s document
    /// reads is held ([`Config::held`](super::Config::held)): its provider
    /// reads it there, inside `r`'s Apply.
    pub fn held_in(&self, r: &Resource) -> Vec<provider::Held> {
        let held = self.held.borrow();
        crate::lattice::nulls_in(&r.attrs)
            .iter()
            .filter_map(|l| held.get(l).cloned())
            .collect()
    }

    /// Whether the null labeled `label` is a secret a provider holds,
    /// which no evaluation fills (R-218): a resource's sensitive computed
    /// value, an extern's secret column, another stack's held output.
    pub fn holds_secret(&self, label: &str) -> bool {
        self.held.borrow().contains_key(label) || self.null_class(label) == NullClass::Secret
    }

    /// The class of the null labeled `T/A#P`: the schema's, else open (a
    /// ref to a configured attribute nobody set).
    fn null_class(&self, label: &str) -> NullClass {
        let Some((typ, _, path)) = crate::value::null_parts(label) else {
            return NullClass::Open;
        };
        self.schema()
            .class_of(&typ, &path)
            .or_else(|| self.schema().optional_computed_class(&typ, &path))
            .unwrap_or(NullClass::Open)
    }
}

/// What a ref resolves against: the world (refreshed at plan, live at
/// apply), the identity mapping, and the desired documents resolved so far
/// in dependency order.
pub(super) struct Ctx<'a> {
    pub(super) cloud: &'a Providers,
    pub(super) world: &'a World,
    pub(super) state: &'a State,
    pub(super) adopts: &'a BTreeMap<Address, String>,
    pub(super) resolved: &'a BTreeMap<Address, Json>,
    /// Apply: a null that cannot be filled is an error naming the resource.
    pub(super) strict: Option<&'a Address>,
    /// Plan: addresses being replaced, whose nulls stay unresolved (the
    /// replacement is a new object).
    pub(super) retracted: &'a BTreeSet<Address>,
    /// Plan: those of them replaced create-first under a name dform gives
    /// (R-189): a reference to the name is unknown until the replacement
    /// is made, as a null of theirs is.
    pub(super) renaming: &'a BTreeSet<Address>,
}

impl Ctx<'_> {
    /// The object behind an address, through the identity mapping or an
    /// adopt (Imported: it may not be the stack's yet).
    fn existing(&self, addr: &Address) -> Result<Option<Object>> {
        if let Some(e) = self.state.get(addr) {
            return Ok(self.world.get(&key(&addr.typ, &e.remote)).cloned());
        }
        let Some(rn) = self.adopts.get(addr) else {
            return Ok(None);
        };
        match self.world.get(&key(&addr.typ, rn)) {
            Some(o) => Ok(Some(o.clone())),
            None => self.cloud.import(&addr.typ, rn),
        }
    }
}

/// A plan being made ([`Providers::plan_retracting`]): the refresh it is
/// made against, every desired document resolved and their dependency
/// order, the world's document of each object state maps (`before`),
/// what an update keeps as it was created, and the creates that adopt.
struct Planning<'a> {
    cloud: &'a Providers,
    lifecycle: &'a Lifecycle,
    state: &'a State,
    world: World,
    adopt_map: BTreeMap<Address, String>,
    resolved: BTreeMap<Address, Json>,
    order: Vec<Address>,
    before: BTreeMap<Address, Json>,
    kept: BTreeMap<Address, Vec<Change>>,
    adopting: BTreeSet<Address>,
}

/// A delete a plan asks for: its kind, the address, and what it depends
/// on as state recorded it.
type Deleting<'a> = (ActionKind, Address, &'a [String]);

impl<'a> Planning<'a> {
    /// A plan of `cloud` against its refresh of `state`, `adopts` the
    /// objects the program adopts.
    fn new(
        cloud: &'a Providers,
        lifecycle: &'a Lifecycle,
        state: &'a State,
        adopts: &[Adopt],
    ) -> Result<Planning<'a>> {
        let world = cloud.refresh(state)?;
        let adopt_map = state::adopt_map(adopts);
        cloud.proven.borrow_mut().clear();
        Ok(Planning {
            cloud,
            lifecycle,
            state,
            world,
            adopt_map,
            resolved: BTreeMap::new(),
            order: Vec::new(),
            before: BTreeMap::new(),
            kept: BTreeMap::new(),
            adopting: BTreeSet::new(),
        })
    }

    /// The desired documents, in dependency order: refs and nulls
    /// resolved as far as the world allows, a `retracted` address's nulls
    /// left unresolved (the create-first ones among them renamed).
    fn resolve(&mut self, desired: &[Resource], retracted: &BTreeSet<Address>) -> Result<()> {
        let schema = self.cloud.schema();
        let renaming: BTreeSet<Address> = retracted
            .iter()
            .filter(|a| {
                schema.remote_name_of(&a.typ).is_some() && self.lifecycle.create_first(schema, a)
            })
            .cloned()
            .collect();
        for r in topo_sort(desired)? {
            let ctx = Ctx {
                cloud: self.cloud,
                world: &self.world,
                state: self.state,
                adopts: &self.adopt_map,
                resolved: &self.resolved,
                strict: None,
                retracted,
                renaming: &renaming,
            };
            self.cloud.check_held("plan", &r.addr, "", &r.attrs)?;
            let doc = self.cloud.desired_doc(&ctx, &r)?;
            self.cloud.check_required(&r.addr, &doc)?;
            self.order.push(r.addr.clone());
            self.resolved.insert(r.addr.clone(), doc);
        }
        Ok(())
    }

    /// The world's document of each object state maps, through the
    /// identity mapping, as it compares with the desired one. A state
    /// entry whose object is gone is no row (a desired one is created
    /// again); one no longer desired is deleted from state with nothing
    /// to delete. A held secret's holders: a `retracted` object is made
    /// anew, so what was sent with its secret differs.
    fn read_before(&mut self, retracted: &BTreeSet<Address>) {
        let cloud = self.cloud;
        let holders = Holders::of(self.state, Some(&self.world)).without(retracted);
        for (addr, entry) in cloud.entries(&self.state.resources) {
            let Some(cur) = self.world.get(&key(&addr.typ, &entry.remote)) else {
                continue;
            };
            let mut doc = match self.resolved.get(&addr) {
                Some(d) => cloud.world_doc(&addr.typ, cur, d),
                None => cur.attrs.clone(),
            };
            let mut notes = BTreeMap::new();
            if let Some(want) = self.resolved.get(&addr) {
                notes = cloud.written_before(&addr.typ, entry, want, &mut doc, &holders);
                cloud.derived_before(&addr, entry, want, &mut doc);
            }
            if let Some(want) = self.resolved.get_mut(&addr) {
                let k = cloud.at_create(&addr, self.lifecycle, want, &mut doc, &notes);
                if !k.is_empty() {
                    self.kept.insert(addr.clone(), k);
                }
            }
            self.before.insert(addr, doc);
        }
    }

    /// The Z-set `desired − world` (`zset::deformation`), by address.
    fn deformations(&self) -> BTreeMap<Address, zset::Deformation> {
        let flat = |docs: &BTreeMap<Address, Json>| {
            docs.iter()
                .map(|(a, d)| (a.clone(), self.cloud.flat_value(&a.typ, d)))
                .collect::<BTreeMap<Address, Value>>()
        };
        zset::deformation(&flat(&self.resolved), &flat(&self.before))
            .into_iter()
            .map(|d| (d.addr.clone(), d))
            .collect()
    }

    /// What each desired document asks its provider to Plan, in order:
    /// against the world's (an adopt's against the object Import finds;
    /// the provider validates every desired document, undeformed ones
    /// too).
    fn asks(&mut self, deformations: &BTreeMap<Address, zset::Deformation>) -> Result<Vec<Ask>> {
        let mut asks = Vec::new();
        for addr in &self.order {
            let (remote, prior) = match (deformations[addr].kind, self.adopt_map.get(addr)) {
                (zset::Kind::Create, Some(remote_name)) if self.state.get(addr).is_none() => {
                    let Some(found) = self.cloud.import(&addr.typ, remote_name)? else {
                        bail!(
                            "adopt requested but inventory missing {}",
                            key(&addr.typ, remote_name)
                        );
                    };
                    self.adopting.insert(addr.clone());
                    (remote_name.clone(), Some(found.attrs))
                }
                (zset::Kind::Create, _) => (String::new(), None),
                _ => (
                    self.state
                        .get(addr)
                        .map(|e| e.remote.clone())
                        .unwrap_or_default(),
                    self.before.get(addr).cloned(),
                ),
            };
            asks.push((
                addr.clone(),
                remote,
                prior,
                self.resolved.get(addr).cloned(),
            ));
        }
        Ok(asks)
    }

    /// The deletes, each asked of its provider after `asks`: what is no
    /// longer desired, what state maps to a vanished object (no world
    /// document: nothing to show), and deposed objects.
    fn deleting(&self, asks: &mut Vec<Ask>) -> Vec<Deleting<'a>> {
        let (cloud, state) = (self.cloud, self.state);
        let mut deleting = Vec::new();
        for (addr, entry) in cloud.entries(&state.resources) {
            if self.resolved.contains_key(&addr) {
                continue;
            }
            asks.push((
                addr.clone(),
                entry.remote.clone(),
                self.before.get(&addr).cloned(),
                None,
            ));
            deleting.push((ActionKind::Delete, addr, entry.deps.as_slice()));
        }
        for (addr, entry) in cloud.entries(&state.deposed) {
            let prior = self.world.get(&key(&addr.typ, &entry.remote));
            asks.push((
                addr.clone(),
                entry.remote.clone(),
                prior.map(|o| o.attrs.clone()),
                None,
            ));
            deleting.push((ActionKind::DeleteDeposed, addr, entry.deps.as_slice()));
        }
        deleting
    }

    /// The action of the desired `addr`, deformed as `d`, as its
    /// provider's Plan `planned` it.
    fn action(&mut self, addr: Address, d: &zset::Deformation, planned: Planned) -> Result<Action> {
        let Planned {
            changes,
            replaces,
            reason,
        } = planned;
        let kind = self.kind(&addr, d, &changes, replaces);
        let (changes, on) = match d.kind {
            zset::Kind::Pending => (changes, d.unresolved.clone()),
            zset::Kind::Undeformed => (Vec::new(), BTreeSet::new()),
            _ => (changes, BTreeSet::new()),
        };
        let renamed = match kind {
            ActionKind::Replace { create_first: true } => {
                if let Some(why) = zset::create_first_refused(self.cloud.schema(), &addr) {
                    return Err(crate::report::Failure::located(&addr, why).into());
                }
                let want = self
                    .resolved
                    .get(&addr)
                    .expect("a desired address's document is resolved before its action");
                self.cloud
                    .renamed(&addr, want, self.before.get(&addr), self.state.get(&addr))
            }
            _ => None,
        };
        let kept = self.kept.remove(&addr).unwrap_or_default();
        // Said of a replace only: beside no replace, a reason explains
        // nothing.
        let reason = reason.filter(|_| matches!(kind, ActionKind::Replace { .. }));
        // An ordinary list's edit script (the ticket's R-111 round 2),
        // from the full documents this plan already resolved: a replace
        // sends its unchanged values too (`sent`), so it compares the
        // same way an update does.
        let list_edits = match (
            matches!(
                kind,
                ActionKind::Update
                    | ActionKind::Drift
                    | ActionKind::Pending
                    | ActionKind::Replace { .. }
            ),
            self.before.get(&addr),
            self.resolved.get(&addr),
        ) {
            (true, Some(before), Some(after)) => {
                provider::list_diffs(self.cloud.schema(), &addr.typ, before, after)
            }
            _ => Default::default(),
        };
        Ok(Action {
            kind,
            addr,
            changes,
            on,
            kept,
            renamed,
            reason,
            list_edits,
        })
    }

    /// The kind of the action of the desired `addr`, deformed as `d`.
    fn kind(
        &self,
        addr: &Address,
        d: &zset::Deformation,
        changes: &[Change],
        replaces: bool,
    ) -> ActionKind {
        match d.kind {
            zset::Kind::Create if self.adopting.contains(addr) => ActionKind::Adopt,
            zset::Kind::Create => ActionKind::Create,
            zset::Kind::Delete => unreachable!("a desired address is never deleted"),
            zset::Kind::Update | zset::Kind::Drift if replaces => ActionKind::Replace {
                create_first: self.lifecycle.create_first(self.cloud.schema(), addr),
            },
            zset::Kind::Drift => ActionKind::Drift,
            // The provider's Plan finds nothing to change: the world
            // document differs only where the provider compares for
            // itself (a write-only attribute its API never answers,
            // such as an OVH instance's user data).
            zset::Kind::Update if changes.is_empty() => ActionKind::Noop,
            zset::Kind::Update => ActionKind::Update,
            zset::Kind::Pending => ActionKind::Pending,
            zset::Kind::Undeformed => ActionKind::Noop,
        }
    }
}

/// The delete actions of `deleting`, their Plans `answers`: one goes
/// before the deletes of what it depends on.
fn deletes(deleting: Vec<Deleting<'_>>, answers: impl Iterator<Item = Planned>) -> Vec<Action> {
    let mut deletes: Vec<(Action, &[String])> = deleting
        .into_iter()
        .zip(answers)
        .map(|((kind, addr, deps), planned)| (Action::new(kind, addr, planned.changes), deps))
        .collect();
    deletes.sort_by(|a, b| a.0.addr.cmp(&b.0.addr));
    reverse_dependency_order(deletes)
}

/// Deletes in reverse dependency order: an object goes before every object
/// at an address it depended on. Ties keep address order.
fn reverse_dependency_order(mut deletes: Vec<(Action, &[String])>) -> Vec<Action> {
    let mut out = Vec::new();
    while !deletes.is_empty() {
        // Ready: nothing still to be deleted depends on it.
        let ready = (0..deletes.len())
            .find(|&i| {
                let k = state::key(&deletes[i].0.addr);
                !deletes
                    .iter()
                    .enumerate()
                    .any(|(j, (_, deps))| j != i && deps.contains(&k))
            })
            // A cycle cannot come from a DAG; break it in address order.
            .unwrap_or(0);
        out.push(deletes.remove(ready).0);
    }
    out
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
            let r = by_addr
                .get(&a)
                .expect("pending holds the desired addresses");
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
