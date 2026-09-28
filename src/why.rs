//! `dform why`: a fact's derivation tree, read from the provenance circuit
//! (E §3.3). Each fact prints with the firing that derived it (rule id and
//! text, the rule's bindings) and that firing's children, recursively; a
//! given fact prints with where it came from. An aggregate prints every
//! contribution with its rank and owner. A fact with several alternatives
//! shows the first and `...` for the rest unless `all`; a fact already
//! expanded above prints `(see above)`.
//!
//! An `attr` or `arg` pattern may name part of an object attribute, by a
//! dotted path (`"tags.team"`) or an object value (`{team: "platform"}`):
//! it matches the attribute that contains it, and the tree shows only the
//! contributions that do.

use crate::ast::{Atom, Lit, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::query::Redactor;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The part of an object attribute a pattern named: the keys below the
/// top-level path, and the value there (`None`: any).
#[derive(Debug, Clone, PartialEq)]
pub struct Focus {
    keys: Vec<String>,
    value: Option<Value>,
}

impl Focus {
    fn holds(&self, v: &Value) -> bool {
        let mut at = v;
        for k in &self.keys {
            let Value::Obj(m) = at else { return false };
            let Some(x) = m.get(k) else { return false };
            at = x;
        }
        self.value.as_ref().is_none_or(|w| contains(at, w))
    }
}

/// `v` has `w`: equal, or an object with every key of object `w`, recursively.
fn contains(v: &Value, w: &Value) -> bool {
    match (v, w) {
        (Value::Obj(vm), Value::Obj(wm)) => wm
            .iter()
            .all(|(k, wv)| vm.get(k).is_some_and(|vv| contains(vv, wv))),
        _ => v == w,
    }
}

/// A ground term's value: literals, lists and objects of them.
fn ground(t: &Term) -> Option<Value> {
    match t {
        Term::Val(v) => Some(v.clone()),
        Term::List(xs) => xs
            .iter()
            .map(ground)
            .collect::<Option<_>>()
            .map(Value::List),
        Term::Obj(m) => m
            .iter()
            .map(|(k, t)| Some((k.clone(), ground(t)?)))
            .collect::<Option<BTreeMap<_, _>>>()
            .map(Value::Obj),
        _ => None,
    }
}

/// The facts `pattern` names, each with the focus it was matched under.
pub fn find(pattern: &Atom, facts: &BTreeSet<Atom>) -> Result<Vec<(Atom, Option<Focus>)>> {
    let matched = |p: &Atom| -> Result<Vec<Atom>> {
        let mut out: Vec<Atom> = engine::query(&[Lit::Pos(p.clone())], facts)?
            .into_iter()
            .filter_map(|(_, used)| used.into_iter().next())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    };
    let exact = matched(pattern)?;
    if !exact.is_empty()
        || !matches!(pattern.pred.as_str(), "attr" | "arg")
        || pattern.args.len() < 4
    {
        return Ok(exact.into_iter().map(|a| (a, None)).collect());
    }
    // Part of an object attribute: relax the path to its top level and the
    // value to a wildcard, then keep what contains the part.
    let Term::Val(Value::Str(path)) = &pattern.args[2] else {
        return Ok(vec![]);
    };
    let mut keys: Vec<String> = path.split('.').map(str::to_string).collect();
    let top = keys.remove(0);
    let value = match &pattern.args[3] {
        Term::Var(_) | Term::Wildcard => None,
        t => match ground(t) {
            Some(v) => Some(v),
            None => return Ok(vec![]),
        },
    };
    if keys.is_empty() && !matches!(value, Some(Value::Obj(_))) {
        return Ok(vec![]);
    }
    let focus = Focus { keys, value };
    let mut relaxed = pattern.clone();
    relaxed.args[2] = Term::Val(Value::Str(top));
    relaxed.args[3] = Term::Wildcard;
    Ok(matched(&relaxed)?
        .into_iter()
        .filter(|a| matches!(&a.args[3], Term::Val(v) if focus.holds(v)))
        .map(|a| (a, Some(focus.clone())))
        .collect())
}

/// The output so far, and the facts already expanded in it.
#[derive(Default)]
struct Walk {
    out: String,
    seen: BTreeSet<NodeId>,
}

pub struct Printer<'a> {
    pub circuit: &'a Circuit,
    pub redact: &'a Redactor,
    /// Show every alternative, not only the first.
    pub all: bool,
}

