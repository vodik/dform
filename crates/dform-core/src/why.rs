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

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::ir::Address;
use crate::query::Redactor;
use crate::syntax::resolve::capitalise;
use crate::syntax::{SyntaxElement, SyntaxKind, SyntaxNode};
use crate::value::Value;
use anyhow::Result;
use rowan::NodeOrToken;
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
            Leaf::Plan { tick, .. } => plan_text(*tick),
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

/// Where a planner-injected fact came from: the plan, or the apply tick.
fn plan_text(tick: Option<usize>) -> String {
    match tick {
        Some(n) => format!("plan (tick {n})"),
        None => "plan".into(),
    }
}

fn leaf_text(l: &Leaf) -> String {
    match l {
        Leaf::Base { span } => format!("fact, {span}"),
        Leaf::Input { source } => format!("input {source}"),
        Leaf::Schema { span } => format!("provider schema {span}"),
        Leaf::World { event } => format!("world {event}"),
        Leaf::Plan { fact, tick } => format!("{} {fact}", plan_text(*tick)),
        Leaf::Extern { call } => format!("extern {call}"),
        Leaf::Rule { id } => format!("by {id}"),
        Leaf::Absent { pattern } => format!("not {pattern}   (absent)"),
    }
}

// --- the tree in the program's own terms ---------------------------------

impl Printer<'_> {
    /// The derivation tree of fact node `root` as the program says it: a
    /// fired rule as its statement at `file:line`, bindings by the source's
    /// names with every computed term of the statement and its value under
    /// them, a derived fact as its address (`T["A"]`, `T["A"].p = v`), an
    /// aggregate as the contributions it merged. `rules` are the lowered
    /// rules, by the index in their id (`EvalResult::rules`). [`tree`] is the
    /// same tree in the core's spelling (`--core`).
    ///
    /// [`tree`]: Printer::tree
    pub fn source_tree(&self, rules: &[RuleStmt], root: NodeId, focus: Option<&Focus>) -> String {
        let mut s = Surface {
            p: self,
            rules,
            w: Walk::default(),
            files: BTreeMap::new(),
        };
        s.fact(root, "", "", false, focus);
        s.w.out
    }
}

struct Surface<'a, 'b> {
    p: &'a Printer<'b>,
    rules: &'a [RuleStmt],
    w: Walk,
    /// Each source file a printed rule is in, parsed once.
    files: BTreeMap<String, SyntaxNode>,
}

impl Surface<'_, '_> {
    fn push(&mut self, line: String) {
        self.w.out.push_str(&line);
        self.w.out.push('\n');
    }

    /// Print fact node `id`; `contribution`: as a contribution to the
    /// aggregate above it, its value and rank.
    fn fact(
        &mut self,
        id: NodeId,
        lead: &str,
        pad: &str,
        contribution: bool,
        focus: Option<&Focus>,
    ) {
        let circuit = self.p.circuit;
        let id = if contribution {
            id
        } else {
            self.cell_read(id).unwrap_or(id)
        };
        let View::Fact {
            fact,
            alts,
            truncated,
        } = circuit.view(id)
        else {
            self.push(format!("{lead}(retracted)"));
            return;
        };
        let text = if contribution {
            self.contribution_text(fact)
        } else {
            self.fact_text(fact)
        };
        if let [a] = alts
            && let Some(src) = self.given(*a)
        {
            // A value given on the command line is the flag that gave it.
            match src.strip_prefix("input ") {
                Some(flag) if matches!(fact.pred.as_str(), "input" | "data") => {
                    self.push(format!("{lead}{flag}"))
                }
                _ => self.push(format!("{lead}{text}   {src}")),
            }
            return;
        }
        if !self.w.seen.insert(id) {
            self.push(format!("{lead}{text}   (see above)"));
            return;
        }
        self.push(format!("{lead}{text}"));
        let shown = if self.p.all { alts.len() } else { 1 };
        for (k, a) in alts.iter().take(shown).enumerate() {
            if alts.len() > 1 && self.p.all {
                self.push(format!("{pad}  alternative {} of {}:", k + 1, alts.len()));
            }
            self.firing(*a, &format!("{pad}  "), focus);
        }
        if alts.len() > shown {
            let more = alts.len() - shown;
            self.push(format!(
                "{pad}  ... {more} more alternative{} (--all)",
                if more == 1 { "" } else { "s" }
            ));
        }
        if truncated {
            self.push(format!(
                "{pad}  ... further alternatives dropped at {}",
                crate::circuit::MAX_ALTS
            ));
        }
    }

