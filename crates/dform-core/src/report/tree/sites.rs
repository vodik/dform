//! Where a fact is derived, terse (`Site`): the statement of its shortest
//! derivation and its place, a winning value followed to where it was written,
//! a stated fact's place, the cell an `attr` or `arg` names.

use super::chains::{
    FOLLOW, contribution_focus, field_of, holds, passed_cell, placeholder, rank_of,
};
use super::compress::Compress;
use super::printer::{Focus, Printer};
use super::statement::{Cx, collapse, is_check, statement_at};
use super::surface::Surface;
use crate::ast::{Atom, RuleStmt, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::ir::Address;
use crate::syntax::resolve::capitalise;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Where a fact is derived, terse: the statement of its shortest
/// derivation at `file:line` with the statement's variables bound, or
/// where it is stated. An attribute's site is the statement that wrote
/// its winning value, followed through the inputs and `let`s that pass
/// the value on unchanged, with the rank it won at and the rank it beat.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Site {
    /// `file:line`; empty for a value given on the command line.
    pub at: String,
    /// The statement on one line, its block elided but for the entry
    /// that fired; a stated fact as the program names it; a flag as given.
    pub statement: String,
    /// The block entry that fired, alone (`max = nodepool_max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The statement's variables with their values (`az = "us-east-1c"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    /// The pack or module instance it came from (`use synapse`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// An attribute's winning rank, when it is not normal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    /// The highest rank of the contributions the winner overrode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beat: Option<String>,
    /// Where the contribution it overrode was written (`file:line`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beat_at: Option<String>,
    /// The statement's file and first line: two sites in one statement
    /// share it.
    #[serde(skip)]
    pub stmt: Option<(String, usize)>,
    /// The statement's last line: a constant stated inside its block is
    /// one of its entries.
    #[serde(skip)]
    pub last: usize,
    /// A constant the program states of a resource (no rule fired): its
    /// entry says no more than its value, unless it reads something.
    #[serde(skip)]
    pub stated: bool,
}

/// The cell an `attr` or `arg` of an input, a `let` or an output names,
/// as a [`Because`]'s text spells it (`input kubernetes.nodepool_max`).
pub fn cell_name(t: &str, a: &str, p: &str) -> String {
    cell(t, a, p)
}

