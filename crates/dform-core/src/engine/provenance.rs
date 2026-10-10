//! Provenance (E §3.1, DR-10): the fact store with a circuit node per tuple, a firing
//! recorded as it is inserted, and the leaf a given fact is labelled with.

use super::store::{Store, TupleId};
use crate::ast::{Atom, Span, Term};
use crate::circuit::{self, Circuit, Leaf, NodeId};
use crate::diag;
use crate::spell;
use crate::value::Value;
use std::collections::BTreeSet;

/// The circuit's spelling of a ground fact.
pub fn circuit_fact(a: &Atom) -> circuit::Fact {
    circuit::Fact::new(
        &a.pred,
        a.args
            .iter()
            .map(|t| match t {
                Term::Val(v) => v.clone(),
                other => Value::Str(spell::term(other)),
            })
            .collect(),
    )
}

/// The fact store and its provenance: every tuple in the store has a node
/// in the circuit, recorded when the tuple is inserted and once more per
/// distinct firing that derives it again.
#[derive(Default, Clone)]
pub(super) struct Prov {
    pub(super) circuit: Circuit,
    pub(super) store: Store,
    /// Circuit node per tuple id.
    node: Vec<NodeId>,
}

impl Prov {
    /// Record one firing of `a` and insert it: its tuple id, and whether
    /// it is new.
    pub(super) fn record(
        &mut self,
        a: Atom,
        children: Vec<NodeId>,
        bindings: Vec<(String, Value)>,
    ) -> (TupleId, bool) {
        match self.store.id(&a) {
            Some(id) => {
                self.circuit
                    .fire(self.node[id as usize], children, bindings);
                (id, false)
            }
            None => {
                let n = self
                    .circuit
                    .derive_with(circuit_fact(&a), children, bindings);
                self.node.push(n);
                (self.store.insert(a).0, true)
            }
        }
    }

    pub(super) fn given(&mut self, a: Atom, leaf: Leaf) -> TupleId {
        let l = self.circuit.leaf(leaf);
        self.record(a, vec![l], vec![]).0
    }

    /// Insert a fact the program states, labelled with where it is written.
    pub(super) fn stated(&mut self, g: Atom) -> TupleId {
        let span = match (diag::at(g.span), diag::origin(g.span)) {
            (Some(at), Some(o)) => format!("{at} ({}, {o})", g.pred),
            (Some(at), None) => format!("{at} ({})", g.pred),
            (None, _) => format!("compiler ({})", g.pred),
        };
        self.given(g, Leaf::Base { span })
    }

    pub(super) fn rule(&mut self, id: String, text: &str, span: Span) -> NodeId {
        self.circuit.name_rule(&id, text);
        if let Some(at) = diag::place(span) {
            self.circuit.locate_rule(&id, at);
        }
        if let (Some((file, text)), Some((_, line, _))) =
            (diag::source_of(span), diag::location(span))
        {
            let end = (span.end as usize).min(text.len());
            let src = circuit::RuleSource {
                file,
                start: (span.start as usize).min(end),
                end,
                line,
                origin: diag::origin(span),
                text,
            };
            self.circuit.source_rule(&id, src);
        }
        self.circuit.leaf(Leaf::Rule { id })
    }

    pub(super) fn absent(&mut self, a: &Atom) -> NodeId {
        self.circuit.leaf(Leaf::Absent { atom: a.clone() })
    }

    pub(super) fn id(&self, t: TupleId) -> NodeId {
        self.node[t as usize]
    }
}

/// The leaf for a fact given to this run rather than stated by the program.
/// `tick`: the apply tick the planner injected its facts at (`None`: the
/// plan).
pub(super) fn given_leaf(
    a: &Atom,
    externs: &BTreeSet<crate::ast::Extern>,
    tick: Option<usize>,
) -> Leaf {
    let text = spell::atom(a);
    if let Some(event) = crate::stack::published(a) {
        return Leaf::World { event };
    }
    match a.pred.as_str() {
        p if crate::zset::POLICY_INPUTS.contains(&p) => Leaf::Plan { fact: text, tick },
        "input" | "data" => {
            let flag = if a.pred == "input" { "set" } else { "data" };
            let kv = match a.args.as_slice() {
                [Term::Val(k), Term::Val(v)] => {
                    format!("{}={}", spell::bare(k), spell::bare(v))
                }
                _ => text,
            };
            Leaf::Input {
                source: format!("--{flag} {kv}"),
            }
        }
        p if p.starts_with("type_") => Leaf::Schema { span: text },
        // A kept value says when it was kept, never the value or the
        // candidate (either may be a secret).
        crate::memo::FIRST => match a.args.first() {
            Some(Term::Val(Value::Str(k))) => Leaf::Extern {
                call: crate::memo::provenance(k),
            },
            _ => Leaf::Extern { call: text },
        },
        // A table's row: stated where its file states it.
        p if externs.iter().any(|e| e.pred == p) => match crate::tables::at(a) {
            Some(span) => Leaf::Base { span },
            None => Leaf::Extern { call: text },
        },
        _ => Leaf::World { event: text },
    }
}
