//! The attribute aggregate inside the evaluator (E §2.5): the `arg/5` contributions, where
//! each came from, grouped by `(T, A, P)` as they arrive, each group collapsed once the
//! strata that can contribute to it have run; the lattices and refinements it reads.

use super::LATTICE_DECLS;
use super::builtins::eval_term;
use super::collapse::collapse_group;
use super::errors::with_place;
use super::provenance::Prov;
use crate::ast::{Atom, Span, Term, str_term};
use crate::circuit::NodeId;
use crate::ir::ops;
use crate::ir::store::{Store, TupleId, Window};
use crate::lattice::{Lattice, Rank};
use crate::partition::{self, Node};
use crate::spell;
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A contribution to the attribute aggregate: `arg/5`.
pub(super) fn is_contribution(a: &Atom) -> bool {
    a.pred == "arg" && a.args.len() == 5
}

/// One place a contribution came from.
#[derive(Debug, Clone, Copy)]
pub(super) enum Origin {
    /// The program states it, here.
    Fact(Span),
    /// Rule `i` derived it.
    Rule(usize),
}

/// Where each contribution came from: the text of every rule that derived
/// it, or of the fact, with where it is written. Spelled out only for a
/// witness the aggregate names.
#[derive(Default, Clone)]
pub(super) struct Origins {
    by: HashMap<TupleId, Vec<Origin>>,
    /// Each rule's text with its place.
    pub(super) rules: Vec<String>,
}

impl Origins {
    pub(super) fn note(&mut self, t: TupleId, o: Origin) {
        self.by.entry(t).or_default().push(o);
    }

    /// The origins of tuple `t` (which is `a`), sorted.
    pub(super) fn of(&self, t: TupleId, a: &Atom) -> Vec<String> {
        let texts: BTreeSet<String> = self
            .by
            .get(&t)
            .into_iter()
            .flatten()
            .map(|o| match o {
                Origin::Fact(span) => with_place(spell::atom(a), *span),
                Origin::Rule(i) => self.rules[*i].clone(),
            })
            .collect();
        texts.into_iter().collect()
    }
}

/// One attribute group `(T, A, P)`: the type, the address, the normalized path.
pub(super) type GroupKey = (String, Value, String);

/// One contribution to a group: the `arg/5` tuple, its rank, and its value
/// after path normalization.
pub(super) type Contribution = (TupleId, Rank, Value);

/// An element write to a group (R-35): the `arg/5` tuple, its rank, the
/// keyed list's path, the key and the content.
pub(super) type ElemContribution = (TupleId, Rank, String, Value, Value);

/// An element write's list path and key.
type ElemOf = Option<(String, Value)>;

/// The attribute aggregate of E §2.5 inside the evaluator: `attr/4`,
/// `attr_conflict/5` and `attr_stuck/4` from the `arg/5` contributions, one
/// ranked lattice cell per group, collapsed with F's shadow-aware rule
/// (DR-9 revised). A group is collapsed once, as soon as every partition
/// node that can contribute to it is in a lower stratum; the stratifier
/// puts every reader of the group above that.
///
/// Contributions are grouped as they arrive (the `arg/5` tuples inserted
/// since the last collapse), so a stratum costs the groups it collapses,
/// not a pass over every fact.
#[derive(Clone)]
pub(super) struct AttrAggregate {
    arg_nodes: Vec<(Node, usize)>,
    /// The types partitioned by address too (`partition::Options::split`):
    /// a group of one is complete per address.
    split: BTreeSet<String>,
    /// `ready_at` per `T`, then `P` (`A\0P` for a type in `split`).
    ready: HashMap<String, HashMap<String, usize>>,
    /// `arg/5` tuples below this id are grouped.
    grouped: TupleId,
    lattices: Option<BTreeMap<(String, String), Lattice>>,
    /// The checkable refinements the engine checks: every `type_refine` and
    /// `attr_refine` fact but those on a `sensitive` path (the provider's).
    refinements: Option<Vec<(TupleId, crate::refine::Stated)>>,
    pending: HashMap<GroupKey, Vec<Contribution>>,
    /// The element writes per group, beside its other contributions.
    elems: HashMap<GroupKey, Vec<ElemContribution>>,
    /// The pending groups by the stratum they are complete at.
    waiting: BTreeMap<usize, BTreeSet<GroupKey>>,
    /// The groups whose base (`transform::ATTR_BASE`) a rule reads, by the
    /// stratum the base is complete at: every contribution but the element
    /// writes.
    waiting_base: BTreeMap<usize, BTreeSet<GroupKey>>,
    base_nodes: Vec<Node>,
    emitted_base: BTreeSet<GroupKey>,
    /// The lattices declared below a type's top attributes, by path.
    nested: HashMap<String, std::rc::Rc<BTreeMap<String, Lattice>>>,
    emitted: BTreeSet<GroupKey>,
    /// Groups that gained a contribution after they were collapsed.
    late: BTreeSet<GroupKey>,
}