/// Where the one table row a fact is read from is (`input p(..) from
/// csv(..)`: one firing over the row the table states), as `path:line`.
pub(super) fn table_row(c: &Circuit, alts: &[NodeId]) -> Option<String> {
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

/// The cell an `attr` or `arg` names: `T k3s.server.p` (R-111), or an
/// input, `let` or output by its name.
pub(super) fn cell(t: &str, a: &str, p: &str) -> String {
    match t {
        "input" | "let" | "output" if a.is_empty() => format!("{t} {p}"),
        "input" | "let" | "output" => format!("{t} {a}.{p}"),
        _ => crate::report::attribute(
            &Address {
                typ: t.to_string(),
                name: a.to_string(),
            },
            p,
        ),
    }
}

/// A contribution's rank as the program writes it: nothing for normal.
pub(super) fn rank_text(rank: &Value) -> String {
    match rank.as_str() {
        Some("normal") | None => String::new(),
        Some(r) => format!(" @{r}"),
    }
}

/// A stated fact's place, `file:line:col (pred[, origin])` as the engine
/// labels it, as `file:line`, and the pack or module instance it came from.
pub(super) fn base_place(span: &str) -> String {
    match base_parts(span) {
        (at, Some(o)) => format!("{at}   ({o})"),
        (at, None) => at.to_string(),
    }
}

/// A stated fact's `file:line`, and the pack or module instance it came
/// from.
pub(super) fn base_parts(span: &str) -> (&str, Option<&str>) {
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

impl Printer<'_> {
    /// Where resource `addr` is derived: the site of its `want`. `None`
    /// when the program does not want it.
    pub fn want_site(&self, rules: &[RuleStmt], addr: &Address) -> Option<Site> {
        let f = Fact::new(
            "want",
            vec![Value::Str(addr.typ.clone()), Value::Str(addr.name.clone())],
        );
        self.site(rules, self.circuit.fact_id(&f)?)
    }

    /// Where rule `id` (`r12`) is written, with `bindings` for the
    /// variables it shows: a rule that has not fired (a pending group's,
    /// an undetermined deny's). `None` for a rule the compiler wrote.
    pub fn rule_site(
        &self,
        rules: &[RuleStmt],
        id: &str,
        bindings: &[(String, Value)],
    ) -> Option<Site> {
        self.surface(rules).site(id, bindings)
    }

    /// Where fact node `id` is derived.
    pub fn site(&self, rules: &[RuleStmt], id: NodeId) -> Option<Site> {
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        s.site_of(&mut c, id, 0)
    }

    /// Where the winning value of attribute fact `attr` (an `attr/4`),
    /// at `keys` below it when they name part of an object, was written.
    /// `whole`: the keys reach the leaf asked about; when they stop short
    /// (at a list element), only a single winning contribution says.
    /// `None` when no statement of the program wrote it.
    pub fn attr_site(
        &self,
        rules: &[RuleStmt],
        attr: &Atom,
        keys: &[String],
        whole: bool,
    ) -> Option<Site> {
        self.attr_sites(rules, attr, &[(keys.to_vec(), whole)])
            .pop()
            .flatten()
    }

    /// [`Printer::attr_site`] of each of `asks` (keys, whole) below one
    /// attribute fact: the fact is found once, and the sites its leaves
    /// share are found once.
    pub fn attr_sites(
        &self,
        rules: &[RuleStmt],
        attr: &Atom,
        asks: &[(Vec<String>, bool)],
    ) -> Vec<Option<Site>> {
        let Some(id) = self.circuit.fact_id(&engine::circuit_fact(attr)) else {
            return vec![None; asks.len()];
        };
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        asks.iter()
            .map(|(keys, whole)| {
                let focus = (!keys.is_empty()).then(|| Focus {
                    keys: keys.clone(),
                    value: None,
                });
                s.winner_site(&mut c, id, focus.as_ref(), !whole, 0)
            })
            .collect()
    }
}

impl Printer<'_> {
    /// Which contribution to attribute fact `attr` (an `attr/4`) wrote
    /// each leaf of `paths` (as the plan prints them, from the resource:
    /// `spec.containers[name=web].image`): the winning `arg` that holds
    /// it, the attribute itself when no aggregate made it, `None` when no
    /// contribution holds it (a schema's default of a merge key).
    pub fn writers(&self, attr: &Atom, paths: &[String]) -> Vec<Option<NodeId>> {
        let none = || vec![None; paths.len()];
        let Some(id) = self.circuit.fact_id(&engine::circuit_fact(attr)) else {
            return none();
        };
        let View::Fact { alts, .. } = self.circuit.view(id) else {
            return none();
        };
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        let Some(&alt) = alts.iter().min_by_key(|a| c.size(self.circuit, **a)) else {
            return none();
        };
        let View::Times { children, .. } = self.circuit.view(alt) else {
            return none();
        };
        let aggregate = children.iter().any(
            |ch| matches!(self.circuit.view(*ch), View::Leaf(Leaf::Rule { id }) if id.starts_with('Σ')),
        );
        if !aggregate {
            return vec![Some(id); paths.len()];
        }
        let contributions: Vec<(NodeId, &Fact)> = children
            .iter()
            .filter_map(|ch| match self.circuit.view(*ch) {
                View::Fact { fact, .. } if fact.pred == "arg" && !is_check(fact) => {
                    Some((*ch, fact))
                }
                _ => None,
            })
            .collect();
        let (top, merged) = match attr.args.as_slice() {
            [_, _, Term::Val(Value::Str(p)), Term::Val(v)] => {
                (crate::report::fold::tokens(p).len(), v)
            }
            _ => return none(),
        };
        // Each contribution followed to the leaves once per prefix: a
        // list's element is matched against the merged list's once, not
        // once per leaf under it (a CRD's `versions[0]` holds hundreds).
        let mut memos: Vec<crate::report::fold::Reached> =
            vec![std::collections::HashMap::new(); contributions.len()];
        paths
            .iter()
            .map(|p| {
                let toks = crate::report::fold::tokens(p);
                contributions
                    .iter()
                    .zip(memos.iter_mut())
                    .filter_map(|(c, memo)| holds(memo, c.1, &toks, merged, top).then_some(c))
                    .max_by_key(|(id, f)| (rank_of(f).0, std::cmp::Reverse(*id)))
                    .map(|(id, _)| *id)
            })
            .collect()
    }

    /// Where contribution `id` (an `arg`, or an attribute no aggregate
    /// made) wrote the part of its value at printed path `path`, with its
    /// rank when it is not normal.
    pub fn contribution_site(&self, rules: &[RuleStmt], id: NodeId, path: &str) -> Option<Site> {
        let View::Fact { fact, .. } = self.circuit.view(id) else {
            return None;
        };
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        let focus = contribution_focus(fact, path);
        let mut site = s.value_site(&mut c, id, focus.as_ref(), 0)?;
        let rank = rank_of(fact).1;
        if fact.pred == "arg" && site.rank.is_none() && rank != "normal" {
            site.rank = Some(rank.to_string());
        }
        Some(site)
    }
}

