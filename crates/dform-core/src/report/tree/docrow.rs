//! A value a loader read (R-131): the plan says it by its row in the document it
//! came from (`crds.yml:412`), not by the statement that read the document.

use super::compress::Compress;
use super::printer::Printer;
use crate::ast::{Lit, RuleStmt, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::value::Value;
use std::collections::BTreeMap;

/// A value a loader read (R-131): the plan says it by its row, not its
/// content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRow {
    /// The row of the document it is, `vendor/crds.yml:412` (the file
    /// alone for a document that is the whole file), and the steps into
    /// it when the value is part of one, `teams.yml .teams[2]`.
    pub at: String,
    /// Its size as JSON, as the plan file writes it.
    pub size: usize,
    /// A resource's value body (`resource T N = d`): the row is the
    /// resource's, not one attribute's.
    pub body: bool,
    /// The contribution's path, as the attribute stores it.
    pub path: String,
}

impl DocRow {
    /// `vendor/crds.yml:412  (24.0 KB)`.
    pub fn text(&self) -> String {
        format!("{}  ({})", self.at, crate::query::size(self.size))
    }
}

/// The loader documents (`table.FORMAT.document(.., At, Doc)`) a firing's
/// `children` read, and those the cells it reads read (`d in docs`, a
/// `let docs = yaml(..)`), a few steps deep.
pub(super) fn documents<'c>(
    c: &mut Compress,
    circuit: &'c Circuit,
    children: &[NodeId],
    depth: usize,
) -> Vec<&'c Fact> {
    let mut out = Vec::new();
    for ch in children {
        let View::Fact { fact, alts, .. } = circuit.view(*ch) else {
            continue;
        };
        if crate::tables::is_document(&fact.pred) {
            out.push(fact);
            continue;
        }
        if depth >= 2 || !matches!(fact.pred.as_str(), "attr" | "arg") {
            continue;
        }
        if let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a))
            && let View::Times { children, .. } = circuit.view(alt)
        {
            out.extend(documents(c, circuit, children, depth + 1));
        }
    }
    out
}

/// Whether `v` is an object or a list with something in it: a value a
/// row says (R-131), where a scalar says itself.
pub(super) fn structured(v: &Value) -> bool {
    match v {
        Value::Obj(m) => !m.is_empty(),
        Value::List(xs) => !xs.is_empty(),
        _ => false,
    }
}

/// Where value `v` is in loader document `d`: its row, `crds.yml:412` (a
/// stream's document by the line it starts on), and the steps to it from
/// there, `teams.yml .teams[2]`. `None` when the document does not hold
/// it.
pub(super) fn document_place(d: &Fact, v: &Value) -> Option<String> {
    let [.., Value::Str(at), doc] = d.args.as_slice() else {
        return None;
    };
    place_in(at, doc, v)
}

/// Where value `v` is in document `doc` read at `at` ([`document_place`]).
pub(super) fn place_in(at: &str, doc: &Value, v: &Value) -> Option<String> {
    let mut steps = Vec::new();
    if !locate(doc, v, &mut steps) {
        return None;
    }
    // A repository's commit by its first digits (R-153).
    let shown = crate::tables::shown_at(at);
    let mut place = shown.clone();
    let mut rest = steps.as_slice();
    if let (Value::List(xs), [(Some(i), _), tail @ ..]) = (doc, rest)
        && let Some(line) = crate::tables::document_line(at, xs.len(), *i)
    {
        place = format!("{shown}:{line}");
        rest = tail;
    }
    if !rest.is_empty() {
        place.push(' ');
        place.extend(rest.iter().map(|(_, s)| s.as_str()));
    }
    Some(place)
}

/// The steps from `d` to a part of it equal to `v` (an element by its
/// index), the first in document order; `false` when there is none.
pub(super) fn locate(d: &Value, v: &Value, steps: &mut Vec<(Option<usize>, String)>) -> bool {
    if d == v {
        return true;
    }
    match d {
        Value::Obj(m) => m.iter().any(|(k, x)| {
            steps.push((None, format!(".{}", crate::fmt::value::key_text(k))));
            let found = locate(x, v, steps);
            if !found {
                steps.pop();
            }
            found
        }),
        Value::List(xs) => xs.iter().enumerate().any(|(i, x)| {
            steps.push((Some(i), format!("[{i}]")));
            let found = locate(x, v, steps);
            if !found {
                steps.pop();
            }
            found
        }),
        _ => false,
    }
}

impl Printer<'_> {
    /// The document row contribution `id` (an `arg`, or an attribute no
    /// aggregate made: its `arg`) read its value from: a loader's document
    /// its firing reads that holds the value whole, the whole body for a
    /// value body's contribution. `None` for a scalar, or a value no
    /// document holds as it is (an expression made it from one).
    pub fn document_row(&self, rules: &[RuleStmt], id: NodeId) -> Option<DocRow> {
        let circuit = self.circuit;
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        let mut id = id;
        let (fact, children, bindings) = loop {
            let View::Fact { fact, alts, .. } = circuit.view(id) else {
                return None;
            };
            let &alt = alts.iter().min_by_key(|a| c.size(circuit, **a))?;
            let View::Times { children, bindings } = circuit.view(alt) else {
                return None;
            };
            match fact.pred.as_str() {
                "arg" => break (fact, children, bindings),
                "attr" if !children.iter().any(|ch| matches!(circuit.view(*ch), View::Leaf(Leaf::Rule { id }) if id.starts_with('Σ'))) => {
                    id = children.iter().copied().find(
                        |ch| matches!(circuit.view(*ch), View::Fact { fact, .. } if fact.pred == "arg"),
                    )?;
                }
                _ => return None,
            }
        };
        let rule = children.iter().find_map(|ch| match circuit.view(*ch) {
            View::Leaf(Leaf::Rule { id }) => id
                .strip_prefix('r')
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| rules.get(i)),
            _ => None,
        })?;
        // A value body's contribution is one key of the body: the body
        // is the value read.
        let body = rule.body.iter().find_map(|l| match l {
            Lit::Eq(Term::Var(x), Term::Func { name, .. }) if name == crate::ir::RESOURCE_BODY => {
                bindings.iter().find(|(k, _)| k == x).map(|(_, v)| v)
            }
            _ => None,
        });
        let docs = documents(&mut c, circuit, children, 0);
        let mut path = fact.args.get(2).and_then(Value::as_str)?.to_string();
        let mut value = match body {
            Some(v) => v,
            None => fact.args.get(3)?,
        };
        // A block's entry below an attribute (`metadata.labels = ..`) is
        // the attribute's contribution of an object of one key.
        loop {
            if !structured(value) {
                return None;
            }
            if let Some(at) = docs.iter().find_map(|d| document_place(d, value)) {
                return Some(DocRow {
                    at,
                    size: serde_json::to_vec(&engine::value_to_json(value)).map_or(0, |b| b.len()),
                    body: body.is_some(),
                    path,
                });
            }
            match value {
                Value::Obj(m) if body.is_none() && m.len() == 1 => {
                    let (k, v) = m.iter().next()?;
                    path = crate::ir::path_join(&path, k);
                    value = v;
                }
                _ => return None,
            }
        }
    }
}