    /// The cell fact node `id` only reads: `env("prod")` derived from
    /// `input env = "prod"` by the rule that reads the input by its name.
    /// The program writes `env`, so the tree shows the cell.
    fn cell_read(&self, id: NodeId) -> Option<NodeId> {
        let c = self.p.circuit;
        let View::Fact {
            fact, alts: [a], ..
        } = c.view(id)
        else {
            return None;
        };
        if matches!(fact.pred.as_str(), "attr" | "arg" | "want") {
            return None;
        }
        let View::Times { children, .. } = c.view(*a) else {
            return None;
        };
        let mut rule = None;
        let mut read = None;
        for ch in children {
            match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) if rule.is_none() => rule = Some(id),
                View::Fact { fact, .. } if read.is_none() => read = Some((*ch, fact)),
                _ => return None,
            }
        }
        let r = self
            .rules
            .get(rule?.strip_prefix('r')?.parse::<usize>().ok()?)?;
        let (node, cell) = read?;
        let kind = cell.args.first().and_then(Value::as_str);
        (r.body.len() == 1
            && cell.pred == "attr"
            && matches!(kind, Some("input" | "let" | "output")))
        .then_some(node)
    }

    /// A derived fact as the program would name it.
    fn fact_text(&self, f: &Fact) -> String {
        let r = self.p.redact;
        match (f.pred.as_str(), f.args.as_slice()) {
            ("want", [Value::Str(t), Value::Str(a)]) => Address {
                typ: t.clone(),
                name: a.clone(),
            }
            .to_string(),
            ("attr", [Value::Str(t), Value::Str(a), Value::Str(p), v]) => {
                format!("{} = {}", cell(t, a, p), r.surface(v))
            }
            ("arg", [Value::Str(t), Value::Str(a), Value::Str(p), v, rank]) => {
                format!("{} = {}{}", cell(t, a, p), r.surface(v), rank_text(rank))
            }
            ("deny" | "warn", [msg @ Value::Str(_), ctx @ ..]) => {
                let mut out = format!("{} {}", f.pred, r.surface(msg));
                for c in ctx {
                    out.push(' ');
                    out.push_str(&r.surface(c));
                }
                out
            }
            _ => r.surface_atom(&atom_of(f)),
        }
    }

    /// A contribution under its aggregate: the value it contributes and its
    /// rank (the cell is the aggregate's, printed above).
    fn contribution_text(&self, f: &Fact) -> String {
        match f.args.as_slice() {
            [_, _, _, v, rank] if f.pred == "arg" => {
                format!("{}{}", self.p.redact.surface(v), rank_text(rank))
            }
            _ => self.fact_text(f),
        }
    }

    /// A fact the firing found absent, spelled as the program would; its
    /// core text when it does not read back as one ground fact.
    fn absent(&self, pattern: &str) -> String {
        let fact = match crate::query::parse(pattern) {
            Ok(crate::query::Query::Body { body, vars }) if vars.is_empty() => {
                match body.as_slice() {
                    [Lit::Pos(a)] => a
                        .args
                        .iter()
                        .map(|t| match t {
                            Term::Val(v) => Some(v.clone()),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(|args| Fact::new(&a.pred, args)),
                    _ => None,
                }
            }
            _ => None,
        };
        match fact {
            Some(f) if crate::partition::fmt_atom(&atom_of(&f)) == pattern => self.fact_text(&f),
            _ => self.p.redact.text(pattern),
        }
    }

    /// Where a given fact came from, when firing `a` is a single source leaf.
    fn given(&self, a: NodeId) -> Option<String> {
        let View::Times { children, .. } = self.p.circuit.view(a) else {
            return None;
        };
        let [c] = children else { return None };
        let View::Leaf(l) = self.p.circuit.view(*c) else {
            return None;
        };
        Some(match l {
            Leaf::Base { span } => base_place(span),
            Leaf::Schema { .. } => "provider schema".into(),
            Leaf::World { .. } => "world (refresh)".into(),
            Leaf::Plan { tick, .. } => plan_text(*tick),
            Leaf::Extern { .. } => "extern".into(),
            l => self.p.redact.text(&leaf_text(l)),
        })
    }

    fn firing(&mut self, a: NodeId, pad: &str, focus: Option<&Focus>) {
        let circuit = self.p.circuit;
        let View::Times { children, bindings } = circuit.view(a) else {
            return;
        };
        let mut facts = Vec::new();
        let mut others = Vec::new();
        let mut rule = None;
        for c in children {
            match circuit.view(*c) {
                View::Leaf(Leaf::Rule { id }) => rule = Some(id.as_str()),
                View::Leaf(l) => others.push(l.clone()),
                View::Fact { .. } => facts.push(*c),
                View::Times { .. } | View::Dead => {}
            }
        }
        let aggregate = rule.is_some_and(|id| id.starts_with('Σ'));
        let mut hidden = 0;
        if aggregate {
            let n = facts.len();
            self.push(format!(
                "{pad}merged from {n} contribution{}",
                if n == 1 { "" } else { "s" }
            ));
            if let Some(focus) = focus.filter(|_| !self.p.all) {
                facts.retain(|f| match circuit.view(*f) {
                    View::Fact { fact, .. } => fact.args.get(3).is_some_and(|v| focus.holds(v)),
                    _ => true,
                });
                hidden = n - facts.len();
            }
        } else if let Some(id) = rule {
            self.statement(id, bindings, pad);
            // The facts in the order the statement reads them.
            if let Some(r) = id
                .strip_prefix('r')
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| self.rules.get(i))
            {
                let at = |f: &NodeId| match circuit.view(*f) {
                    View::Fact { fact, .. } => r
                        .body
                        .iter()
                        .position(|l| matches!(l, Lit::Pos(a) if a.pred == fact.pred))
                        .unwrap_or(usize::MAX),
                    _ => usize::MAX,
                };
                facts.sort_by_key(at);
            }
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
            self.fact(
                *f,
                &format!("{pad}{b}"),
                &format!("{pad}{p}"),
                aggregate,
                None,
            );
        }
        for (j, l) in others.iter().enumerate() {
            let (b, _) = mark(facts.len() + j);
            let text = match l {
                Leaf::Base { span } => base_place(span),
                Leaf::Absent { pattern } => format!("not {}   (absent)", self.absent(pattern)),
                l => self.p.redact.text(&leaf_text(l)),
            };
            self.push(format!("{pad}{b}{text}"));
        }
        if hidden > 0 {
            self.push(format!(
                "{pad}└─ ... {hidden} other contribution{} (--all)",
                if hidden == 1 { "" } else { "s" }
            ));
        }
    }

    /// The fired rule `id`: its statement at `file:line`, then `with` its
    /// bindings by the source's names and every computed term of the
    /// statement with its value, aligned under them.
    /// The fired rule `id` as its statement: `file:line`, its text on one
    /// line (redacted, with the pack or module instance it came from), and
    /// what of it is shown. `None` for a rule the compiler wrote.
    fn source_line(&mut self, id: &str) -> Option<(String, String, Option<Shown>)> {
        let redact = self.p.redact;
        let src = self.p.circuit.rule_source(id)?;
        let root = self
            .files
            .entry(src.file.clone())
            .or_insert_with(|| crate::syntax::parser::parse(&src.text).syntax())
            .clone();
        let origin = src
            .origin
            .as_ref()
            .map(|o| format!("   ({o})"))
            .unwrap_or_default();
        let place = format!("{}:{}", src.file, src.line);
        let Some((stmt, entry)) = statement_at(&root, src.start, src.end) else {
            let text = collapse(src.text.get(src.start..src.end).unwrap_or(""));
            return Some((place, format!("{}{origin}", redact.text(&text)), None));
        };
        let mut shown = Shown::default();
        shown.render(&stmt, &stmt, entry.as_ref());
        let text = collapse(&shown.text);
        Some((
            place,
            format!("{}{origin}", redact.text(&text)),
            Some(shown),
        ))
    }

    fn statement(&mut self, id: &str, bindings: &[(String, Value)], pad: &str) {
        let redact = self.p.redact;
        let Some((place, text, shown)) = self.source_line(id) else {
            // A rule the compiler wrote: there is no source to show.
            let text = self.p.circuit.rule_text(id).unwrap_or(id);
            self.push(format!("{pad}{}", redact.text(text)));
            if !bindings.is_empty() {
                let b: Vec<String> = bindings
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", redact.surface(v)))
                    .collect();
                self.push(format!("{pad}with {}", b.join(", ")));
            }
            return;
        };
        self.push(format!("{pad}{place}  {text}"));
        let Some(shown) = shown else { return };

        let rule = id
            .strip_prefix('r')
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.rules.get(i));
        let cx = Cx {
            env: bindings.iter().cloned().collect(),
            rule,
        };
        let mut lines = Vec::new();
        let mut names = BTreeSet::new();
        for name in shown.vars {
            let var = capitalise(&name);
            if !names.insert(name.clone()) {
                continue;
            }
            if let Some(v) = cx.env.get(&var) {
                lines.push(format!("{name} = {}", cx.show_var(&var, v, redact)));
            }
        }
        let mut terms = Vec::new();
        let mut seen = BTreeSet::new();
        for t in &shown.terms {
            let text = match t {
                NodeOrToken::Node(n) => collapse(&n.text().to_string()),
                NodeOrToken::Token(t) => t.text().to_string(),
            };
            if !seen.insert(text.clone()) {
                continue;
            }
            if let Some(v) = cx.eval_el(t) {
                terms.push(format!("{text} = {}", redact.surface(&v)));
            }
        }
        let mut rest = terms.into_iter();
        let first = if lines.is_empty() {
            rest.next()
        } else {
            Some(lines.join(", "))
        };
        if let Some(first) = first {
            self.push(format!("{pad}with {}", redact.text(&first)));
            for t in rest {
                self.push(format!("{pad}     {}", redact.text(&t)));
            }
        }
    }
}