impl Surface<'_, '_> {
    /// The site of aggregate fact `id`'s winning contribution (holding the
    /// focused part), the value followed to where it was written.
    fn winner_site(
        &mut self,
        c: &mut Compress,
        id: NodeId,
        focus: Option<&Focus>,
        single: bool,
        depth: usize,
    ) -> Option<Site> {
        let circuit = self.p.circuit;
        let View::Fact { alts, .. } = circuit.view(id) else {
            return None;
        };
        let alt = *alts.iter().min_by_key(|a| c.size(circuit, **a))?;
        let View::Times { children, .. } = circuit.view(alt) else {
            return None;
        };
        let aggregate = children.iter().any(
            |ch| matches!(circuit.view(*ch), View::Leaf(Leaf::Rule { id }) if id.starts_with('Σ')),
        );
        if !aggregate {
            return self.value_site(c, id, focus, depth);
        }
        let mut contributions: Vec<(NodeId, &Fact)> = children
            .iter()
            .filter_map(|ch| match circuit.view(*ch) {
                View::Fact { fact, .. } if fact.pred == "arg" && !is_check(fact) => {
                    Some((*ch, fact))
                }
                _ => None,
            })
            .filter(|(_, f)| match focus {
                Some(focus) => f.args.get(3).is_some_and(|v| focus.holds(v)),
                None => true,
            })
            .collect();
        contributions.sort_by_key(|(_, f)| std::cmp::Reverse(rank_of(f).0));
        let (win, fact) = *contributions.first()?;
        let (top, rank) = rank_of(fact);
        // Part of the value, not reached by the focus: one writer, or none.
        if single
            && contributions
                .iter()
                .filter(|(_, f)| rank_of(f).0 == top)
                .count()
                > 1
        {
            return None;
        }
        // The rank of a contribution the program wrote that the winner beat;
        // a provider's default is beaten by every value.
        let lower: Vec<(NodeId, &Fact)> = contributions
            .iter()
            .filter(|(_, f)| rank_of(f).0 < top && !placeholder(f, f.args.get(3)))
            .copied()
            .collect();
        let beat = lower.into_iter().find_map(|(id, f)| {
            self.site_of(c, id, depth + 1)
                .filter(|w| !w.at.is_empty() && !w.at.starts_with('<'))
                .map(|w| (rank_of(f).1.to_string(), w.at))
        });
        let mut site = self.value_site(c, win, focus, depth)?;
        if site.rank.is_none() && rank != "normal" {
            site.rank = Some(rank.to_string());
        }
        if site.beat.is_none()
            && let Some((rank, at)) = beat
        {
            site.beat = Some(rank);
            site.beat_at = Some(at);
        }
        Some(site)
    }