impl AttrAggregate {
    pub(super) fn new(strata: &BTreeMap<Node, usize>, split: &BTreeSet<String>) -> Self {
        let arg_nodes = strata
            .iter()
            .filter(|(n, _)| n.pred == "arg")
            .map(|(n, s)| (n.clone(), *s))
            .collect();
        let base_nodes = strata
            .keys()
            .filter(|n| n.pred == transform::ATTR_BASE)
            .cloned()
            .collect();
        AttrAggregate {
            arg_nodes,
            split: split.clone(),
            ready: HashMap::new(),
            grouped: 0,
            lattices: None,
            refinements: None,
            pending: HashMap::new(),
            elems: HashMap::new(),
            waiting: BTreeMap::new(),
            waiting_base: BTreeMap::new(),
            base_nodes,
            emitted_base: BTreeSet::new(),
            nested: HashMap::new(),
            emitted: BTreeSet::new(),
            late: BTreeSet::new(),
        }
    }

    /// The first stratum at which group `(typ, addr, path)` is complete.
    fn ready_at(&mut self, typ: &str, addr: &Value, path: &str) -> usize {
        let addr = match (self.split.contains(typ), addr) {
            (true, Value::Str(a)) => Some(a.as_str()),
            _ => None,
        };
        let key = match addr {
            Some(a) => format!("{a}\0{path}"),
            None => path.to_string(),
        };
        if let Some(s) = self.ready.get(typ).and_then(|p| p.get(&key)) {
            return *s;
        }
        let node = Node {
            pred: "arg".into(),
            typ: Some(typ.into()),
            path: Some(path.into()),
            addr: addr.map(partition::Addr::exact),
        };
        let s = self
            .arg_nodes
            .iter()
            .filter(|(n, _)| n.unifies(&node))
            .map(|(_, s)| s + 1)
            .max()
            .unwrap_or(0);
        self.ready
            .entry(typ.to_string())
            .or_default()
            .insert(key, s);
        s
    }