impl Printer<'_> {
    /// The derivation tree of fact node `root`; with a focus, only the
    /// contributions to it that hold the focused part.
    pub fn tree(&self, root: NodeId, focus: Option<&Focus>) -> String {
        let mut w = Walk::default();
        self.fact(&mut w, root, "", "", None, focus);
        w.out
    }

    fn fact_text(&self, f: &Fact) -> String {
        self.redact.fmt_atom(&Atom {
            pred: f.pred.clone(),
            args: f.args.iter().cloned().map(Term::Val).collect(),
            record: None,
            span: Default::default(),
        })
    }

    /// Print fact node `id`: its line after `lead`, the rest after `pad`.
    /// `note` is appended to the fact's line (rank and owner of an
    /// aggregate contribution).
    fn fact(
        &self,
        w: &mut Walk,
        id: NodeId,
        lead: &str,
        pad: &str,
        note: Option<String>,
        focus: Option<&Focus>,
    ) {
        let View::Fact {
            fact,
            alts,
            truncated,
        } = self.circuit.view(id)
        else {
            w.out.push_str(&format!("{lead}(retracted)\n"));
            return;
        };
        let note = note.map(|n| format!("   [{n}]")).unwrap_or_default();
        let text = self.fact_text(fact);
        // A given fact: one firing over one source leaf.
        if let [a] = alts
            && let Some(src) = self.given(*a)
        {
            w.out.push_str(&format!("{lead}{text}   {src}{note}\n"));
            return;
        }
        if !w.seen.insert(id) {
            w.out
                .push_str(&format!("{lead}{text}   (see above){note}\n"));
            return;
        }
        w.out.push_str(&format!("{lead}{text}{note}\n"));
        let shown = if self.all { alts.len() } else { 1 };
        for (k, a) in alts.iter().take(shown).enumerate() {
            if alts.len() > 1 && self.all {
                w.out.push_str(&format!(
                    "{pad}  alternative {} of {}:\n",
                    k + 1,
                    alts.len()
                ));
            }
            self.firing(w, *a, &format!("{pad}  "), focus);
        }
        if alts.len() > shown {
            let more = alts.len() - shown;
            w.out.push_str(&format!(
                "{pad}  ... {more} more alternative{} (--all)\n",
                if more == 1 { "" } else { "s" }
            ));
        }
        if truncated {
            w.out.push_str(&format!(
                "{pad}  ... further alternatives dropped at {}\n",
                crate::circuit::MAX_ALTS
            ));
        }
    }

    /// Where a given fact came from, when firing `a` is a single source leaf.
    fn given(&self, a: NodeId) -> Option<String> {
        let View::Times { children, .. } = self.circuit.view(a) else {
            return None;
        };
        let [c] = children else { return None };
        let View::Leaf(l) = self.circuit.view(*c) else {
            return None;
        };
        Some(match l {
            Leaf::Schema { .. } => "provider schema".into(),
            Leaf::World { .. } => "world (refresh)".into(),
            Leaf::Extern { .. } => "extern".into(),
            l => self.redact.text(&leaf_text(l)),
        })
    }

    fn firing(&self, w: &mut Walk, a: NodeId, pad: &str, focus: Option<&Focus>) {
        let View::Times { children, bindings } = self.circuit.view(a) else {
            return;
        };
        let mut facts = Vec::new();
        let mut others = Vec::new();
        let mut aggregate = false;
        for c in children {
            match self.circuit.view(*c) {
                View::Leaf(Leaf::Rule { id }) => {
                    let text = self.circuit.rule_text(id).unwrap_or("");
                    aggregate = id.starts_with('Σ');
                    let over = if aggregate {
                        let n = children.len() - 1;
                        format!(" over {n} contribution{}", if n == 1 { "" } else { "s" })
                    } else {
                        String::new()
                    };
                    w.out
                        .push_str(&format!("{pad}by {id}: {}{over}\n", self.redact.text(text)));
                }
                View::Leaf(l) => others.push(l.clone()),
                View::Fact { .. } => facts.push(*c),
                View::Times { .. } | View::Dead => {}
            }
        }
        if !bindings.is_empty() {
            let b: Vec<String> = bindings
                .iter()
                .map(|(k, v)| format!("{k} = {}", self.redact.fmt(v)))
                .collect();
            w.out.push_str(&format!("{pad}with {}\n", b.join(", ")));
        }
        let mut hidden = 0;
        if let Some(focus) = focus.filter(|_| aggregate && !self.all) {
            let before = facts.len();
            facts.retain(|f| match self.circuit.view(*f) {
                View::Fact { fact, .. } => fact.args.get(3).is_some_and(|v| focus.holds(v)),
                _ => true,
            });
            hidden = before - facts.len();
        }
        let n = facts.len() + others.len() + usize::from(hidden > 0);
        let mark = |i: usize| {
            if i + 1 == n {
                ("└─ ", "   ")
            } else {
                ("├─ ", "│  ")
            }
        };
        for (i, f) in facts.iter().enumerate() {
            let (b, p) = mark(i);
            let note = if aggregate { self.owner(*f) } else { None };
            self.fact(
                w,
                *f,
                &format!("{pad}{b}"),
                &format!("{pad}{p}"),
                note,
                None,
            );
        }
        for (j, l) in others.iter().enumerate() {
            let (b, _) = mark(facts.len() + j);
            w.out
                .push_str(&format!("{pad}{b}{}\n", self.redact.text(&leaf_text(l))));
        }
        if hidden > 0 {
            w.out.push_str(&format!(
                "{pad}└─ ... {hidden} other contribution{} (--all)\n",
                if hidden == 1 { "" } else { "s" }
            ));
        }
    }

    /// An aggregate contribution's rank and owner: `arg/5`'s rank, and the
    /// rule or statement of each of its firings, with where it is written
    /// and the pack or module instance it came from.
    fn owner(&self, id: NodeId) -> Option<String> {
        let View::Fact { fact, alts, .. } = self.circuit.view(id) else {
            return None;
        };
        let rank = match (fact.pred.as_str(), fact.args.get(4)) {
            ("arg", Some(crate::value::Value::Str(r))) => format!("rank {r}"),
            _ => return None,
        };
        let mut owners = BTreeSet::new();
        for a in alts {
            let View::Times { children, .. } = self.circuit.view(*a) else {
                continue;
            };
            for c in children {
                match self.circuit.view(*c) {
                    View::Leaf(Leaf::Rule { id }) => {
                        owners.insert(match self.circuit.rule_at(id) {
                            Some(at) => format!("{id} ({at})"),
                            None => id.clone(),
                        });
                    }
                    View::Leaf(Leaf::Base { span }) => {
                        owners.insert(span.clone());
                    }
                    _ => {}
                }
            }
        }
        let owners: Vec<String> = owners.into_iter().collect();
        Some(format!("{rank}, owner {}", owners.join(", ")))
    }
}

fn leaf_text(l: &Leaf) -> String {
    match l {
        Leaf::Base { span } => format!("fact, {span}"),
        Leaf::Input { source } => format!("input {source}"),
        Leaf::Schema { span } => format!("provider schema {span}"),
        Leaf::World { event } => format!("world {event}"),
        Leaf::Extern { call } => format!("extern {call}"),
        Leaf::Rule { id } => format!("by {id}"),
        Leaf::Absent { pattern } => format!("not {pattern}   (absent)"),
    }
}
