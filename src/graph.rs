//! `dform graph`: Graphviz DOT for the resource dependency DAG, the
//! partition graph, or any binary relation in the final fact store. Nodes
//! and edges are sorted, so the output is deterministic.

use crate::ast::{Atom, Term};
use crate::ir::Resource;
use crate::partition::{self, Node};
use crate::query::Redactor;
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Edges `a -> b`; a dashed edge is drawn `style=dashed`.
struct Dot {
    name: String,
    nodes: BTreeSet<String>,
    edges: BTreeSet<(String, String, bool)>,
    /// Node clusters, by label: every node of a cluster is drawn inside it.
    clusters: BTreeMap<usize, (String, BTreeSet<String>)>,
}

impl Dot {
    fn new(name: &str) -> Dot {
        Dot {
            name: name.into(),
            nodes: BTreeSet::new(),
            edges: BTreeSet::new(),
            clusters: BTreeMap::new(),
        }
    }

    fn edge(&mut self, a: String, b: String, dashed: bool) {
        self.nodes.insert(a.clone());
        self.nodes.insert(b.clone());
        self.edges.insert((a, b, dashed));
    }

    fn render(&self) -> String {
        let mut out = format!("digraph {} {{\n  rankdir=LR;\n", quote(&self.name));
        let clustered: BTreeSet<&String> = self.clusters.values().flat_map(|(_, ns)| ns).collect();
        for (i, (label, ns)) in &self.clusters {
            out.push_str(&format!(
                "  subgraph cluster_{i} {{\n    label={};\n",
                quote(label)
            ));
            for n in ns {
                out.push_str(&format!("    {};\n", quote(n)));
            }
            out.push_str("  }\n");
        }
        for n in self.nodes.iter().filter(|n| !clustered.contains(n)) {
            out.push_str(&format!("  {};\n", quote(n)));
        }
        for (a, b, dashed) in &self.edges {
            let style = if *dashed { " [style=dashed]" } else { "" };
            out.push_str(&format!("  {} -> {}{style};\n", quote(a), quote(b)));
        }
        out.push_str("}\n");
        out
    }
}

/// The resource dependency DAG: `A -> B` when resource A reads B (a ref or
/// a null B's Apply resolves), so B is applied first.
pub fn resources(rs: &[Resource]) -> String {
    let addr = |a: &crate::ir::Address| format!("{}/{}", a.typ, a.name);
    let mut d = Dot::new("resources");
    for r in rs {
        d.nodes.insert(addr(&r.addr));
        for dep in &r.deps {
            d.edge(addr(&r.addr), addr(dep), false);
        }
    }
    d.render()
}

/// The partition graph: `A -> B` when a rule reads A to derive B; a
/// negative edge dashed. With strata, each stratum is a cluster.
pub fn strata(g: &partition::Graph, strata: Option<&BTreeMap<Node, usize>>) -> String {
    let mut d = Dot::new("strata");
    d.nodes.extend(g.nodes.iter().map(Node::to_string));
    for e in &g.edges {
        d.edge(e.from.to_string(), e.to.to_string(), e.negative);
    }
    for (n, s) in strata.into_iter().flatten() {
        d.clusters
            .entry(*s)
            .or_insert_with(|| (format!("stratum {s}"), BTreeSet::new()))
            .1
            .insert(n.to_string());
    }
    d.render()
}

/// A node's label: a string as itself, anything else (or a secret) as the
/// query printer prints it.
fn label(r: &Redactor, v: &Value) -> String {
    match v {
        Value::Str(s) if !r.is_secret(v) => s.clone(),
        v => r.fmt(v),
    }
}

/// Any binary relation `pred/2` of the fact store: `a -> b` per fact.
pub fn relation(spec: &str, facts: &BTreeSet<Atom>, r: &Redactor) -> Result<String> {
    let pred = match spec.split_once('/') {
        Some((p, "2")) => p,
        Some((_, n)) => bail!("graph: {spec} is not a binary relation (arity {n}, want 2)"),
        None => spec,
    };
    let mut d = Dot::new(pred);
    let mut any = false;
    for a in facts.iter().filter(|a| a.pred == pred) {
        let [Term::Val(x), Term::Val(y)] = a.args.as_slice() else {
            bail!("graph: {pred} is not a binary relation: {}", r.fmt_atom(a));
        };
        any = true;
        d.edge(label(r, x), label(r, y), false);
    }
    if !any {
        bail!("graph: no {pred}/2 facts");
    }
    Ok(d.render())
}