    /// Group the contributions inserted since the last call, in fact order.
    fn group_new(&mut self, store: &Store) -> Result<()> {
        let rel = ops::Rel {
            pred: "arg".into(),
            arity: 5,
        };
        let w = Window {
            lo: self.grouped,
            hi: store.len(),
        };
        self.grouped = store.len();
        let mut new: Vec<TupleId> = store.ids(&rel, w).to_vec();
        new.sort_by(|a, b| store.get(*a).cmp(store.get(*b)));
        for t in new {
            let (key, rank, value, elem) = contribution(store.get(t))?;
            if self.emitted.contains(&key) || (elem.is_none() && self.emitted_base.contains(&key)) {
                self.late.insert(key);
            } else {
                if !self.pending.contains_key(&key) && !self.elems.contains_key(&key) {
                    // An input, `let` or output cell is partitioned by its
                    // scope too (`partition::type_path_node`): a module's
                    // input `k` of the copy `m` is the node `m::k`.
                    let path = match key.1.as_str() {
                        Some(scope)
                            if !scope.is_empty() && crate::partition::scoped_cell(&key.0) =>
                        {
                            format!("{scope}::{}", key.2)
                        }
                        _ => key.2.clone(),
                    };
                    let base = self.ready_at(&key.0, &key.1, &path);
                    let elems =
                        self.ready_at(&key.0, &key.1, &format!("{path}{}", transform::ELEM));
                    self.waiting
                        .entry(base.max(elems))
                        .or_default()
                        .insert(key.clone());
                    let read = Node {
                        pred: transform::ATTR_BASE.into(),
                        addr: match (self.split.contains(&key.0), &key.1) {
                            (true, Value::Str(a)) => Some(partition::Addr::exact(a)),
                            _ => None,
                        },
                        typ: Some(key.0.clone()),
                        path: Some(path),
                    };
                    if self.base_nodes.iter().any(|n| n.unifies(&read)) {
                        self.waiting_base
                            .entry(base)
                            .or_default()
                            .insert(key.clone());
                    }
                }
                match elem {
                    Some((list, k)) => {
                        let v = (t, rank, list, k, value);
                        self.elems.entry(key).or_default().push(v);
                    }
                    None => self.pending.entry(key).or_default().push((t, rank, value)),
                }
            }
        }
        Ok(())
    }

    /// The lattices declared below `typ`'s top attributes, by path.
    fn nested(&mut self, typ: &str) -> std::rc::Rc<BTreeMap<String, Lattice>> {
        let lattices = self.lattices.as_ref();
        let n = self.nested.entry(typ.to_string()).or_insert_with(|| {
            let m = lattices
                .into_iter()
                .flat_map(|l| l.range((typ.to_string(), String::new())..))
                .take_while(|((t, _), _)| t == typ)
                .filter(|((_, p), _)| p.contains('.'))
                .map(|((_, p), l)| (p.clone(), l.clone()))
                .collect();
            std::rc::Rc::new(m)
        });
        n.clone()
    }

