//! The derivation in the core's spelling (`Printer::tree`, `--core`): each fact
//! with the firing that derived it and that firing's children, a given fact with
//! where it came from; the facts a pattern names and the part of an object it
//! focuses on (`find`, `Focus`).

use crate::address::{Step, Tok};
use crate::ast::{Atom, Lit, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::query::Redactor;
use crate::report::fold;
use crate::value::Value;
use anyhow::Result;
use std::collections::BTreeSet;

/// The part of an object attribute a pattern named: the keys below the
/// top-level path, the steps past a list after them, as the plan prints
/// them (`[port=5432,protocol=TCP].protocol`), and the value there
/// (`None`: any).
#[derive(Debug, Clone, PartialEq)]
pub struct Focus {
    pub(super) keys: Vec<String>,
    pub(super) below: Vec<Tok>,
    pub(super) value: Option<Value>,
}

/// The part of an object at `keys`, any value.
impl From<Vec<String>> for Focus {
    fn from(keys: Vec<String>) -> Focus {
        Focus {
            keys,
            below: Vec::new(),
            value: None,
        }
    }
}

impl Focus {
    /// The keys below the attribute's top-level path.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The steps past a list below [`Focus::keys`]: an element by its key
    /// or position, and the keys inside it.
    pub fn below(&self) -> &[Tok] {
        &self.below
    }

    /// The part of `v` at the keys.
    fn at<'v>(&self, v: &'v Value) -> Option<&'v Value> {
        self.keys.iter().try_fold(v, |at, k| match at {
            Value::Obj(m) => m.get(k),
            _ => None,
        })
    }

    /// A contribution's value `v` holds the part: its keys, and the value
    /// there (an element past a list is not one a contribution need
    /// hold whole: the merge may have defaulted its keys).
    pub(super) fn holds(&self, v: &Value) -> bool {
        self.at(v).is_some_and(|at| {
            !self.below.is_empty() || self.value.as_ref().is_none_or(|w| contains(at, w))
        })
    }

    /// The merged value `v` has the part the pattern named: its keys,
    /// the element and the keys past a list, and the value there.
    fn named_in(&self, v: &Value) -> bool {
        self.at(v)
            .and_then(|at| fold::reach(at, &self.below))
            .is_some_and(|at| self.value.as_ref().is_none_or(|w| contains(at, w)))
    }
}

/// `v` has `w`: equal, or an object with every key of object `w`, recursively.
pub(super) fn contains(v: &Value, w: &Value) -> bool {
    match (v, w) {
        (Value::Obj(vm), Value::Obj(wm)) => wm
            .iter()
            .all(|(k, wv)| vm.get(k).is_some_and(|vv| contains(vv, wv))),
        _ => v == w,
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
    // A quoted segment is one key (R-77): `annotations."a.b/c"`; past a
    // list, the element by its key or position as the plan prints it,
    // `ports[port=5432,protocol=TCP].protocol`.
    let toks = crate::address::tokens(path);
    let Some(Tok {
        step: Step::Key(top),
        ..
    }) = toks.first()
    else {
        return Ok(vec![]);
    };
    let top = top.clone();
    let keys: Vec<String> = toks[1..]
        .iter()
        .map_while(|t| match &t.step {
            Step::Key(k) => Some(crate::address::segment_key(k).into_owned()),
            _ => None,
        })
        .collect();
    let below = toks[1 + keys.len()..].to_vec();
    let value = match &pattern.args[3] {
        Term::Var(_) | Term::Wildcard => None,
        t => match t.ground() {
            Some(v) => Some(v),
            None => return Ok(vec![]),
        },
    };
    if keys.is_empty() && below.is_empty() && !matches!(value, Some(Value::Obj(_))) {
        return Ok(vec![]);
    }
    let focus = Focus { keys, below, value };
    let mut relaxed = pattern.clone();
    relaxed.args[2] = Term::Val(Value::Str(top));
    relaxed.args[3] = Term::Wildcard;
    Ok(matched(&relaxed)?
        .into_iter()
        .filter(|a| matches!(&a.args[3], Term::Val(v) if focus.named_in(v)))
        .map(|a| (a, Some(focus.clone())))
        .collect())
}

/// The output so far, and the facts already expanded in it.
#[derive(Default)]
pub(super) struct Walk {
    pub(super) out: String,
    pub(super) seen: BTreeSet<NodeId>,
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

    pub(super) fn fact_text(&self, f: &Fact) -> String {
        self.redact.fmt_atom(&f.atom())
    }

    /// Print fact node `id`: its line after `lead`, the rest after `pad`.
    /// `note` is appended to the fact's line (rank and owner of an
    /// aggregate contribution).
    pub(super) fn fact(
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
    pub(super) fn given(&self, a: NodeId) -> Option<String> {
        let View::Times { children, .. } = self.circuit.view(a) else {
            return None;
        };
        let [c] = children else { return None };
        let View::Leaf(l) = self.circuit.view(*c) else {
            return None;
        };
        Some(match l {
            Leaf::Schema { .. } => "provider schema".into(),
            Leaf::World { event } => world_text(event),
            Leaf::Plan { tick, .. } => plan_text(*tick),
            Leaf::Extern { call } => crate::memo::source(call),
            l => self.redact.text(&leaf_text(l)),
        })
    }

    pub(super) fn firing(&self, w: &mut Walk, a: NodeId, pad: &str, focus: Option<&Focus>) {
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
        let mark = |i: usize| branch(i, n);
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
    pub(super) fn owner(&self, id: NodeId) -> Option<String> {
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

/// Where a planner-injected fact came from: the plan, or the apply tick.
pub(super) fn plan_text(tick: Option<usize>) -> String {
    match tick {
        Some(n) => format!("plan (tick {n})"),
        None => "plan".into(),
    }
}

/// Where a world leaf's fact came from: a deployment's published outputs
/// (`stack::published`), else the refresh.
pub(super) fn world_text(event: &str) -> String {
    match event.starts_with(crate::stack::PUBLISHED) || event.ends_with(crate::stack::NOT_APPLIED) {
        true => event.to_string(),
        false => "world (refresh)".into(),
    }
}

pub(super) fn leaf_text(l: &Leaf) -> String {
    match l {
        Leaf::Base { span } => format!("fact, {span}"),
        Leaf::Input { source } => format!("input {source}"),
        Leaf::Schema { span } => format!("provider schema {span}"),
        Leaf::World { event } => format!("world {event}"),
        Leaf::Plan { fact, tick } => format!("{} {fact}", plan_text(*tick)),
        Leaf::Extern { call } => call.clone(),
        Leaf::Rule { id } => format!("by {id}"),
        Leaf::Absent { atom } => format!("not {}   (absent)", crate::spell::atom(atom)),
    }
}

/// The glyphs of entry `i` of `n` hung off a tree: its own line's
/// (`├─ `, the last's `└─ `) and the lines under it (`│  `, the last's
/// blank): what `why --tree` and a diagnostic's sites draw.
pub fn branch(i: usize, n: usize) -> (&'static str, &'static str) {
    match i + 1 == n {
        true => ("└─ ", "   "),
        false => ("├─ ", "│  "),
    }
}
