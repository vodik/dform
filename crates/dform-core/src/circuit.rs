//! Minimal absorptive provenance circuit for proposals/E-synthesis.org, seam 2.
//!
//! A circuit is a DAG of three node kinds: leaves (base fact, rule, world
//! event, schema fact, negation witness), fact nodes (one per derived tuple,
//! holding its alternatives), and Times nodes (one per rule firing, whose
//! children are the leaves and fact nodes the firing used). The Plus of a fact
//! is its list of alternatives; absorption keeps that list an antichain.
//!
//! Three homomorphisms are implemented: `why` (sets of witness sets), `phase`
//! ((P(N), ∩, ∪): nulls that must resolve before the fact is definite) and
//! `touches` ((P(N), ∪, ∪): nulls whose resolution may change the fact).
//! Two phase-boundary operations: `resolve` (substitute a null, re-attach the
//! world leaf) and `retract` (deletion propagation, DRed-lite, no SCCs).
//!
//! The evaluator records every fact it inserts here (E DR-10: provenance is
//! always on). A firing also keeps the rule's variable bindings, for `why`.

use crate::ast::{Atom, Term};
use crate::ir::fx::{FxHashMap, FxHashSet};
use crate::lattice::{nulls_in, subst};
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

pub type NodeId = usize;

/// E §3.1: a fact keeps at most this many alternatives; more are dropped
/// and the fact is marked truncated.
pub const MAX_ALTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Leaf {
    /// A fact the program states.
    Base {
        span: String,
    },
    /// A rule, by id; `Circuit::rule_text` has its text. `Σ` ids mark an
    /// aggregate over its group.
    Rule {
        id: String,
    },
    Schema {
        span: String,
    },
    World {
        event: String,
    },
    /// A fact the planner hands back to the program for the policy pass
    /// (`zset::POLICY_INPUTS`): the plan's deformation, as planned at apply
    /// tick `tick` (`None`: by `plan`).
    Plan {
        fact: String,
        tick: Option<usize>,
    },
    /// A fact given on the command line (`--set`, `--data`).
    Input {
        source: String,
    },
    /// A row of an `extern` predicate, as the provider returned it.
    Extern {
        call: String,
    },
    /// A fact a firing found absent (`not p(..)`): the negated atom,
    /// ground but for its wildcards, spelled where it is printed.
    Absent {
        atom: Atom,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fact {
    pub pred: String,
    pub args: Vec<Value>,
}

impl Fact {
    pub fn new(pred: &str, args: Vec<Value>) -> Fact {
        Fact {
            pred: pred.into(),
            args,
        }
    }
    /// The fact as a ground atom (no record, no span): what the printers
    /// format.
    pub fn atom(&self) -> Atom {
        Atom {
            pred: self.pred.clone(),
            args: self.args.iter().cloned().map(Term::Val).collect(),
            record: None,
            span: Default::default(),
        }
    }
    fn nulls(&self) -> BTreeSet<String> {
        self.args.iter().flat_map(nulls_in).collect()
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(Leaf),
    /// A derived (or base) tuple with its alternatives (each a Times node).
    /// `truncated`: a further firing was dropped at `MAX_ALTS`.
    Fact {
        fact: Fact,
        alts: Vec<NodeId>,
        truncated: bool,
    },
    /// One firing: children are leaves and fact nodes.
    Times(Vec<NodeId>),
    Dead,
}

#[derive(Debug, Default, Clone)]
pub struct Circuit {
    nodes: Vec<Node>,
    by_fact: FxHashMap<Fact, NodeId>,
    leaves: BTreeMap<Leaf, NodeId>,
    /// Per Times node: the rule's variable bindings for that firing,
    /// shared by the circuit's copies (a firing over a manifest binds its
    /// documents; the policy pass copies the circuit).
    bindings: BTreeMap<NodeId, std::sync::Arc<[(String, Value)]>>,
    /// Rule id -> rule text.
    rule_text: BTreeMap<String, String>,
    /// Rule id -> where the rule is written (`diag::place`).
    rule_at: BTreeMap<String, String>,
    /// Rule id -> the source it was lowered from, for `why`.
    rule_src: BTreeMap<String, RuleSource>,
    /// Firings already absorbed or truncated, so naive re-evaluation does
    /// not re-test (or re-store) them.
    rejected: FxHashSet<(NodeId, Vec<NodeId>)>,
}

/// The statement a rule was lowered from: its file's text and the byte
/// range of the rule's span in it (the statement, or the block entry the
/// rule was lowered out of), its line, and the pack or module instance it
/// came from.
#[derive(Debug, Clone)]
pub struct RuleSource {
    pub file: String,
    pub text: std::sync::Arc<str>,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub origin: Option<String>,
}

/// A read-only view of one node, for printers.
pub enum View<'a> {
    Leaf(&'a Leaf),
    Fact {
        fact: &'a Fact,
        alts: &'a [NodeId],
        truncated: bool,
    },
    Times {
        children: &'a [NodeId],
        bindings: &'a [(String, Value)],
    },
    Dead,
}

/// Size of the circuit, for DR-10's cost claim.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub leaves: usize,
    pub facts: usize,
    pub times: usize,
    pub dead: usize,
    /// Edges from Times nodes to their children.
    pub children: usize,
    /// Estimated heap and inline bytes of everything the circuit holds.
    pub bytes: usize,
}

impl Circuit {
    pub fn leaf(&mut self, l: Leaf) -> NodeId {
        if let Some(id) = self.leaves.get(&l) {
            return *id;
        }
        self.nodes.push(Node::Leaf(l.clone()));
        let id = self.nodes.len() - 1;
        self.leaves.insert(l, id);
        id
    }

    /// Every leaf, in leaf order.
    pub fn leaves(&self) -> impl Iterator<Item = &Leaf> {
        self.leaves.keys()
    }

    pub fn fact_id(&self, f: &Fact) -> Option<NodeId> {
        self.by_fact.get(f).copied()
    }

    pub fn has(&self, f: &Fact) -> bool {
        self.by_fact.contains_key(f)
    }

    /// Derive `head` by one firing over `children`. If the tuple exists the
    /// firing is ⊕-ed in, with absorption on the leaf sets.
    pub fn derive(&mut self, head: Fact, children: Vec<NodeId>) -> NodeId {
        self.derive_with(head, children, vec![])
    }

    /// `derive`, recording the firing's variable bindings.
    pub fn derive_with(
        &mut self,
        head: Fact,
        children: Vec<NodeId>,
        bindings: Vec<(String, Value)>,
    ) -> NodeId {
        if let Some(&id) = self.by_fact.get(&head) {
            self.fire(id, children, bindings);
            return id;
        }
        let ch = normalize(children);
        let times = self.push_times(ch, bindings);
        self.nodes.push(Node::Fact {
            fact: head.clone(),
            alts: vec![times],
            truncated: false,
        });
        let id = self.nodes.len() - 1;
        self.by_fact.insert(head, id);
        id
    }

    /// ⊕ one more firing into the existing fact node `id`. The same firing
    /// again is a no-op (hash-consing by child set); a firing some existing
    /// alternative absorbs is dropped; alternatives it absorbs are evicted.
    pub fn fire(&mut self, id: NodeId, children: Vec<NodeId>, bindings: Vec<(String, Value)>) {
        let ch = normalize(children);
        let Node::Fact { alts, .. } = &self.nodes[id] else {
            panic!("fire: node {id} is not a fact")
        };
        if alts
            .iter()
            .any(|a| matches!(&self.nodes[*a], Node::Times(c) if *c == ch))
        {
            return;
        }
        let key = (id, ch);
        if self.rejected.contains(&key) {
            return;
        }
        let ch = key.1;
        let new_set = self.why_children(&ch);
        let existing: Vec<(NodeId, BTreeSet<BTreeSet<Leaf>>)> =
            alts.iter().map(|a| (*a, self.why_node(*a))).collect();
        // Absorption: a ⊕ (a ⊗ b) = a. Drop the new firing if some existing
        // alternative's witness set is a subset of one of its own; drop
        // existing alternatives the new one absorbs.
        let absorbed = existing
            .iter()
            .any(|(_, w)| w.iter().any(|e| new_set.iter().all(|n| e.is_subset(n))));
        let keep: Vec<NodeId> = existing
            .iter()
            .filter(|(_, w)| !w.iter().all(|e| new_set.iter().any(|n| n.is_subset(e))))
            .map(|(a, _)| *a)
            .collect();
        if absorbed || keep.len() >= MAX_ALTS {
            if !absorbed && let Node::Fact { truncated, .. } = &mut self.nodes[id] {
                *truncated = true;
            }
            self.rejected.insert((id, ch));
            return;
        }
        let times = self.push_times(ch, bindings);
        let Node::Fact { alts, .. } = &mut self.nodes[id] else {
            unreachable!()
        };
        *alts = keep;
        alts.push(times);
    }

    fn push_times(&mut self, ch: Vec<NodeId>, bindings: Vec<(String, Value)>) -> NodeId {
        self.nodes.push(Node::Times(ch));
        let times = self.nodes.len() - 1;
        if !bindings.is_empty() {
            self.bindings.insert(times, bindings.into());
        }
        times
    }

    /// Name rule `id` with its text, for printers.
    pub fn name_rule(&mut self, id: &str, text: &str) {
        self.rule_text
            .entry(id.to_string())
            .or_insert_with(|| text.to_string());
    }

    pub fn rule_text(&self, id: &str) -> Option<&str> {
        self.rule_text.get(id).map(String::as_str)
    }

    /// Say where rule `id` is written, for printers.
    pub fn locate_rule(&mut self, id: &str, at: String) {
        self.rule_at.insert(id.to_string(), at);
    }

    pub fn rule_at(&self, id: &str) -> Option<&str> {
        self.rule_at.get(id).map(String::as_str)
    }

    /// Record the source rule `id` was lowered from, for printers.
    pub fn source_rule(&mut self, id: &str, src: RuleSource) {
        self.rule_src.insert(id.to_string(), src);
    }

    pub fn rule_source(&self, id: &str) -> Option<&RuleSource> {
        self.rule_src.get(id)
    }

    pub fn view(&self, id: NodeId) -> View<'_> {
        match &self.nodes[id] {
            Node::Leaf(l) => View::Leaf(l),
            Node::Fact {
                fact,
                alts,
                truncated,
            } => View::Fact {
                fact,
                alts,
                truncated: *truncated,
            },
            Node::Times(ch) => View::Times {
                children: ch,
                bindings: self.bindings.get(&id).map(|b| &b[..]).unwrap_or(&[]),
            },
            Node::Dead => View::Dead,
        }
    }

    pub fn stats(&self) -> Stats {
        let mut st = Stats::default();
        let word = std::mem::size_of::<usize>();
        st.bytes += self.nodes.capacity() * std::mem::size_of::<Node>();
        for n in &self.nodes {
            match n {
                Node::Leaf(l) => {
                    st.leaves += 1;
                    // The node and the interning map each hold the text.
                    st.bytes += 2 * leaf_bytes(l) + std::mem::size_of::<Leaf>() + word;
                }
                Node::Fact { fact, alts, .. } => {
                    st.facts += 1;
                    // The node and `by_fact` each hold the tuple.
                    st.bytes += 2 * fact_bytes(fact)
                        + std::mem::size_of::<Fact>()
                        + word
                        + alts.capacity() * word;
                }
                Node::Times(ch) => {
                    st.times += 1;
                    st.children += ch.len();
                    st.bytes += ch.capacity() * word;
                }
                Node::Dead => st.dead += 1,
            }
        }
        for b in self.bindings.values() {
            st.bytes += word
                + b.iter()
                    .map(|(k, v)| k.len() + value_bytes(v) + std::mem::size_of::<(String, Value)>())
                    .sum::<usize>();
        }
        for (k, v) in &self.rule_text {
            st.bytes += k.len() + v.len() + 2 * std::mem::size_of::<String>();
        }
        for (_, ch) in &self.rejected {
            st.bytes += word + ch.capacity() * word + std::mem::size_of::<Vec<NodeId>>();
        }
        st
    }

    /// Why-provenance: sets of leaf sets. ⊕ is union, ⊗ is pairwise union.
    pub fn why(&self, f: &Fact) -> BTreeSet<BTreeSet<Leaf>> {
        match self.by_fact.get(f) {
            Some(id) => self.why_node(*id),
            None => BTreeSet::new(),
        }
    }

    fn why_node(&self, id: NodeId) -> BTreeSet<BTreeSet<Leaf>> {
        match &self.nodes[id] {
            Node::Leaf(l) => BTreeSet::from([BTreeSet::from([l.clone()])]),
            Node::Fact { alts, .. } => alts.iter().flat_map(|a| self.why_node(*a)).collect(),
            Node::Times(ch) => self.why_children(ch),
            Node::Dead => BTreeSet::new(),
        }
    }

    /// ⊗ over a firing's children.
    fn why_children(&self, ch: &[NodeId]) -> BTreeSet<BTreeSet<Leaf>> {
        ch.iter().fold(BTreeSet::from([BTreeSet::new()]), |acc, c| {
            let w = self.why_node(*c);
            let mut out = BTreeSet::new();
            for a in &acc {
                for b in &w {
                    out.insert(a.union(b).cloned().collect());
                }
            }
            out
        })
    }

    /// (P(N), ∩, ∪): nulls that must resolve before this fact is definite.
    /// A fact with one null-free derivation is definite even if it has others.
    pub fn phase(&self, f: &Fact) -> BTreeSet<String> {
        self.by_fact
            .get(f)
            .map(|id| self.phase_node(*id))
            .unwrap_or_default()
    }
    fn phase_node(&self, id: NodeId) -> BTreeSet<String> {
        match &self.nodes[id] {
            Node::Leaf(_) | Node::Dead => BTreeSet::new(),
            Node::Fact { fact, alts, .. } => {
                let mut acc: Option<BTreeSet<String>> = None;
                for a in alts {
                    let Node::Times(ch) = &self.nodes[*a] else {
                        continue;
                    };
                    let s: BTreeSet<String> = ch.iter().flat_map(|c| self.phase_node(*c)).collect();
                    acc = Some(match acc {
                        None => s,
                        Some(prev) => prev.intersection(&s).cloned().collect(),
                    });
                }
                let mut out = acc.unwrap_or_default();
                out.extend(fact.nulls());
                out
            }
            Node::Times(ch) => ch.iter().flat_map(|c| self.phase_node(*c)).collect(),
        }
    }

    /// (P(N), ∪, ∪): nulls whose resolution may change this fact's spelling or
    /// existence. This is the invalidation set for a phase boundary.
    pub fn touches(&self, f: &Fact) -> BTreeSet<String> {
        self.by_fact
            .get(f)
            .map(|id| self.touches_node(*id))
            .unwrap_or_default()
    }
    fn touches_node(&self, id: NodeId) -> BTreeSet<String> {
        match &self.nodes[id] {
            Node::Leaf(_) | Node::Dead => BTreeSet::new(),
            Node::Fact { fact, alts, .. } => {
                let mut out = fact.nulls();
                for a in alts {
                    out.extend(self.touches_node(*a));
                }
                out
            }
            Node::Times(ch) => ch.iter().flat_map(|c| self.touches_node(*c)).collect(),
        }
    }

    /// Phase boundary: `label := value`, justified by `world`. Every fact whose
    /// tuple carries the label is re-spelled in place (same node id, so every
    /// Times that referenced it still does) and every one of its alternatives
    /// gains the world leaf. Returns the re-spelled facts.
    pub fn resolve(&mut self, label: &str, value: &Value, world: Leaf) -> Vec<(Fact, Fact)> {
        let w = self.leaf(world);
        let mut carrying: Vec<(Fact, NodeId)> = self
            .by_fact
            .iter()
            .filter(|(f, _)| f.nulls().contains(label))
            .map(|(f, id)| (f.clone(), *id))
            .collect();
        carrying.sort();
        let mut renamed = Vec::new();
        for (old, id) in carrying {
            let new = Fact {
                pred: old.pred.clone(),
                args: old.args.iter().map(|a| subst(a, label, value)).collect(),
            };
            let Node::Fact { fact, alts, .. } = &mut self.nodes[id] else {
                unreachable!()
            };
            *fact = new.clone();
            let alts_snapshot = alts.clone();
            for a in alts_snapshot {
                if let Node::Times(ch) = &mut self.nodes[a]
                    && !ch.contains(&w)
                {
                    ch.push(w);
                    ch.sort();
                }
            }
            self.by_fact.remove(&old);
            if let Some(&other) = self.by_fact.get(&new) {
                // Two tuples became one: ⊕ their alternatives.
                let Node::Fact { alts: mine, .. } =
                    std::mem::replace(&mut self.nodes[id], Node::Dead)
                else {
                    unreachable!()
                };
                let Node::Fact { alts, .. } = &mut self.nodes[other] else {
                    unreachable!()
                };
                alts.extend(mine);
                self.redirect(id, other);
            } else {
                self.by_fact.insert(new.clone(), id);
            }
            renamed.push((old, new));
        }
        renamed
    }

    fn redirect(&mut self, from: NodeId, to: NodeId) {
        for n in self.nodes.iter_mut() {
            if let Node::Times(ch) = n {
                for c in ch.iter_mut() {
                    if *c == from {
                        *c = to;
                    }
                }
                ch.sort();
                ch.dedup();
            }
        }
    }

    /// Deletion propagation: remove `f`; every firing that used it dies; every
    /// fact left with no alternatives is retracted in turn. Returns everything
    /// retracted, in order. (DRed without the re-derivation pass; enough for
    /// the non-recursive strata this prototype models.)
    #[cfg(test)]
    fn retract(&mut self, f: &Fact) -> Vec<Fact> {
        let mut out = Vec::new();
        let mut work = vec![f.clone()];
        while let Some(f) = work.pop() {
            let Some(id) = self.by_fact.remove(&f) else {
                continue;
            };
            self.nodes[id] = Node::Dead;
            out.push(f);
            let dead_times: Vec<NodeId> = self
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| match n {
                    Node::Times(ch) if ch.contains(&id) => Some(i),
                    _ => None,
                })
                .collect();
            for t in &dead_times {
                self.nodes[*t] = Node::Dead;
            }
            let orphaned: Vec<Fact> = self
                .nodes
                .iter_mut()
                .filter_map(|n| match n {
                    Node::Fact { fact, alts, .. } => {
                        alts.retain(|a| !dead_times.contains(a));
                        if alts.is_empty() {
                            Some(fact.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect();
            work.extend(orphaned);
        }
        out
    }

    /// Every fact, in fact order.
    pub fn facts(&self) -> Vec<Fact> {
        let mut out: Vec<Fact> = self.by_fact.keys().cloned().collect();
        out.sort();
        out
    }
}

fn normalize(mut ch: Vec<NodeId>) -> Vec<NodeId> {
    ch.sort();
    ch.dedup();
    ch
}

fn leaf_bytes(l: &Leaf) -> usize {
    match l {
        Leaf::Base { span: s }
        | Leaf::Rule { id: s }
        | Leaf::Schema { span: s }
        | Leaf::World { event: s }
        | Leaf::Plan { fact: s, .. }
        | Leaf::Input { source: s }
        | Leaf::Extern { call: s } => s.len(),
        Leaf::Absent { atom } => atom.pred.len() + atom.args.len() * size_of::<Term>(),
    }
}

/// Heap bytes of a tuple, for `Stats` and for sizing the bare fact store.
fn fact_bytes(f: &Fact) -> usize {
    f.pred.len()
        + f.args.capacity() * std::mem::size_of::<Value>()
        + f.args.iter().map(value_bytes).sum::<usize>()
}

fn value_bytes(v: &Value) -> usize {
    match v {
        Value::Str(s) => s.len(),
        Value::List(xs) => {
            xs.capacity() * std::mem::size_of::<Value>() + xs.iter().map(value_bytes).sum::<usize>()
        }
        // A BTreeMap node per entry, roughly key + value + two words.
        Value::Obj(m) => m
            .iter()
            .map(|(k, v)| {
                k.len()
                    + std::mem::size_of::<(String, Value)>()
                    + 2 * std::mem::size_of::<usize>()
                    + value_bytes(v)
            })
            .sum(),
        Value::Ref { typ, name, attr } | Value::CloudRef { typ, name, attr } => {
            typ.len() + name.len() + attr.len()
        }
        Value::Null { label, ty, .. } => label.len() + ty.len(),
        Value::Time(t) => t.zone.len(),
        Value::Uri(u) => u.to_string().len(),
        Value::Oci(u) => u.len(),
        Value::Semver(v) => v.to_string().len(),
        Value::Quantity(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Ip(_)
        | Value::IpNet { .. }
        | Value::Range(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lattice::{Collapsed, Constraint, Rank, Ranked};
    use crate::value::NullClass;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }
    fn fresh(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Fresh,
            ty: "string".into(),
        }
    }
    fn open(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Open,
            ty: "inet".into(),
        }
    }
    fn net(a: &str, p: u8) -> Value {
        Value::IpNet {
            addr: crate::value::ipv4_to_u32(a).unwrap(),
            prefix: p,
        }
    }
    fn leaves(w: &BTreeSet<BTreeSet<Leaf>>) -> BTreeSet<Leaf> {
        w.iter().flatten().cloned().collect()
    }

    /// The network module in miniature: want(vpc) -> prelude mints ν(vpc#id)
    /// -> subnet forwards it -> cluster forwards a list of it. One fact,
    /// want(vpc), never carries a null.
    fn build() -> (Circuit, Fact, Fact, Fact, Fact) {
        let mut c = Circuit::default();
        let base = c.leaf(Leaf::Base {
            span: "dform.df:40".into(),
        });
        let want_vpc = Fact::new("want", vec![s("net.vpc"), s("network.main/vpc")]);
        c.derive(want_vpc.clone(), vec![base]);
        let wv = c.fact_id(&want_vpc).unwrap();

        let prelude = c.leaf(Leaf::Rule {
            id: "prelude:computed(net.vpc,.id)".into(),
        });
        let schema = c.leaf(Leaf::Schema {
            span: "fake:type_attr(net.vpc,.id,computed)".into(),
        });
        let attr_vpc_id = Fact::new(
            "attr",
            vec![
                s("net.vpc"),
                s("network.main/vpc"),
                s(".id"),
                fresh("net.vpc/network.main/vpc#id"),
            ],
        );
        c.derive(attr_vpc_id.clone(), vec![prelude, schema, wv]);
        let av = c.fact_id(&attr_vpc_id).unwrap();

        let r_subnet = c.leaf(Leaf::Rule {
            id: "network.df:15".into(),
        });
        let attr_sub = Fact::new(
            "attr",
            vec![
                s("net.subnet"),
                s("network.main/private-a"),
                s(".vpc_id"),
                fresh("net.vpc/network.main/vpc#id"),
            ],
        );
        c.derive(attr_sub.clone(), vec![r_subnet, av]);
        let asub = c.fact_id(&attr_sub).unwrap();

        let r_cluster = c.leaf(Leaf::Rule {
            id: "kubernetes.df:8".into(),
        });
        let attr_cluster = Fact::new(
            "attr",
            vec![
                s("k8s.cluster"),
                s("kubernetes.main/cluster"),
                s(".vpc_ids"),
                Value::List(vec![fresh("net.vpc/network.main/vpc#id")]),
            ],
        );
        c.derive(attr_cluster.clone(), vec![r_cluster, asub]);
        (c, want_vpc, attr_vpc_id, attr_sub, attr_cluster)
    }

    #[test]
    fn phase_and_touches_before_resolution() {
        let (c, want_vpc, attr_vpc_id, attr_sub, attr_cluster) = build();
        let nu = "net.vpc/network.main/vpc#id".to_string();
        assert!(c.phase(&want_vpc).is_empty());
        assert!(c.touches(&want_vpc).is_empty());
        for f in [&attr_vpc_id, &attr_sub, &attr_cluster] {
            assert_eq!(c.phase(f), BTreeSet::from([nu.clone()]));
            assert_eq!(c.touches(f), BTreeSet::from([nu.clone()]));
        }
        // A fact with two alternatives, one null-free, is definite (∩) but touched (∪).
        let mut c = c;
        let other = c.leaf(Leaf::Base {
            span: "adopt.df:1".into(),
        });
        c.derive(attr_sub.clone(), vec![other]);
        assert!(
            !c.phase(&attr_sub).is_empty(),
            "the tuple itself still carries the null, so it is not definite"
        );
        // But a null-free *tuple* with one null-free alternative and one null-carrying alternative:
        let p = Fact::new(
            "subnet_of",
            vec![s("network.main/private-a"), s("network.main/vpc")],
        );
        let r = c.leaf(Leaf::Rule {
            id: "network.df:20".into(),
        });
        let asub = c.fact_id(&attr_sub).unwrap();
        c.derive(p.clone(), vec![r, asub]);
        assert!(!c.phase(&p).is_empty() || c.touches(&p).contains(&nu));
        c.derive(p.clone(), vec![other]);
        assert!(
            c.phase(&p).is_empty(),
            "a null-free alternative makes it definite"
        );
        assert!(
            c.touches(&p).contains(&nu),
            "but a resolution still touches it"
        );
    }

    #[test]
    fn resolution_respells_carrying_facts_and_why_records_the_world_event() {
        let (mut c, want_vpc, attr_vpc_id, attr_sub, attr_cluster) = build();
        let nu = "net.vpc/network.main/vpc#id";
        let want_id = c.fact_id(&want_vpc).unwrap();
        let before_sub = c.why(&attr_sub);

        let renamed = c.resolve(
            nu,
            &s("vpc-0a1b"),
            Leaf::World {
                event: "apply#1 create net.vpc".into(),
            },
        );
        assert_eq!(renamed.len(), 3, "exactly the touched facts are re-spelled");

        // The untouched fact keeps its node and its provenance.
        assert_eq!(c.fact_id(&want_vpc), Some(want_id));
        assert!(c.phase(&want_vpc).is_empty());

        // The old spellings are gone; the new ones exist.
        for f in [&attr_vpc_id, &attr_sub, &attr_cluster] {
            assert!(!c.has(f));
        }
        let new_sub = Fact::new(
            "attr",
            vec![
                s("net.subnet"),
                s("network.main/private-a"),
                s(".vpc_id"),
                s("vpc-0a1b"),
            ],
        );
        let new_cluster = Fact::new(
            "attr",
            vec![
                s("k8s.cluster"),
                s("kubernetes.main/cluster"),
                s(".vpc_ids"),
                Value::List(vec![s("vpc-0a1b")]),
            ],
        );
        assert!(c.has(&new_sub) && c.has(&new_cluster));
        assert!(c.phase(&new_sub).is_empty() && c.touches(&new_sub).is_empty());

        // why(new) = why(old) ⊗ world: every old leaf is still there, plus the event.
        let after = c.why(&new_sub);
        let world = Leaf::World {
            event: "apply#1 create net.vpc".into(),
        };
        assert!(after.iter().all(|ws| ws.contains(&world)));
        assert_eq!(
            leaves(&after)
                .difference(&leaves(&before_sub))
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([world.clone()])
        );
        assert!(leaves(&after).contains(&Leaf::Rule {
            id: "network.df:15".into()
        }));
        assert!(leaves(&after).contains(&Leaf::Base {
            span: "dform.df:40".into()
        }));
        // The cluster fact, two hops downstream, also sees the event exactly once.
        let cl = c.why(&new_cluster);
        assert!(cl.iter().all(|ws| ws.contains(&world)));
    }

    #[test]
    fn resolution_that_violates_a_refinement_retracts_the_cone_and_derives_a_conflict() {
        // Seam 1 case 4 driven through the circuit: an attr row carrying an open
        // null under a prefix_len refinement; resolution to a /24 makes the cell
        // ⊤, the attr row is retracted, its forwarding cone goes with it, and
        // attr_conflict is derived naming the schema and the contributor.
        let mut c = Circuit::default();
        let base = c.leaf(Leaf::Base {
            span: "pngu.df:70".into(),
        });
        let alloc_rule = c.leaf(Leaf::Rule {
            id: "pngu.df:88".into(),
        });
        let schema = c.leaf(Leaf::Schema {
            span: "pngu.df:31 control_plane_cidr: inet where prefix_len = 28".into(),
        });
        let agg = c.leaf(Leaf::Rule {
            id: "prelude:attr-aggregate".into(),
        });
        let nu = "alloc/cp#cidr";

        let arg = Fact::new(
            "arg",
            vec![
                s("gke.cluster"),
                s("pngu"),
                s(".master_control_plane_cidr"),
                open(nu),
                s("normal"),
            ],
        );
        c.derive(arg.clone(), vec![alloc_rule, base]);
        let arg_id = c.fact_id(&arg).unwrap();
        let attr = Fact::new(
            "attr",
            vec![
                s("gke.cluster"),
                s("pngu"),
                s(".master_control_plane_cidr"),
                open(nu),
            ],
        );
        c.derive(attr.clone(), vec![agg, schema, arg_id]);
        let attr_id = c.fact_id(&attr).unwrap();
        let fwd_rule = c.leaf(Leaf::Rule {
            id: "monitoring.df:3".into(),
        });
        let fwd = Fact::new(
            "attr",
            vec![
                s("google.monitoring_dashboard"),
                s("pngu"),
                s(".note"),
                open(nu),
            ],
        );
        c.derive(fwd.clone(), vec![fwd_rule, attr_id]);

        // The cell, as the aggregate sees it.
        let cell = Ranked::at(Rank::Normal, arg_id as u32, open(nu)).join(
            &Ranked::constraint(Constraint::PrefixLenGe(28), schema as u32),
            ".x",
        );
        assert!(
            matches!(cell.collapse(), Collapsed::Val { ref deferred, .. } if !deferred.is_empty())
        );

        // Phase boundary.
        let world = Leaf::World {
            event: "apply#1 allocate cidr".into(),
        };
        let bad = net("10.0.0.0", 24);
        c.resolve(nu, &bad, world.clone());
        let resolved_attr = Fact::new(
            "attr",
            vec![
                s("gke.cluster"),
                s("pngu"),
                s(".master_control_plane_cidr"),
                bad.clone(),
            ],
        );
        assert!(c.has(&resolved_attr));
        let Collapsed::Violated {
            witnesses: a,
            refinement: b,
            ..
        } = cell.resolve(nu, &bad).collapse()
        else {
            panic!("expected a violation")
        };

        // The engine's reaction: retract the attr row and its cone, derive attr_conflict.
        let gone = c.retract(&resolved_attr);
        let resolved_fwd = Fact::new(
            "attr",
            vec![
                s("google.monitoring_dashboard"),
                s("pngu"),
                s(".note"),
                bad.clone(),
            ],
        );
        assert!(
            gone.contains(&resolved_attr) && gone.contains(&resolved_fwd),
            "cone retracted: {gone:?}"
        );
        let resolved_arg = Fact::new(
            "arg",
            vec![
                s("gke.cluster"),
                s("pngu"),
                s(".master_control_plane_cidr"),
                bad.clone(),
                s("normal"),
            ],
        );
        assert!(
            c.has(&resolved_arg),
            "the contribution below the aggregate survives"
        );

        let wl = c.leaf(world.clone());
        let conflict = Fact::new(
            "attr_conflict",
            vec![
                s("gke.cluster"),
                s("pngu"),
                s(".master_control_plane_cidr"),
                Value::Int(a.iter().next().copied().unwrap() as i64),
                Value::Int(b.iter().next().copied().unwrap() as i64),
            ],
        );
        let resolved_arg_id = c.fact_id(&resolved_arg).unwrap();
        c.derive(conflict.clone(), vec![agg, schema, resolved_arg_id, wl]);
        let why = leaves(&c.why(&conflict));
        assert!(why.contains(&Leaf::Schema {
            span: "pngu.df:31 control_plane_cidr: inet where prefix_len = 28".into()
        }));
        assert!(why.contains(&Leaf::Rule {
            id: "pngu.df:88".into()
        }));
        assert!(why.contains(&world));
        assert!(c.phase(&conflict).is_empty(), "the conflict is definite");
    }

    #[test]
    fn absorption_keeps_alternatives_an_antichain() {
        let mut c = Circuit::default();
        let a = c.leaf(Leaf::Base { span: "a".into() });
        let b = c.leaf(Leaf::Base { span: "b".into() });
        let r = c.leaf(Leaf::Rule { id: "r".into() });
        let f = Fact::new("p", vec![s("x")]);
        c.derive(f.clone(), vec![r, a, b]);
        c.derive(f.clone(), vec![r, a]); // absorbs the first
        assert_eq!(
            c.why(&f),
            BTreeSet::from([BTreeSet::from([
                Leaf::Rule { id: "r".into() },
                Leaf::Base { span: "a".into() }
            ])])
        );
        c.derive(f.clone(), vec![r, a, b]); // absorbed by the second
        assert_eq!(c.why(&f).len(), 1);
    }

    #[test]
    fn a_repeated_firing_is_stored_once_and_alternatives_are_capped() {
        let mut c = Circuit::default();
        let r = c.leaf(Leaf::Rule { id: "r".into() });
        let f = Fact::new("p", vec![s("x")]);
        let a = c.leaf(Leaf::Base { span: "a".into() });
        c.derive(f.clone(), vec![r, a]);
        let n = c.stats().times;
        c.derive(f.clone(), vec![a, r]);
        assert_eq!(c.stats().times, n, "naive re-evaluation adds no node");
        for i in 0..MAX_ALTS + 2 {
            let b = c.leaf(Leaf::Base {
                span: format!("b{i}"),
            });
            c.derive(f.clone(), vec![r, b]);
        }
        let View::Fact {
            alts, truncated, ..
        } = c.view(c.fact_id(&f).unwrap())
        else {
            panic!()
        };
        assert_eq!(alts.len(), MAX_ALTS);
        assert!(truncated);
    }

    #[test]
    fn a_fact_prints_as_its_ground_atom() {
        let f = Fact::new("p", vec![s("a"), Value::Int(1)]);
        assert_eq!(crate::spell::atom(&f.atom()), "p(\"a\", 1)");
    }
}