    /// Collapse every complete group. Rule 3 per key: a group that a stuck
    /// contribution could still join is undetermined and is not collapsed;
    /// it and a Stuck cell are returned as stuck heads `attr(T, A, P, _)`,
    /// so their readers are undetermined too.
    pub(super) fn emit_ready(
        &mut self,
        stratum: usize,
        prov: &mut Prov,
        origins: &Origins,
        known: &stuck::Known,
        sigma: NodeId,
    ) -> Result<Vec<Stuck>> {
        if self.lattices.is_none() {
            self.lattices = Some(declared_lattices(prov.store.atoms())?);
        }
        if self.refinements.is_none() {
            self.refinements = Some(declared_refinements(&prov.store)?);
        }
        self.group_new(&prov.store)?;
        // The groups complete at this stratum, in group order.
        let next = stratum.checked_add(1);
        let take = |w: &mut BTreeMap<usize, BTreeSet<GroupKey>>| -> BTreeSet<GroupKey> {
            let later = next.map(|n| w.split_off(&n)).unwrap_or_default();
            std::mem::replace(w, later)
                .into_values()
                .flatten()
                .collect()
        };
        let bases = take(&mut self.waiting_base);
        let keys = take(&mut self.waiting);
        let mut out = Vec::new();
        let mut stuck_groups = Vec::new();
        // Bases first: a group's base is complete no later than the group.
        let groups = bases
            .into_iter()
            .map(|k| (k, true))
            .chain(keys.into_iter().map(|k| (k, false)));
        for (key, base) in groups {
            let mut contribs = match base {
                true => self.pending.get(&key).cloned().unwrap_or_default(),
                false => self.pending.remove(&key).unwrap_or_default(),
            };
            contribs.sort_by(|a, b| prov.store.get(a.0).cmp(prov.store.get(b.0)));
            let mut elems = match base {
                true => vec![],
                false => self.elems.remove(&key).unwrap_or_default(),
            };
            elems.sort_by(|a, b| prov.store.get(a.0).cmp(prov.store.get(b.0)));
            let (typ, addr, path) = &key;
            let pred = if base { transform::ATTR_BASE } else { "attr" };
            let read = Atom {
                pred: pred.into(),
                args: vec![
                    str_term(typ),
                    Term::Val(addr.clone()),
                    str_term(path),
                    Term::Wildcard,
                ],
                record: None,
                span: Default::default(),
            };
            let group_stuck = |nulls: BTreeSet<String>, reason: String| Stuck {
                rule: None,
                head: read.clone(),
                bindings: BTreeMap::new(),
                nulls,
                reason,
                text: format!("attr({typ}, {}, {path}, _)", spell::value(addr)),
            };
            let any = Atom {
                pred: "attr".into(),
                ..read.clone()
            };
            if known.any(&any) {
                stuck_groups.push(group_stuck(
                    known.blocking(&any),
                    "a stuck rule instance may still contribute to this attribute".into(),
                ));
            } else {
                let lat = self
                    .lattices
                    .as_ref()
                    .and_then(|l| l.get(&(typ.clone(), path.clone())))
                    .cloned()
                    .unwrap_or_else(|| infer_lattice(&contribs));
                // An element is written by its key: the list declares one.
                for (t, _, list, _, _) in &elems {
                    let keyed = self
                        .lattices
                        .as_ref()
                        .and_then(|l| l.get(&(typ.clone(), list.clone())));
                    if !matches!(keyed, Some(Lattice::Keyed { .. })) {
                        let a = prov.store.get(*t);
                        bail!(
                            "resource {typ}[{}]: {list} is not a keyed list, so an element of it \
                             is not written by its key; declare the field that names an element, \
                             type_list_key({typ}, \"{list}\", [\"FIELD\"]), or write the whole \
                             list\n  in: {}",
                            spell::value(addr),
                            origins.of(*t, a).join("; ")
                        );
                    }
                }
                let nested = self.nested(typ);
                let refs: Vec<&(TupleId, crate::refine::Stated)> = match base {
                    true => vec![],
                    false => self
                        .refinements
                        .iter()
                        .flatten()
                        .filter(|(_, r)| r.applies(typ, addr, path))
                        .collect(),
                };
                let mut cell = collapse_group(
                    &key,
                    &contribs,
                    &elems,
                    &refs,
                    &lat,
                    &nested,
                    origins,
                    &prov.store,
                );
                if base {
                    // The base's value, or nothing: its conflicts and
                    // warnings are the group's, said once.
                    cell.retain(|a| a.pred == "attr" || a.pred == "attr_stuck");
                    for a in cell.iter_mut().filter(|a| a.pred == "attr") {
                        a.pred = transform::ATTR_BASE.into();
                    }
                }
                for a in &cell {
                    if a.pred == "attr_stuck"
                        && let Some(Term::Val(Value::List(ls))) = a.args.get(3)
                    {
                        let nulls = ls.iter().filter_map(|l| l.as_str().map(str::to_string));
                        stuck_groups.push(group_stuck(
                            nulls.collect(),
                            "contributions disagree until a null resolves".into(),
                        ));
                    }
                }
                // Σ over the group: every contribution that reached it,
                // and every refinement joined into it.
                let children: Vec<NodeId> = std::iter::once(sigma)
                    .chain(contribs.iter().map(|(t, _, _)| prov.id(*t)))
                    .chain(elems.iter().map(|(t, ..)| prov.id(*t)))
                    .chain(refs.iter().map(|(t, _)| prov.id(*t)))
                    .collect();
                for a in cell
                    .into_iter()
                    .filter(|a| !(base && a.pred == "attr_stuck"))
                {
                    out.push((a, children.clone()));
                }
            }
            match base {
                true => self.emitted_base.insert(key),
                false => self.emitted.insert(key),
            };
        }
        for (a, children) in out {
            prov.record(a, children, vec![]);
        }
        Ok(stuck_groups)
    }