    /// The site of fact `id`, a contribution or a cell's value: where its
    /// firing's statement is, unless the firing only passes on the value of
    /// an input or a `let` it reads, whose winning site it is then.
    fn value_site(
        &mut self,
        c: &mut Compress,
        id: NodeId,
        focus: Option<&Focus>,
        depth: usize,
    ) -> Option<Site> {
        let circuit = self.p.circuit;
        let own = self.site_of(c, id, depth);
        let View::Fact { fact, alts, .. } = circuit.view(id) else {
            return own;
        };
        let mut value = match fact.pred.as_str() {
            "arg" | "attr" => fact.args.get(3),
            _ => None,
        };
        let entry = own.as_ref().and_then(|o| o.entry.as_deref());
        let mut rhs = entry.map(|e| e.split_once(" = ").map_or(e, |(_, r)| r).to_string());
        // The focused field of an object the entry writes, `{ d: config.d }`,
        // and its part of the value.
        if let (Some(f), Some(e)) = (focus.filter(|f| !f.keys.is_empty()), rhs.as_deref())
            && let Some(field) = field_of(e, &f.keys)
        {
            rhs = Some(field);
            value = f
                .keys
                .iter()
                .try_fold(value, |v, k| match v {
                    Some(Value::Obj(m)) => Some(m.get(k)),
                    _ => None,
                })
                .flatten();
        }
        // What the entry reads, when it is a path (`gcp.project_id`).
        let read: Option<Vec<String>> = rhs.as_deref().and_then(|rhs| {
            let plain = !rhs.is_empty()
                && !rhs.contains(|c: char| c.is_whitespace() || "()[]{}$,".contains(c));
            plain.then(|| crate::ir::path_keys(rhs))
        });
        if depth < FOLLOW
            && let Some(v) = value
            && let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a))
            && let View::Times { children, .. } = circuit.view(alt)
            && let Some((p, keys)) = passed_cell(c, circuit, children, v, read.as_deref())
        {
            let focus = (!keys.is_empty()).then_some(Focus { keys, value: None });
            if let Some(site) = self.winner_site(c, p, focus.as_ref(), false, depth + 1) {
                return Some(site);
            }
        }
        own
    }

    /// Where fact node `id` is derived: its stated place, or the statement of
    /// its shortest firing; through a firing of a rule the compiler wrote, the
    /// first fact it read that has one.
    pub(super) fn site_of(&mut self, c: &mut Compress, id: NodeId, depth: usize) -> Option<Site> {
        if let Some(site) = self.sites.get(&(id, depth)) {
            return site.clone();
        }
        let site = self.site_found(c, id, depth);
        self.sites.insert((id, depth), site.clone());
        site
    }

    /// [`site_of`], found.
    fn site_found(&mut self, c: &mut Compress, id: NodeId, depth: usize) -> Option<Site> {
        let circuit = self.p.circuit;
        let View::Fact { fact, alts, .. } = circuit.view(id) else {
            return None;
        };
        if let [a] = alts
            && let View::Times { children: [l], .. } = circuit.view(*a)
            && let View::Leaf(l) = circuit.view(*l)
        {
            return match l {
                Leaf::Base { span } => {
                    let (at, origin) = base_parts(span);
                    let stmt = at
                        .rsplit_once(':')
                        .and_then(|(f, l)| Some((f.to_string(), l.parse().ok()?)));
                    let mut site = Site {
                        at: at.to_string(),
                        statement: self.fact_text(fact),
                        origin: origin.map(str::to_string),
                        stmt,
                        // A resource's own attribute; an input's value given
                        // in a `use` says which input.
                        stated: !matches!(
                            fact.args.first().and_then(Value::as_str),
                            Some("input" | "let")
                        ),
                        ..Site::default()
                    };
                    // Stated in a block: the statement's lines, and the entry.
                    if let Some((file, first, last, entry)) = self.stated_in(span) {
                        site.stmt = Some((file, first));
                        site.last = last;
                        site.entry = entry;
                    }
                    Some(site)
                }
                Leaf::Input { source } => Some(Site {
                    statement: self.p.redact.text(source),
                    ..Site::default()
                }),
                _ => None,
            };
        }
        if let Some(at) = table_row(circuit, alts) {
            return Some(Site {
                at,
                statement: self.fact_text(fact),
                ..Site::default()
            });
        }
        let alt = *alts.iter().min_by_key(|a| c.size(circuit, **a))?;
        let View::Times { children, bindings } = circuit.view(alt) else {
            return None;
        };
        let rule = children.iter().find_map(|ch| match circuit.view(*ch) {
            View::Leaf(Leaf::Rule { id }) => Some(id.as_str()),
            _ => None,
        });
        // A value given on the command line is the flag that gave it, not the
        // declaration that reads it.
        let given = |id: NodeId| match circuit.view(id) {
            View::Leaf(Leaf::Input { source }) => Some(source),
            View::Fact { alts: [a], .. } => match circuit.view(*a) {
                View::Times { children: [l], .. } => match circuit.view(*l) {
                    View::Leaf(Leaf::Input { source }) => Some(source),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if let Some(source) = children.iter().find_map(|ch| given(*ch)) {
            return Some(Site {
                statement: self.p.redact.text(source),
                ..Site::default()
            });
        }
        if let Some(r) = rule
            && !r.starts_with('Σ')
            && let Some(site) = self.site(r, bindings)
        {
            return Some(site);
        }
        if depth >= FOLLOW {
            return None;
        }
        children.iter().find_map(|ch| match circuit.view(*ch) {
            View::Fact { .. } => self.site_of(c, *ch, depth + 1),
            _ => None,
        })
    }

    /// The statement a stated fact at `span` (`FILE:LINE:COL (..)`) is
    /// written in: its file, first and last lines, and the block entry.
    fn stated_in(&mut self, span: &str) -> Option<(String, usize, usize, Option<String>)> {
        let place = span.split_once(" (").map_or(span, |(p, _)| p);
        let (rest, col) = place.rsplit_once(':')?;
        let (file, line) = rest.rsplit_once(':')?;
        let (line, col): (usize, usize) = (line.parse().ok()?, col.parse().ok()?);
        let circuit = self.p.circuit;
        // A program of constants alone has no rule's text: the file's.
        let text: std::sync::Arc<str> = match (0..self.rules.len())
            .filter_map(|i| circuit.rule_source(&format!("r{i}")))
            .find(|r| r.file == file)
        {
            Some(r) => r.text.clone(),
            None => std::fs::read_to_string(file).ok()?.into(),
        };
        let root = self
            .files
            .entry(file.to_string())
            .or_insert_with(|| crate::syntax::parser::parse(&text).syntax())
            .clone();
        let start: usize = text
            .split_inclusive('\n')
            .take(line.checked_sub(1)?)
            .map(str::len)
            .sum();
        let at = start + col.checked_sub(1)?;
        // The first character there: an empty range at a token's edge is
        // the whitespace before it.
        let (stmt, entry) = statement_at(&root, at, (at + 1).min(text.len()))?;
        let line_at = |x: rowan::TextSize| {
            text.get(..usize::from(x))
                .unwrap_or("")
                .matches('\n')
                .count()
                + 1
        };
        let entry = entry.map(|e| self.p.redact.text(&collapse(&e.text().to_string())));
        Some((
            file.to_string(),
            line_at(stmt.text_range().start()),
            line_at(stmt.text_range().end()),
            entry,
        ))
    }

    /// The site of a firing of rule `id` with `bindings`; `None` for a
    /// rule the compiler wrote.
    pub(super) fn site(&mut self, id: &str, bindings: &[(String, Value)]) -> Option<Site> {
        let src = self.p.circuit.rule_source(id)?.clone();
        let (place, text, shown) = self.source_line(id)?;
        if place == "dform" {
            return None;
        }
        let origin = src.origin.clone();
        let statement = match &origin {
            Some(o) => text
                .strip_suffix(&format!("   ({o})"))
                .unwrap_or(&text)
                .to_string(),
            None => text,
        };
        let root = self.files.get(&src.file).cloned();
        let found = root.and_then(|r| statement_at(&r, src.start, src.end));
        let line_at = |at: rowan::TextSize| {
            src.text
                .get(..usize::from(at))
                .unwrap_or("")
                .matches('\n')
                .count()
                + 1
        };
        let stmt = found
            .as_ref()
            .map(|(n, _)| (src.file.clone(), line_at(n.text_range().start())));
        let last = found
            .as_ref()
            .map_or(0, |(n, _)| line_at(n.text_range().end()));
        let redact = self.p.redact;
        let entry = found
            .and_then(|(_, e)| e)
            .map(|e| redact.text(&collapse(&e.text().to_string())));
        let mut with = Vec::new();
        if let Some(shown) = shown {
            let rule = id
                .strip_prefix('r')
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| self.rules.get(i));
            let cx = Cx {
                env: bindings.iter().map(|(k, v)| (k.as_str(), v)).collect(),
                rule,
            };
            let mut names = BTreeSet::new();
            for name in shown.vars {
                let var = capitalise(&name);
                if !names.insert(name.clone()) {
                    continue;
                }
                if let Some(v) = cx.env.get(var.as_str()) {
                    with.push(redact.text(&format!("{name} = {}", cx.show_var(&var, v, redact))));
                }
            }
            for (name, var) in shown.each {
                if let Some(v) = cx.env.get(var.as_str()) {
                    with.push(redact.text(&format!("{name} = {}", cx.show_var(&var, v, redact))));
                }
            }
        }
        Some(Site {
            at: place,
            statement,
            entry,
            with,
            origin,
            rank: None,
            beat: None,
            beat_at: None,
            stmt,
            last,
            stated: false,
        })
    }
}
