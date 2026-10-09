//! A derivation compressed to its leaves (`plan --why`, `diff`): walked
//! through the shortest alternative of each fact, each line a `Because`.

use super::because::Because;
use super::printer::{Focus, plan_text};
use super::sites::{base_parts, table_row};
use super::surface::Surface;
use crate::circuit::{Circuit, Leaf, NodeId, View};
use crate::value::Value;
use std::collections::BTreeMap;

/// The compressed walk: the lines so far, and each fact node's size (the
/// leaves of its shortest derivation).
pub(super) struct Compress {
    pub(super) out: Vec<Because>,
    pub(super) sizes: BTreeMap<NodeId, usize>,
}

impl Compress {
    pub(super) fn push(&mut self, b: Because) {
        if !self.out.contains(&b) {
            self.out.push(b);
        }
    }

    /// How many leaves node `id`'s shortest derivation has.
    pub(super) fn size(&mut self, c: &Circuit, id: NodeId) -> usize {
        if let Some(n) = self.sizes.get(&id) {
            return *n;
        }
        // A node on the current path counts as large: no cycle is taken.
        self.sizes.insert(id, usize::MAX / 4);
        let n = match c.view(id) {
            View::Leaf(_) => 1,
            View::Fact { alts, .. } => alts
                .iter()
                .map(|a| self.size(c, *a))
                .min()
                .unwrap_or(usize::MAX / 4),
            View::Times { children, .. } => children
                .iter()
                .map(|ch| self.size(c, *ch))
                .fold(0usize, |a, b| a.saturating_add(b)),
            View::Dead => usize::MAX / 4,
        };
        self.sizes.insert(id, n);
        n
    }

    /// Fact node `id`'s lines. `subject`: the attribute a contribution is
    /// to, which a stated contribution is named by; `head`: its statement
    /// is printed (the explained fact's, and each winning contribution's).
    pub(super) fn fact(
        &mut self,
        s: &mut Surface,
        id: NodeId,
        subject: Option<&str>,
        head: bool,
        focus: Option<&Focus>,
    ) {
        let circuit = s.p.circuit;
        let View::Fact { fact, alts, .. } = circuit.view(id) else {
            return;
        };
        let text = subject
            .map(str::to_string)
            .unwrap_or_else(|| s.fact_text(fact));
        if let [a] = alts
            && let View::Times { children: [l], .. } = circuit.view(*a)
            && let View::Leaf(l) = circuit.view(*l)
        {
            if let Some(b) = self.leaf(s, l, &text) {
                self.push(b);
            }
            return;
        }
        // A relation read from a table's row: the row, by the relation.
        if let Some(at) = table_row(circuit, alts) {
            self.push(Because::new("fact", Some(at), text));
            return;
        }
        if !s.w.seen.insert(id) {
            return;
        }
        let Some(&alt) = alts.iter().min_by_key(|a| self.size(circuit, **a)) else {
            return;
        };
        let View::Times { children, .. } = circuit.view(alt) else {
            return;
        };
        let mut rule = None;
        let mut facts = Vec::new();
        let mut others = Vec::new();
        for c in children {
            match circuit.view(*c) {
                View::Leaf(Leaf::Rule { id }) => rule = Some(id.as_str()),
                View::Leaf(l) => others.push(l),
                View::Fact { .. } => facts.push(*c),
                View::Times { .. } | View::Dead => {}
            }
        }
        if rule.is_some_and(|r| r.starts_with('Σ')) {
            // The winners: the contributions at the highest rank, of those
            // that hold the focused part.
            let rank = |f: &NodeId| match circuit.view(*f) {
                View::Fact { fact, .. } => match fact.args.get(4).and_then(Value::as_str) {
                    Some("override") => 2,
                    Some("default") => 0,
                    _ => 1,
                },
                _ => 0,
            };
            facts.retain(|f| match (focus, circuit.view(*f)) {
                (Some(focus), View::Fact { fact, .. }) => {
                    fact.args.get(3).is_some_and(|v| focus.holds(v))
                }
                _ => true,
            });
            let top = facts.iter().map(rank).max();
            let cell = s.fact_text(fact);
            for f in facts.into_iter().filter(|f| Some(rank(f)) == top) {
                self.fact(s, f, Some(&cell), head, None);
            }
            return;
        }
        // A rule the compiler wrote has no statement to show.
        if head && let Some((place, text, _)) = rule.and_then(|r| s.source_line(r)) {
            self.push(Because::new("rule", Some(place), text));
        }
        // A contribution a statement makes to an input, a `set`'s (R-38), is named where it is written, as a stated
        // one is; `--set` is its flag, below, and the declaration's default
        // a stated fact.
        if !head
            && subject.is_some_and(|t| t.starts_with("input "))
            && let Some((place, text, _)) = rule.and_then(|r| s.source_line(r))
            && !text.starts_with("input ")
            && !text.starts_with("key ")
        {
            self.push(Because::new("fact", Some(place), s.fact_text(fact)));
        }
        for f in facts {
            self.fact(s, f, None, false, None);
        }
        for l in others {
            if let Some(b) = self.leaf(s, l, &text) {
                self.push(b);
            }
        }
    }

    /// Leaf `l` of a firing of the fact printed as `text`.
    fn leaf(&self, s: &Surface, l: &Leaf, text: &str) -> Option<Because> {
        let r = s.p.redact;
        Some(match l {
            Leaf::Base { span } => {
                let (at, origin) = base_parts(span);
                let origin = origin.map(|o| format!("   ({o})")).unwrap_or_default();
                Because::new("fact", Some(at.to_string()), format!("{text}{origin}"))
            }
            Leaf::Input { source } => Because::new("input", None, r.text(source)),
            Leaf::Extern { .. } => Because::new("answered", None, text.to_string()),
            Leaf::World { .. } => Because::new("world", None, text.to_string()),
            Leaf::Plan { tick, .. } => {
                Because::new("plan", None, format!("{text}   ({})", plan_text(*tick)))
            }
            // A read of a computed attribute, as the compiler lowers it:
            // not the program's to explain.
            Leaf::Absent { pattern } if pattern.starts_with("resolved(") => return None,
            Leaf::Absent { pattern } => {
                Because::new("absent", None, format!("not {}", s.absent(pattern)))
            }
            Leaf::Schema { .. } | Leaf::Rule { .. } => return None,
        })
    }
}