// --- the tree compressed to its leaves (`plan --why`, `diff`) -------------

/// One line of a deformation's explanation: the statement that derived it
/// (`kind` "rule"), or one leaf under it: a fact the program or a table
/// states ("fact"), a `--set` or `--data` ("input"), an extern's answer
/// ("extern"), a world fact ("world"), a fact of the plan ("plan"), a
/// fact found absent ("absent"), or what state alone says ("state").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Because {
    pub kind: String,
    /// `file:line` (a table's row, `path:line`), when it has a place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub text: String,
}

impl Because {
    fn new(kind: &str, at: Option<String>, text: String) -> Because {
        Because {
            kind: kind.into(),
            at,
            text,
        }
    }

    /// `by FILE:LINE  STATEMENT`, or `because [PLACE  ]FACT`.
    pub fn line(&self) -> String {
        let word = if self.kind == "rule" { "by" } else { "because" };
        match &self.at {
            Some(at) => format!("{word} {at}  {}", self.text),
            None => format!("{word} {}", self.text),
        }
    }
}

impl Printer<'_> {
    /// Why fact node `root` holds, compressed: the statement that derived
    /// it, then one line per leaf of its shortest derivation (the facts in
    /// between are dropped). An attribute (`attr`) is explained by its
    /// winning contributions, each its statement and leaves; with a focus,
    /// only the contributions that hold the focused part.
    pub fn because(&self, rules: &[RuleStmt], root: NodeId, focus: Option<&Focus>) -> Vec<Because> {
        let mut s = Surface {
            p: self,
            rules,
            w: Walk::default(),
            files: BTreeMap::new(),
        };
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        c.fact(&mut s, root, None, true, focus);
        c.out
    }

    /// Why resource `addr` is wanted ([`because`] of its `want`); `None`
    /// when the program does not want it.
    ///
    /// [`because`]: Printer::because
    pub fn want(&self, rules: &[RuleStmt], addr: &Address) -> Option<Vec<Because>> {
        let f = Fact::new(
            "want",
            vec![Value::Str(addr.typ.clone()), Value::Str(addr.name.clone())],
        );
        let id = self.circuit.fact_id(&f)?;
        Some(self.because(rules, id, None))
    }

    /// Why attribute `path` of `addr` has its value: its winning
    /// contributions; a dotted path below an object attribute
    /// (`tags.team`) only the contributions that set that part, and a
    /// list element (`rules[0]`) the list's. `None` when the program sets
    /// no such attribute.
    pub fn attr(
        &self,
        rules: &[RuleStmt],
        facts: &BTreeSet<Atom>,
        addr: &Address,
        path: &str,
    ) -> Option<Vec<Because>> {
        let path = path.split('[').next().unwrap_or(path);
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let pattern = Atom {
            pred: "attr".into(),
            args: vec![s(&addr.typ), s(&addr.name), s(path), Term::Wildcard],
            record: None,
            span: Default::default(),
        };
        let found = find(&pattern, facts).ok()?;
        let (a, focus) = found.first()?;
        let id = self.circuit.fact_id(&engine::circuit_fact(a))?;
        Some(self.because(rules, id, focus.as_ref()))
    }
}