    /// Guard on the stratifier: no contribution arrived after its group was
    /// collapsed.
    pub(super) fn check_complete(&self) -> Result<()> {
        if let Some(key) = self.late.iter().next() {
            bail!(
                "internal: attribute {} {} {} gained a contribution after it was collapsed",
                key.0,
                spell::value(&key.1),
                key.2
            );
        }
        Ok(())
    }
}

fn parse_rank(v: &Value) -> Option<Rank> {
    match v.as_str()? {
        "default" => Some(Rank::Default),
        transform::NORMAL => Some(Rank::Normal),
        "override" => Some(Rank::Override),
        _ => None,
    }
}

pub(super) fn rank_name(r: Rank) -> &'static str {
    match r {
        Rank::Default => "default",
        Rank::Normal => transform::NORMAL,
        Rank::Override => "override",
    }
}

/// An `arg/5` contribution's group `(T, A, normalized P)`, rank and value;
/// for an element write (`transform::ELEM`) its list's path and key, and
/// the value is the element's content.
fn contribution(a: &Atom) -> Result<(GroupKey, Rank, Value, ElemOf)> {
    let vals: Vec<&Value> = a
        .args
        .iter()
        .map(|t| match t {
            Term::Val(v) => Ok(v),
            _ => Err(anyhow!("internal: non-ground contribution")),
        })
        .collect::<Result<_>>()?;
    let (Some(typ), Some(path)) = (vals[0].as_str(), vals[2].as_str()) else {
        bail!(
            "contribution {} needs a string type and path",
            spell::atom(a)
        );
    };
    let Some(rank) = parse_rank(vals[4]) else {
        bail!(
            "contribution {}: rank must be default, normal or override",
            spell::atom(a)
        );
    };
    if let Some(list) = path.strip_suffix(transform::ELEM) {
        let Value::List(kv) = vals[3] else {
            bail!("internal: element write {}", spell::atom(a));
        };
        let [k, v] = kv.as_slice() else {
            bail!("internal: element write {}", spell::atom(a));
        };
        let top = list.split('.').next().unwrap_or(list).to_string();
        let group = (typ.to_string(), vals[1].clone(), top);
        return Ok((group, rank, v.clone(), Some((list.to_string(), k.clone()))));
    }
    let (path, value) = transform::normalize_contribution(typ, path, Term::Val(vals[3].clone()));
    let value = eval_term(&value, &HashMap::new()).ok_or_else(|| anyhow!("internal: normalize"))?;
    Ok(((typ.to_string(), vals[1].clone(), path), rank, value, None))
}