impl Printer<'_> {
    /// Every fact the program or a table states, as the program names it,
    /// with its place: the rows `diff` compares between two evaluations.
    /// Resources, attributes and contributions are the plan's, not rows.
    pub fn stated(&self, rules: &[RuleStmt]) -> Vec<Because> {
        let s = Surface {
            p: self,
            rules,
            w: Walk::default(),
            files: BTreeMap::new(),
        };
        let c = self.circuit;
        let mut out = Vec::new();
        for f in c.facts() {
            if matches!(f.pred.as_str(), "want" | "attr" | "arg") || f.pred.starts_with("table.") {
                continue;
            }
            let Some(View::Fact { fact, alts, .. }) = c.fact_id(&f).map(|id| c.view(id)) else {
                continue;
            };
            let at = match alts {
                [a] => match c.view(*a) {
                    View::Times { children: [l], .. } => match c.view(*l) {
                        View::Leaf(Leaf::Base { span }) => Some(base_parts(span).0.to_string()),
                        _ => None,
                    },
                    _ => table_row(c, alts),
                },
                _ => None,
            };
            // A fact the compiler states has no place.
            let placed = |at: &str| {
                at.rsplit_once(':')
                    .is_some_and(|(_, l)| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
            };
            if let Some(at) = at.filter(|at| placed(at)) {
                out.push(Because::new("fact", Some(at), s.fact_text(fact)));
            }
        }
        out
    }
}

/// The compressed walk: the lines so far, and each fact node's size (the
/// leaves of its shortest derivation).
struct Compress {
    out: Vec<Because>,
    sizes: BTreeMap<NodeId, usize>,
}

impl Compress {
    fn push(&mut self, b: Because) {
        if !self.out.contains(&b) {
            self.out.push(b);
        }
    }

    /// How many leaves node `id`'s shortest derivation has.
    fn size(&mut self, c: &Circuit, id: NodeId) -> usize {
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
    fn fact(
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
            Leaf::Extern { .. } => Because::new("extern", None, text.to_string()),
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

/// Where the one table row a fact is read from is (`input p(..) from
/// csv(..)`: one firing over the row the table states), as `path:line`.
fn table_row(c: &Circuit, alts: &[NodeId]) -> Option<String> {
    let [a] = alts else { return None };
    let View::Times { children, .. } = c.view(*a) else {
        return None;
    };
    let mut row = None;
    for ch in children {
        match c.view(*ch) {
            View::Leaf(Leaf::Rule { .. }) => {}
            View::Fact { fact, alts, .. } if row.is_none() && fact.pred.starts_with("table.") => {
                row = Some(alts)
            }
            _ => return None,
        }
    }
    let [a] = row? else { return None };
    let View::Times { children: [l], .. } = c.view(*a) else {
        return None;
    };
    match c.view(*l) {
        View::Leaf(Leaf::Base { span }) => Some(base_parts(span).0.to_string()),
        _ => None,
    }
}

/// The cell an `attr` or `arg` names: `T["A"].p`, a settings row's
/// `settings["row"].p`, or an input, `let` or output by its name.
fn cell(t: &str, a: &str, p: &str) -> String {
    match t {
        "input" | "let" | "output" if a.is_empty() => format!("{t} {p}"),
        "input" | "let" | "output" => format!("{t} {a}.{p}"),
        _ => Address {
            typ: t.to_string(),
            name: a.to_string(),
        }
        .attr(p),
    }
}

/// A contribution's rank as the program writes it: nothing for normal.
fn rank_text(rank: &Value) -> String {
    match rank.as_str() {
        Some("normal") | None => String::new(),
        Some(r) => format!(" @{r}"),
    }
}

fn atom_of(f: &Fact) -> Atom {
    Atom {
        pred: f.pred.clone(),
        args: f.args.iter().cloned().map(Term::Val).collect(),
        record: None,
        span: Default::default(),
    }
}

/// A stated fact's place, `file:line:col (pred[, origin])` as the engine
/// labels it, as `file:line`, and the pack or module instance it came from.
fn base_place(span: &str) -> String {
    match base_parts(span) {
        (at, Some(o)) => format!("{at}   ({o})"),
        (at, None) => at.to_string(),
    }
}

/// A stated fact's `file:line`, and the pack or module instance it came
/// from.
fn base_parts(span: &str) -> (&str, Option<&str>) {
    let (at, rest) = span.split_once(" (").unwrap_or((span, ""));
    let origin = rest
        .strip_suffix(')')
        .and_then(|r| r.split_once(", "))
        .map(|(_, o)| o);
    // Drop the column: the last of two numeric segments.
    let at = match at.rsplit_once(':') {
        Some((head, col))
            if col.bytes().all(|b| b.is_ascii_digit())
                && head.rsplit_once(':').is_some_and(|(_, l)| {
                    !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit())
                }) =>
        {
            head
        }
        _ => at,
    };
    (at, origin)
}

/// Runs of whitespace as one space: a statement on one line.
fn collapse(s: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let mut esc = false;
    let mut space = false;
    for c in s.trim().chars() {
        if in_str {
            out.push(c);
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if c == '"' {
            in_str = true;
        }
        out.push(c);
    }
    out
}

/// The statement the span `start..end` is in, and the block entry it is
/// when it is one (a rule lowered out of a resource's or an instance's
/// `k = v`).
fn statement_at(
    root: &SyntaxNode,
    start: usize,
    end: usize,
) -> Option<(SyntaxNode, Option<SyntaxNode>)> {
    let len: usize = root.text_range().end().into();
    if start > len || end > len || start > end {
        return None;
    }
    let range = rowan::TextRange::new((start as u32).into(), (end as u32).into());
    let mut n = match root.covering_element(range) {
        NodeOrToken::Node(n) => n,
        NodeOrToken::Token(t) => t.parent()?,
    };
    loop {
        let parent = n.parent()?;
        match parent.kind() {
            SyntaxKind::SOURCE_FILE | SyntaxKind::STMT_BLOCK => return Some((n, None)),
            SyntaxKind::BLOCK if n.kind() == SyntaxKind::ASSIGN => {
                return Some((parent.parent()?, Some(n)));
            }
            _ => n = parent,
        }
    }
}

/// What of a statement is printed: its text, with the block elided but
/// for the entry that fired and a braced clause as one line, and, in
/// source order, the variables and the computed terms in that text.
#[derive(Default)]
struct Shown {
    text: String,
    vars: Vec<String>,
    terms: Vec<SyntaxElement>,
}

impl Shown {
    fn render(&mut self, stmt: &SyntaxNode, n: &SyntaxNode, entry: Option<&SyntaxNode>) {
        use SyntaxKind::*;
        for el in n.children_with_tokens() {
            match el {
                NodeOrToken::Token(t) => match t.kind() {
                    WHITESPACE | COMMENT => self.text.push(' '),
                    STRING => {
                        if has_hole(t.text()) && !negated(n, stmt) {
                            self.terms.push(NodeOrToken::Token(t.clone()));
                        }
                        self.text.push_str(t.text());
                    }
                    _ => self.text.push_str(t.text()),
                },
                NodeOrToken::Node(c) => match c.kind() {
                    BLOCK if c.parent().as_ref() == Some(stmt) => {
                        let entries: Vec<SyntaxNode> =
                            c.children().filter(|e| e.kind() == ASSIGN).collect();
                        match entry.and_then(|e| entries.iter().position(|x| x == e)) {
                            Some(i) => {
                                self.text.push_str("{ ");
                                if i > 0 {
                                    self.text.push_str(".. ");
                                }
                                self.node(stmt, &entries[i], entry);
                                if i + 1 < entries.len() {
                                    self.text.push_str(" ..");
                                }
                                self.text.push_str(" }");
                            }
                            None if entries.is_empty() => self.text.push_str("{}"),
                            None => {
                                // Elided, but its terms are the statement's.
                                for e in &entries {
                                    let mut inner = Shown::default();
                                    inner.node(stmt, e, entry);
                                    self.terms.extend(inner.terms);
                                }
                                self.text.push_str("{ .. }");
                            }
                        }
                    }
                    BODY if c.first_token().is_some_and(|t| t.kind() == L_BRACE) => {
                        let lits: Vec<SyntaxNode> = c.children().collect();
                        for (i, l) in lits.iter().enumerate() {
                            if i > 0 {
                                self.text.push_str(", ");
                            }
                            self.node(stmt, l, entry);
                        }
                    }
                    _ => self.node(stmt, &c, entry),
                },
            }
        }
    }

    /// Node `c` of `stmt`: noted (a variable, a computed term), then printed.
    fn node(&mut self, stmt: &SyntaxNode, c: &SyntaxNode, entry: Option<&SyntaxNode>) {
        use SyntaxKind::*;
        match c.kind() {
            CHAIN if !is_type_or_target(c) => {
                if let Some(name) = bare_name(c) {
                    self.vars.push(name);
                } else if !negated(c, stmt) {
                    self.terms.push(NodeOrToken::Node(c.clone()));
                }
            }
            CALL if !negated(c, stmt) => self.terms.push(NodeOrToken::Node(c.clone())),
            // `{ env }` and an entry that is only a name take the variable.
            OBJECT_FIELD | BLOCK_PATH if c.children().next().is_none() => {
                let words: Vec<_> = c
                    .children_with_tokens()
                    .filter(|t| !t.kind().is_trivia())
                    .collect();
                let shorthand = c.kind() == OBJECT_FIELD
                    || c.parent().is_some_and(|a| {
                        a.children_with_tokens()
                            .filter(|t| !t.kind().is_trivia())
                            .count()
                            == 1
                    });
                if let [NodeOrToken::Token(t)] = words.as_slice()
                    && t.kind() == IDENT
                    && shorthand
                {
                    self.vars.push(t.text().to_string());
                }
            }
            _ => {}
        }
        // A chain prints as written; only its index terms are noted.
        if c.kind() == CHAIN {
            self.text.push_str(&c.text().to_string());
            for ix in c.children().filter(|x| x.kind() == INDEX) {
                for t in ix.children() {
                    let mut inner = Shown::default();
                    inner.node(stmt, &t, entry);
                    self.vars.extend(inner.vars);
                    self.terms.extend(inner.terms);
                }
            }
            return;
        }
        self.render(stmt, c, entry);
    }
}

/// A chain that is a type (`x in T`), a function's name, or the target of
/// a `set`: not a value of the statement.
fn is_type_or_target(c: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    let Some(parent) = c.parent() else {
        return false;
    };
    let first = parent.children().next().as_ref() == Some(c);
    match parent.kind() {
        CALL | SET => first,
        LIT_IN | LIT_NOT_IN => !first,
        _ => false,
    }
}

/// The name of a chain that is one word: a variable (or a cell read by
/// its name).
fn bare_name(c: &SyntaxNode) -> Option<String> {
    let mut words = c.children_with_tokens().filter(|t| !t.kind().is_trivia());
    match (words.next(), words.next()) {
        (Some(NodeOrToken::Token(t)), None) if t.kind() == SyntaxKind::IDENT => {
            Some(t.text().to_string())
        }
        _ => None,
    }
}

/// Under a `not` within `stmt`: what it names was not found, so it has no
/// value to show.
fn negated(n: &SyntaxNode, stmt: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    n.ancestors()
        .take_while(|a| a != stmt)
        .any(|a| matches!(a.kind(), LIT_NOT | LIT_NOT_BLOCK | LIT_NOT_IN))
}

/// A string literal holds an interpolation `${..}` (`$${` is a literal
/// `${`).
fn has_hole(text: &str) -> bool {
    holes(text).is_some_and(|parts| parts.iter().any(|p| matches!(p, Part::Hole(_))))
}

enum Part {
    Lit(String),
    Hole(String),
}

/// A string literal's text and holes, as `syntax::resolve` reads them.
fn holes(text: &str) -> Option<Vec<Part>> {
    let inner = text.get(1..text.len().checked_sub(1)?)?;
    let bytes = inner.as_bytes();
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let end = if bytes.get(i + 1) == Some(&b'u') {
                    inner[i..].find('}').map_or(i + 2, |e| i + e + 1)
                } else {
                    i + 2
                };
                lit.push_str(inner.get(i..end.min(inner.len()))?);
                i = end;
            }
            b'$' if bytes.get(i + 1) == Some(&b'$') && bytes.get(i + 2) == Some(&b'{') => {
                lit.push_str("${");
                i += 3;
            }
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                let mut depth = 1;
                let mut j = i + 2;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                if depth > 0 {
                    return None;
                }
                parts.push(Part::Lit(std::mem::take(&mut lit)));
                parts.push(Part::Hole(inner[i + 2..j - 1].to_string()));
                i = j;
            }
            _ => {
                let c = inner[i..].chars().next()?;
                lit.push(c);
                i += c.len_utf8();
            }
        }
    }
    parts.push(Part::Lit(lit));
    Some(parts)
}