/// `type_lattice(T, P, flat|map|set)` and `type_list_key(T, P, Keys)`
/// facts, a keyed list's with its keys' `type_default(T, P.K, V)`, and
/// a set for each `type_attr(T, P, "set(..)", _)` that declares none.
fn declared_lattices(facts: &[Atom]) -> Result<BTreeMap<(String, String), Lattice>> {
    let mut defaults: BTreeMap<(&str, &str), &Value> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "type_default") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(v),
        ] = a.args.as_slice()
        {
            defaults.insert((t, p), v);
        }
    }
    let key_defaults = |t: &str, p: &str, keys: &[String]| -> BTreeMap<String, Value> {
        keys.iter()
            .filter_map(|k| {
                let at = format!("{p}.{k}");
                let d = defaults.get(&(t, at.as_str()))?;
                Some((k.clone(), (*d).clone()))
            })
            .collect()
    };
    let mut decls: Vec<&Atom> = facts
        .iter()
        .filter(|a| LATTICE_DECLS.contains(&a.pred.as_str()))
        .collect();
    decls.sort();
    let mut out = BTreeMap::new();
    for a in decls {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(k),
        ] = a.args.as_slice()
        else {
            bail!("{}/3 expects (Type, Path, ...): {}", a.pred, spell::atom(a));
        };
        let lat = match (a.pred.as_str(), k) {
            ("type_lattice", Value::Str(k)) if k == "flat" => Lattice::Flat,
            ("type_lattice", Value::Str(k)) if k == "map" => Lattice::Map(Box::new(Lattice::Flat)),
            ("type_lattice", Value::Str(k)) if k == "set" => Lattice::Set,
            ("type_list_key", Value::List(ks)) => {
                let keys: Vec<String> = ks.iter().map(crate::functions::value_to_string).collect();
                Lattice::Keyed {
                    defaults: key_defaults(t, p, &keys),
                    keys,
                    elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
                }
            }
            ("type_list_key", Value::Str(k)) => Lattice::Keyed {
                keys: vec![k.clone()],
                defaults: key_defaults(t, p, std::slice::from_ref(k)),
                elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
            },
            _ => bail!("{}: unknown lattice {}", spell::atom(a), spell::value(k)),
        };
        let key = (t.clone(), p.clone());
        if out.get(&key).is_some_and(|l| *l != lat) {
            bail!("path {t} {p} declares two lattices");
        }
        out.insert(key, lat);
    }
    // An attribute the schema types `set(T)` (R-158) is a set unless a
    // lattice is declared on it: its contributions union, so several
    // modules each add an element (`policies`, `peerings`, `routes`).
    for a in facts.iter().filter(|a| a.pred == "type_attr") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(Value::Str(ty)),
            ..,
        ] = a.args.as_slice()
            && ty.split('(').next().is_some_and(|k| k.trim() == "set")
        {
            out.entry((t.clone(), p.clone())).or_insert(Lattice::Set);
        }
    }
    Ok(out)
}

/// `type_refine/3` and `attr_refine/4` facts, but those on a path a
/// `type_attr` fact marks `sensitive` (or below one): the engine never
/// checks a secret; the provider does, from an Apply assertion (F DR-13
/// revised).
fn declared_refinements(store: &Store) -> Result<Vec<(TupleId, crate::refine::Stated)>> {
    let mut sensitive: BTreeSet<(&str, &str)> = BTreeSet::new();
    for a in store.atoms().iter().filter(|a| a.pred == "type_attr") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            _,
            Term::Val(Value::List(flags)),
        ] = a.args.as_slice()
            && flags.iter().any(|f| f.as_str() == Some("sensitive"))
        {
            sensitive.insert((t, p));
        }
    }
    let is_sensitive = |t: &str, p: &str| {
        std::iter::successors(Some(p), |p| p.rsplit_once('.').map(|x| x.0))
            .any(|p| sensitive.contains(&(t, p)))
    };
    let mut out = Vec::new();
    for (t, a) in store.atoms().iter().enumerate() {
        let Some(r) = crate::refine::Stated::of(a) else {
            continue;
        };
        let r = r.map_err(|e| anyhow!("{}: {e}", spell::atom(a)))?;
        if !is_sensitive(&r.typ, &r.path) {
            out.push((t as TupleId, r));
        }
    }
    Ok(out)
}

/// With no declaration, a path whose contributions are all objects is a
/// Map with Flat leaves; anything else is Flat (a list is one value, and
/// two authors of different lists conflict, E DR-1).
fn infer_lattice(contribs: &[Contribution]) -> Lattice {
    if contribs.iter().all(|(_, _, v)| matches!(v, Value::Obj(_))) {
        Lattice::Map(Box::new(Lattice::Flat))
    } else {
        Lattice::Flat
    }
}