/// One firing's bindings and its lowered rule: what a term of the
/// statement evaluates against.
struct Cx<'a> {
    env: std::collections::HashMap<String, Value>,
    rule: Option<&'a RuleStmt>,
}

impl Cx<'_> {
    /// A variable's value; one that ranges over a type's resources
    /// (`r in T`) as the resource's address.
    fn show_var(&self, var: &str, v: &Value, redact: &Redactor) -> String {
        let typ = self.rule.and_then(|r| {
            r.body.iter().find_map(|l| match l {
                Lit::Pos(a)
                    if a.pred == "want"
                        && matches!(a.args.get(1), Some(Term::Var(x)) if x == var) =>
                {
                    self.core(&a.args[0])
                }
                _ => None,
            })
        });
        match (typ, v) {
            (Some(Value::Str(t)), Value::Str(name)) if !redact.is_secret(v) => Address {
                typ: t,
                name: name.clone(),
            }
            .to_string(),
            _ => redact.surface(v),
        }
    }

    /// A lowered term's value under the bindings.
    fn core(&self, t: &Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => self.env.get(x).cloned(),
            Term::Func { name, args } => {
                let args = args
                    .iter()
                    .map(|a| self.core(a))
                    .collect::<Option<Vec<_>>>()?;
                call(name, &args)
            }
            Term::List(xs) => xs
                .iter()
                .map(|x| self.core(x))
                .collect::<Option<_>>()
                .map(Value::List),
            Term::Obj(m) => m
                .iter()
                .map(|(k, x)| Some((k.clone(), self.core(x)?)))
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Value::Obj),
            Term::Wildcard | Term::ListComp { .. } => None,
        }
    }

    /// The rule's positive literals: what its firing found.
    fn found(&self) -> impl Iterator<Item = &Atom> {
        self.rule
            .into_iter()
            .flat_map(|r| &r.body)
            .filter_map(|l| match l {
                Lit::Pos(a) => Some(a),
                _ => None,
            })
    }

    fn eval_el(&self, el: &SyntaxElement) -> Option<Value> {
        match el {
            NodeOrToken::Node(n) => self.eval(n),
            NodeOrToken::Token(t) if t.kind() == SyntaxKind::STRING => self.string(t.text()),
            NodeOrToken::Token(_) => None,
        }
    }

    /// A source term's value under the firing's bindings: literals,
    /// variables, interpolations and calls computed again; a read or a
    /// lookup is the value the firing found for it.
    fn eval(&self, n: &SyntaxNode) -> Option<Value> {
        use SyntaxKind::*;
        match n.kind() {
            LITERAL => {
                let t = n
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .find(|t| !t.kind().is_trivia())?;
                match t.kind() {
                    INT => t.text().parse().ok().map(Value::Int),
                    STRING => self.string(t.text()),
                    TRUE_KW => Some(Value::Bool(true)),
                    FALSE_KW => Some(Value::Bool(false)),
                    _ => None,
                }
            }
            PAREN => self.eval(&n.children().next()?),
            CHAIN => self.chain(n),
            CALL => {
                let mut kids = n.children();
                let name: String = kids.next()?.text().to_string().split_whitespace().collect();
                let args = kids.next().filter(|a| a.kind() == ARG_LIST)?;
                let args = args
                    .children()
                    .map(|a| (a.kind() != NAMED_ARG).then(|| self.eval(&a)).flatten())
                    .collect::<Option<Vec<_>>>()?;
                call(&name, &args)
            }
            LIST => n
                .children()
                .map(|x| self.eval(&x))
                .collect::<Option<_>>()
                .map(Value::List),
            OBJECT => n
                .children()
                .filter(|f| f.kind() == OBJECT_FIELD)
                .map(|f| {
                    let key = f
                        .children_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .find(|t| t.kind() == IDENT)?;
                    let v = match f.children().next() {
                        Some(v) => self.eval(&v)?,
                        None => self.env.get(&capitalise(key.text()))?.clone(),
                    };
                    Some((key.text().to_string(), v))
                })
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Value::Obj),
            _ => None,
        }
    }

    /// A string literal's value, its holes filled.
    fn string(&self, text: &str) -> Option<Value> {
        let mut out = String::new();
        for p in holes(text)? {
            match p {
                Part::Lit(l) => {
                    out.push_str(&crate::syntax::resolve::unescape(&format!("\"{l}\"")).ok()?)
                }
                Part::Hole(h) => {
                    let parse = crate::syntax::parser::parse_term(&h);
                    let t = parse.syntax().children().next()?;
                    match self.eval(&t)? {
                        Value::Str(s) => out.push_str(&s),
                        v if crate::stuck::has_null(&v) => return None,
                        v => out.push_str(&crate::partition::fmt_value(&v)),
                    }
                }
            }
        }
        Some(Value::Str(out))
    }

    /// A chain: a variable, a read (`x.p`, `cfg.db.size`, `T[k].p`), or a
    /// lookup (`T[k]`, a relation's `rel[k]`).
    fn chain(&self, c: &SyntaxNode) -> Option<Value> {
        enum Seg {
            Name(String),
            Index(Vec<SyntaxNode>),
        }
        let mut segs = Vec::new();
        for el in c.children_with_tokens() {
            match el {
                NodeOrToken::Node(ix) if ix.kind() == SyntaxKind::INDEX => {
                    segs.push(Seg::Index(ix.children().collect()))
                }
                NodeOrToken::Token(t)
                    if !t.kind().is_trivia()
                        && !matches!(t.kind(), SyntaxKind::DOT | SyntaxKind::STRING) =>
                {
                    segs.push(Seg::Name(t.text().to_string()))
                }
                NodeOrToken::Token(t) if t.kind() == SyntaxKind::STRING => {
                    segs.push(Seg::Name(self.string(t.text())?.as_str()?.to_string()))
                }
                _ => {}
            }
        }
        let at = segs.iter().position(|s| matches!(s, Seg::Index(_)));
        let names = |s: &[Seg]| -> Option<Vec<String>> {
            s.iter()
                .map(|x| match x {
                    Seg::Name(n) => Some(n.clone()),
                    Seg::Index(_) => None,
                })
                .collect()
        };
        let Some(at) = at else {
            let names = names(&segs)?;
            let head = names.first()?;
            if names.len() == 1 {
                return self.env.get(&capitalise(head)).cloned();
            }
            // A variable bound to an object: its field.
            if let Some(mut v) = self
                .env
                .get(&capitalise(head))
                .filter(|v| matches!(v, Value::Obj(_)))
            {
                for k in &names[1..] {
                    let Value::Obj(m) = v else { return None };
                    v = m.get(k)?;
                }
                return Some(v.clone());
            }
            for i in 1..names.len() {
                let owner = names[..i].join(".");
                let path = names[i..].join(".");
                if let Some(v) = self.read(&path, |a| self.owns(a, &owner)) {
                    return Some(v);
                }
            }
            return None;
        };
        let base = names(&segs[..at])?.join(".");
        let Seg::Index(keys) = &segs[at] else {
            return None;
        };
        let keys = keys
            .iter()
            .map(|k| self.eval(k))
            .collect::<Option<Vec<_>>>()?;
        let rest = names(&segs[at + 1..])?;
        if rest.is_empty() {
            // A relation's row (`zone_index[z]`): its last column.
            let row = self.found().find(|a| {
                (a.pred == base || a.pred.ends_with(&format!("::{base}")))
                    && a.args.len() == keys.len() + 1
                    && a.args
                        .iter()
                        .zip(&keys)
                        .all(|(t, k)| self.core(t).as_ref() == Some(k))
            });
            if let Some(a) = row {
                return self.core(a.args.last()?);
            }
            let [Value::Str(name)] = keys.as_slice() else {
                return None;
            };
            return Some(Value::Ref {
                typ: base,
                name: name.clone(),
                attr: String::new(),
            });
        }
        let [key] = keys.as_slice() else { return None };
        let instance = key.as_str().map(|k| format!("{base}.{k}"));
        let path = rest.join(".");
        let read = self.read(&path, |a| match self.core(a) {
            Some(v) if v == *key => true,
            Some(Value::Str(s)) => Some(&s) == instance.as_ref(),
            _ => false,
        });
        // Not read by the firing: a reference to the attribute, passed on.
        read.or_else(|| {
            Some(Value::Ref {
                typ: base,
                name: key.as_str()?.to_string(),
                attr: path,
            })
        })
    }

    /// The value the firing read at `path` of an owner `owns` accepts.
    fn read(&self, path: &str, owns: impl Fn(&Term) -> bool) -> Option<Value> {
        self.found().find_map(|a| match a.args.as_slice() {
            [_, owner, p, v]
                if a.pred == "attr"
                    && self.core(p).as_ref().and_then(Value::as_str) == Some(path)
                    && owns(owner) =>
            {
                self.core(v)
            }
            _ => None,
        })
    }

    /// The lowered owner `a` is what the source calls `name`: its variable,
    /// or the resource, module instance or cell of that name.
    fn owns(&self, a: &Term, name: &str) -> bool {
        if matches!(a, Term::Var(x) if *x == capitalise(name)) {
            return true;
        }
        match self.core(a) {
            Some(Value::Str(s)) => s == name || s.ends_with(&format!("::{name}")),
            _ => false,
        }
    }
}

/// A function's value at `args`, as the engine computes it; none over a
/// null.
fn call(name: &str, args: &[Value]) -> Option<Value> {
    if args.iter().any(crate::stuck::has_null) {
        return None;
    }
    engine::body(name)?(args)
}
