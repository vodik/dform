//! The derivation printer (R-63): `dform why`, and under each deformation
//! of `plan --why` and `diff --since`: a fact's derivation tree, read from
//! the provenance circuit (E §3.3). Each fact prints with the firing that
//! derived it (rule id and text, the rule's bindings) and that firing's
//! children, recursively; a given fact prints with where it came from. An
//! aggregate prints every contribution with its rank and owner. A fact
//! with several alternatives shows the first and `...` for the rest unless
//! `all`; a fact already expanded above prints `(see above)`.
//!
//! An `attr` or `arg` pattern may name part of an object attribute, by a
//! dotted path (`"tags.team"`) or an object value (`{team: "platform"}`):
//! it matches the attribute that contains it, and the tree shows only the
//! contributions that do.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::ir::Address;
use crate::partition::fmt_bare;
use crate::query::Redactor;
use crate::syntax::resolve::{Piece, capitalise, pieces};
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
    /// The keys below the attribute's top-level path.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

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
    // A quoted segment is one key (R-77): `annotations."a.b/c"`.
    let mut keys = crate::ir::path_keys(path);
    let top = crate::ir::path_segments(path)[0].to_string();
    keys.remove(0);
    let value = match &pattern.args[3] {
        Term::Var(_) | Term::Wildcard => None,
        t => match t.ground() {
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
        self.redact.fmt_atom(&f.atom())
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
            Leaf::World { event } => world_text(event),
            Leaf::Plan { tick, .. } => plan_text(*tick),
            Leaf::Extern { call } => crate::memo::source(call),
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

/// Where a world leaf's fact came from: a deployment's published outputs
/// (`stack::published`), else the refresh.
fn world_text(event: &str) -> String {
    match event.starts_with(crate::stack::PUBLISHED) || event.ends_with(crate::stack::NOT_APPLIED) {
        true => event.to_string(),
        false => "world (refresh)".into(),
    }
}

fn leaf_text(l: &Leaf) -> String {
    match l {
        Leaf::Base { span } => format!("fact, {span}"),
        Leaf::Input { source } => format!("input {source}"),
        Leaf::Schema { span } => format!("provider schema {span}"),
        Leaf::World { event } => format!("world {event}"),
        Leaf::Plan { fact, tick } => format!("{} {fact}", plan_text(*tick)),
        Leaf::Extern { call } => call.clone(),
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
        let mut s = self.surface(rules);
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
    /// The keyed lists' keys, `type_list_key(T, L, Keys)`, read once.
    list_keys: std::cell::OnceCell<BTreeMap<(String, String), Vec<String>>>,
    /// Each fact node's site at a depth ([`site_of`]), found once: the
    /// leaves of one large value (a manifest's document) share their
    /// statement, whose bindings print the whole value.
    sites: std::collections::HashMap<(NodeId, usize), Option<Site>>,
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
            ("want", [Value::Str(t), Value::Str(a)]) => super::address(&Address {
                typ: t.clone(),
                name: a.clone(),
            }),
            ("attr", [Value::Str(t), Value::Str(a), Value::Str(p), v]) => {
                format!("{} = {}", cell(t, a, p), r.surface(v))
            }
            ("arg", [Value::Str(t), Value::Str(a), Value::Str(p), v, rank]) => {
                if let Some((p, content)) = self.element(t, p, v) {
                    return format!(
                        "{} = {}{}",
                        cell(t, a, &p),
                        self.content_text(v, content),
                        rank_text(rank)
                    );
                }
                format!("{} = {}{}", cell(t, a, p), r.surface(v), rank_text(rank))
            }
            // `x in T` over an enum type (R-70): the type, as declared.
            ("__enum", [Value::Str(t), Value::List(vs)]) => format!(
                "type {t} = enum({})",
                vs.iter()
                    .map(|v| r.surface(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            // A copy (R-65), as the statement that makes it.
            (
                crate::modules::INSTANCE_OF,
                [Value::Str(path), Value::Str(user), Value::Str(name)],
            ) => match user.is_empty() {
                true => format!("resource {path} {name}"),
                false => format!("resource {path} {name}   (in {user})"),
            },
            ("deny" | "warn", [msg @ Value::Str(_), ctx @ ..]) => {
                let mut out = format!("{} {}", f.pred, r.surface(msg));
                for c in ctx {
                    out.push(' ');
                    out.push_str(&r.surface(c));
                }
                out
            }
            _ => {
                let a = f.atom();
                crate::modules::private_text(&a, &|a| r.surface_atom(a), "   ")
                    .unwrap_or_else(|| r.surface_atom(&a))
            }
        }
    }

    /// A contribution under its aggregate: the value it contributes and its
    /// rank (the cell is the aggregate's, printed above).
    fn contribution_text(&self, f: &Fact) -> String {
        match f.args.as_slice() {
            [Value::Str(t), _, Value::Str(p), v, rank] if f.pred == "arg" => {
                if let Some((p, content)) = self.element(t, p, v) {
                    return format!("{p} = {}{}", self.content_text(v, content), rank_text(rank));
                }
                format!("{}{}", self.p.redact.surface(v), rank_text(rank))
            }
            [_, _, _, v, rank] if f.pred == "arg" => {
                format!("{}{}", self.p.redact.surface(v), rank_text(rank))
            }
            // A refinement is a check on the value, not a contribution.
            [.., Value::Str(c)] if is_check(f) => format!("check {c}"),
            _ => self.fact_text(f),
        }
    }

    /// An element write's content: `(sensitive)` when the write is a
    /// secret's and the content is no secret by itself (R-124 amendment 2).
    fn content_text(&self, write: &Value, content: &Value) -> String {
        let r = self.p.redact;
        match r.is_secret(write) && !r.is_secret(content) {
            true => "(sensitive)".into(),
            false => r.surface(content),
        }
    }

    /// An element write (`transform::ELEM`) by the element's key, as the
    /// plan names it: `spec.template.spec.containers[name=api]`, and what
    /// it writes there.
    fn element<'v>(&self, t: &str, p: &str, v: &'v Value) -> Option<(String, &'v Value)> {
        let list = p.strip_suffix(crate::transform::ELEM)?;
        let Value::List(kv) = v else { return None };
        let [k, content] = kv.as_slice() else {
            return None;
        };
        let keys = self.list_keys.get_or_init(|| {
            let mut out = BTreeMap::new();
            for f in self.p.circuit.facts() {
                if let ("type_list_key", [Value::Str(t), Value::Str(l), ks]) =
                    (f.pred.as_str(), f.args.as_slice())
                {
                    let ks = match ks {
                        Value::List(ks) => ks
                            .iter()
                            .filter_map(|k| k.as_str())
                            .map(String::from)
                            .collect(),
                        Value::Str(k) => vec![k.clone()],
                        _ => continue,
                    };
                    out.insert((t.clone(), l.clone()), ks);
                }
            }
            out
        });
        let label = match (keys.get(&(t.to_string(), list.to_string())), k) {
            (Some(ks), Value::Obj(m)) => ks
                .iter()
                .map(|f| Some(format!("{f}={}", fmt_bare(m.get(f)?))))
                .collect::<Option<Vec<_>>>()?
                .join(","),
            (Some(ks), k) if ks.len() == 1 => format!("{}={}", ks[0], fmt_bare(k)),
            _ => fmt_bare(k),
        };
        Some((format!("{list}[{label}]"), content))
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
            Some(f) if crate::partition::fmt_atom(&f.atom()) == pattern => self.fact_text(&f),
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
            // A rule whose body reads no relation fired on nothing: it is
            // printed as any firing is, by its statement.
            Leaf::Rule { .. } => return None,
            Leaf::Base { span } => base_place(span),
            Leaf::Schema { .. } => "provider schema".into(),
            Leaf::World { event } => world_text(event),
            Leaf::Plan { tick, .. } => plan_text(*tick),
            Leaf::Extern { call } => crate::memo::source(call),
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
                // An aggregate's group (R-59), which the compiler folded in
                // a rule of its own: the group's rows, under the statement
                // that folds them.
                View::Fact { fact, alts, .. } if fact.pred.starts_with("__agg_") => {
                    let rows = alts.first().map(|a| circuit.view(*a));
                    if let Some(View::Times { children, .. }) = rows {
                        for r in children {
                            match circuit.view(*r) {
                                View::Fact { .. } => facts.push(*r),
                                View::Leaf(Leaf::Rule { .. }) => {}
                                View::Leaf(l) => others.push(l.clone()),
                                View::Times { .. } | View::Dead => {}
                            }
                        }
                    }
                }
                View::Fact { .. } => facts.push(*c),
                View::Times { .. } | View::Dead => {}
            }
        }
        let aggregate = rule.is_some_and(|id| id.starts_with('Σ'));
        let mut hidden = 0;
        if aggregate {
            // The refinements the value is checked against print below
            // its contributions, and do not count among them.
            facts.sort_by_key(
                |f| matches!(circuit.view(*f), View::Fact { fact, .. } if is_check(fact)),
            );
            let n = facts
                .iter()
                .filter(|f| !matches!(circuit.view(**f), View::Fact { fact, .. } if is_check(fact)))
                .count();
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
        // A rule the compiler wrote (`zset::POLICY_RULES`): its doc
        // comment names it, at `dform`, not its text at `<input>:N`.
        if src.file.starts_with('<')
            && let Some(name) = crate::syntax::doc::comment(&stmt).and_then(|(_, pairs)| {
                pairs
                    .into_iter()
                    .find(|(k, _)| k == "description")
                    .map(|(_, v)| v)
            })
        {
            shown.terms.clear();
            return Some(("dform".into(), name, Some(shown)));
        }
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
            env: bindings.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            rule,
        };
        let mut lines = Vec::new();
        let mut names = BTreeSet::new();
        for name in shown.vars {
            let var = capitalise(&name);
            if !names.insert(name.clone()) {
                continue;
            }
            if let Some(v) = cx.env.get(var.as_str()) {
                lines.push(format!("{name} = {}", cx.show_var(&var, v, redact)));
            }
        }
        for (name, var) in &shown.each {
            if let Some(v) = cx.env.get(var.as_str()) {
                lines.push(format!("{name} = {}", cx.show_var(var, v, redact)));
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
/// ("answered"), a world fact ("world"), a fact of the plan ("plan"), a
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
        let mut s = self.surface(rules);
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
        let s = self.surface(rules);
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

// --- where a fact is derived, on one line (`plan`'s right column, R-79) ----

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

impl Printer<'_> {
    fn surface<'a>(&'a self, rules: &'a [RuleStmt]) -> Surface<'a, 'a> {
        Surface {
            p: self,
            rules,
            w: Walk::default(),
            files: BTreeMap::new(),
            list_keys: Default::default(),
            sites: Default::default(),
        }
    }

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
        site_of(&mut s, &mut c, id, 0)
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
                winner_site(&mut s, &mut c, id, focus.as_ref(), !whole, 0)
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
            [_, _, Term::Val(Value::Str(p)), Term::Val(v)] => (super::fold::tokens(p).len(), v),
            _ => return none(),
        };
        // Each contribution followed to the leaves once per prefix: a
        // list's element is matched against the merged list's once, not
        // once per leaf under it (a CRD's `versions[0]` holds hundreds).
        let mut memos: Vec<super::fold::Reached> =
            vec![std::collections::HashMap::new(); contributions.len()];
        paths
            .iter()
            .map(|p| {
                let toks = super::fold::tokens(p);
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
        let mut site = value_site(&mut s, &mut c, id, focus.as_ref(), 0)?;
        let rank = rank_of(fact).1;
        if fact.pred == "arg" && site.rank.is_none() && rank != "normal" {
            site.rank = Some(rank.to_string());
        }
        Some(site)
    }
}

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

/// The loader documents (`table.FORMAT.document(.., At, Doc)`) a firing's
/// `children` read, and those the cells it reads read (`d in docs`, a
/// `let docs = yaml(..)`), a few steps deep.
fn documents<'c>(
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
fn structured(v: &Value) -> bool {
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
fn document_place(d: &Fact, v: &Value) -> Option<String> {
    let [.., Value::Str(at), doc] = d.args.as_slice() else {
        return None;
    };
    place_in(at, doc, v)
}

/// Where value `v` is in document `doc` read at `at` ([`document_place`]).
fn place_in(at: &str, doc: &Value, v: &Value) -> Option<String> {
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
fn locate(d: &Value, v: &Value, steps: &mut Vec<(Option<usize>, String)>) -> bool {
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
    /// The chain of the part at printed path `path` of contribution `id`
    /// ([`Printer::writers`]), as [`Printer::attr_chain`] says a value's.
    pub fn contribution_chain(
        &self,
        rules: &[RuleStmt],
        id: NodeId,
        path: &str,
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let View::Fact { fact, .. } = self.circuit.view(id) else {
            return Vec::new();
        };
        let focus = contribution_focus(fact, path);
        self.chain(rules, id, focus.as_ref(), stack_keys)
    }
}

/// The keys of printed path `path` below contribution `fact`'s own path,
/// up to a list: the part of its value the path names.
fn contribution_focus(fact: &Fact, path: &str) -> Option<Focus> {
    let at = fact.args.get(2).and_then(Value::as_str).unwrap_or_default();
    let skip = match at.ends_with(crate::transform::ELEM) {
        true => usize::MAX,
        false => super::fold::tokens(at).len(),
    };
    let keys: Vec<String> = super::fold::tokens(path)
        .into_iter()
        .skip(skip)
        .map_while(|t| match t.step {
            super::fold::Step::Key(k) => Some(crate::ir::segment_key(&k).into_owned()),
            _ => None,
        })
        .collect();
    (!keys.is_empty()).then_some(Focus { keys, value: None })
}

/// Whether contribution `f` (an `arg/5`) holds the leaf at printed path
/// `toks`: its path is a prefix, and its value has the rest.
fn holds<'v>(
    memo: &mut super::fold::Reached<'v, 'v>,
    f: &'v Fact,
    toks: &[super::fold::Tok],
    merged: &'v Value,
    top: usize,
) -> bool {
    use super::fold::Step;
    // The merged value where the contribution's path ends: a list's
    // element is found in a contribution by its value, not its position
    // in the merged list.
    let merged_at = |n: usize| super::fold::reach(merged, &toks[top.min(n)..n]);
    let (Some(at), Some(v)) = (f.args.get(2).and_then(Value::as_str), f.args.get(3)) else {
        return false;
    };
    let key = |s: &Step| match s {
        Step::Key(k) => Some(crate::ir::segment_key(k).into_owned()),
        _ => None,
    };
    let prefix = |p: &str| -> Option<usize> {
        let ptoks = super::fold::tokens(p);
        let same = ptoks.len() <= toks.len()
            && ptoks
                .iter()
                .zip(toks)
                .all(|(a, b)| key(&a.step).is_some() && key(&a.step) == key(&b.step));
        same.then_some(ptoks.len())
    };
    // An element write: `[K, V]` into the element of list `L` keyed `K`.
    if let Some(list) = at.strip_suffix(crate::transform::ELEM) {
        let (Some(n), Value::List(kv)) = (prefix(list), v) else {
            return false;
        };
        let ([k, content], Some(Step::Keyed(pairs))) =
            (kv.as_slice(), toks.get(n).map(|t| &t.step))
        else {
            return false;
        };
        let matches = match k {
            Value::Obj(_) => super::fold::keyed(k, pairs),
            k => matches!(pairs.as_slice(), [(_, want)] if fmt_bare(k) == *want),
        };
        let elem = merged_at(n + 1);
        return matches && super::fold::reach_along(content, elem, &toks[n + 1..]).is_some();
    }
    match prefix(at) {
        Some(n) => super::fold::reach_along_memo(memo, v, merged_at(n), &toks[n..]).is_some(),
        None => false,
    }
}

/// How deep a value is followed through cells that pass it on.
const FOLLOW: usize = 8;

/// The rank of a contribution, `arg/5`'s last column.
fn rank_of(f: &Fact) -> (u8, &str) {
    match f.args.get(4).and_then(Value::as_str) {
        Some("override") => (2, "override"),
        Some("default") => (0, "default"),
        _ => (1, "normal"),
    }
}

/// The site of aggregate fact `id`'s winning contribution (holding the
/// focused part), the value followed to where it was written.
fn winner_site(
    s: &mut Surface,
    c: &mut Compress,
    id: NodeId,
    focus: Option<&Focus>,
    single: bool,
    depth: usize,
) -> Option<Site> {
    let circuit = s.p.circuit;
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
        return value_site(s, c, id, focus, depth);
    }
    let mut contributions: Vec<(NodeId, &Fact)> = children
        .iter()
        .filter_map(|ch| match circuit.view(*ch) {
            View::Fact { fact, .. } if fact.pred == "arg" && !is_check(fact) => Some((*ch, fact)),
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
        site_of(s, c, id, depth + 1)
            .filter(|w| !w.at.is_empty() && !w.at.starts_with('<'))
            .map(|w| (rank_of(f).1.to_string(), w.at))
    });
    let mut site = value_site(s, c, win, focus, depth)?;
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
    s: &mut Surface,
    c: &mut Compress,
    id: NodeId,
    focus: Option<&Focus>,
    depth: usize,
) -> Option<Site> {
    let circuit = s.p.circuit;
    let own = site_of(s, c, id, depth);
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
        let plain =
            !rhs.is_empty() && !rhs.contains(|c: char| c.is_whitespace() || "()[]{}$,".contains(c));
        plain.then(|| crate::ir::path_keys(rhs))
    });
    if depth < FOLLOW
        && let Some(v) = value
        && let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a))
        && let View::Times { children, .. } = circuit.view(alt)
        && let Some((p, keys)) = passed_cell(c, circuit, children, v, read.as_deref())
    {
        let focus = (!keys.is_empty()).then_some(Focus { keys, value: None });
        if let Some(site) = winner_site(s, c, p, focus.as_ref(), false, depth + 1) {
            return Some(site);
        }
    }
    own
}

/// One step of a value's provenance chain (R-122), `= EXPR   SITE`: the
/// expression that wrote the value as the source writes it, where, the
/// clause's bindings and the rank it won at; or a contribution it beat
/// (`lost`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub expr: String,
    /// `file:line`, or the flag that gave the value (`--set env=prod`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lost: bool,
}

impl Printer<'_> {
    /// The chain of the winning value of attribute fact `attr` (an
    /// `attr/4`), at `keys` below it when they name part of an object:
    /// one step per expression it passed through, following only what
    /// each reads, until a literal, a key or a provider's value; then one
    /// step per contribution it beat. Empty when no statement wrote it.
    /// A stack key in `stack_keys` ends it: the deployment line has its
    /// value.
    pub fn attr_chain(
        &self,
        rules: &[RuleStmt],
        attr: &Atom,
        keys: &[String],
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let Some(id) = self.circuit.fact_id(&engine::circuit_fact(attr)) else {
            return Vec::new();
        };
        let focus = (!keys.is_empty()).then(|| Focus {
            keys: keys.to_vec(),
            value: None,
        });
        self.chain(rules, id, focus.as_ref(), stack_keys)
    }

    /// The chain of fact node `id`'s value ([`Printer::attr_chain`]).
    pub fn chain(
        &self,
        rules: &[RuleStmt],
        id: NodeId,
        focus: Option<&Focus>,
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        let (mut out, mut lost) = (Vec::new(), Vec::new());
        let mut w = Chain {
            out: &mut out,
            lost: &mut lost,
            keys: stack_keys,
        };
        winner_chain(&mut s, &mut c, id, focus, 0, &mut w);
        // A binding is a step's when its expression reads it and no next
        // step names its value (`= zone_index[z]  with z = "a"`); a key's
        // value is the deployment line's.
        let last = out.len().saturating_sub(1);
        for (i, step) in out.iter_mut().enumerate() {
            let reads: BTreeSet<&str> = step
                .expr
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .collect();
            step.with.retain(|w| {
                let name = w.split(" = ").next().unwrap_or_default();
                i == last
                    && reads.contains(name)
                    && step.expr.trim() != name
                    && !stack_keys.contains(name)
            });
        }
        out.extend(lost);
        out
    }
}

/// A chain being walked: its steps, the writes they beat, and the stack
/// keys that end it.
struct Chain<'a> {
    out: &'a mut Vec<Step>,
    lost: &'a mut Vec<Step>,
    keys: &'a BTreeSet<String>,
}

/// [`winner_site`], each step kept: the winning contribution's chain,
/// and each one it beat into `lost`.
fn winner_chain(
    s: &mut Surface,
    c: &mut Compress,
    id: NodeId,
    focus: Option<&Focus>,
    depth: usize,
    w: &mut Chain,
) {
    let circuit = s.p.circuit;
    let View::Fact { alts, .. } = circuit.view(id) else {
        return;
    };
    let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a)) else {
        return;
    };
    let View::Times { children, .. } = circuit.view(alt) else {
        return;
    };
    let aggregate = children.iter().any(
        |ch| matches!(circuit.view(*ch), View::Leaf(Leaf::Rule { id }) if id.starts_with('Σ')),
    );
    if !aggregate {
        return value_chain(s, c, id, focus, None, depth, w);
    }
    let part = |v: &Value| -> Option<Value> {
        let mut at = v;
        for k in focus.map(|f| f.keys.as_slice()).unwrap_or_default() {
            let Value::Obj(m) = at else { return None };
            at = m.get(k)?;
        }
        Some(at.clone())
    };
    let mut contributions: Vec<(NodeId, &Fact)> = children
        .iter()
        .filter_map(|ch| match circuit.view(*ch) {
            View::Fact { fact, .. } if fact.pred == "arg" && !is_check(fact) => Some((*ch, fact)),
            _ => None,
        })
        .filter(|(_, f)| f.args.get(3).and_then(part).is_some())
        .collect();
    contributions.sort_by_key(|(_, f)| std::cmp::Reverse(rank_of(f).0));
    let Some(&(win, fact)) = contributions.first() else {
        return;
    };
    let (top, rank) = rank_of(fact);
    let won = fact.args.get(3).and_then(part);
    // What it beat: a lower rank's value, where it differs. An object's
    // contributions at the winning rank merge; none of them lost.
    for &(l, f) in &contributions[1..] {
        let v = f.args.get(3).and_then(part);
        if rank_of(f).0 == top
            || v == won
            || matches!(v, Some(Value::Obj(_)))
            || placeholder(f, v.as_ref())
        {
            continue;
        }
        if let (Some(site), Some(v)) = (site_of(s, c, l, depth + 1), v) {
            let r = rank_of(f).1;
            w.lost.push(Step {
                expr: match f.args.get(3) {
                    Some(whole) => super::surface_in(
                        s.p.redact,
                        whole,
                        focus.map(|f| f.keys.as_slice()).unwrap_or_default(),
                        &v,
                    ),
                    None => s.p.redact.surface(&v),
                },
                at: place(&site),
                with: Vec::new(),
                rank: (r != "normal").then(|| r.to_string()),
                lost: true,
            });
        }
    }
    let rank = (rank != "normal").then_some(rank);
    value_chain(s, c, win, focus, rank, depth, w);
}

/// Whether `v`, contribution `f`'s value (or the part of it asked
/// about), is the engine's own open null for an attribute of the resource
/// it contributes to: a placeholder the program's write replaces, never a
/// write it beat (R-124).
fn placeholder(f: &Fact, v: Option<&Value>) -> bool {
    let own = |label: &str| {
        crate::value::null_owner(label).is_some_and(|(t, a)| {
            f.args.first().and_then(Value::as_str) == Some(t.as_str())
                && f.args.get(1).and_then(Value::as_str) == Some(a.as_str())
        })
    };
    fn nulls<'v>(v: &'v Value, out: &mut Vec<&'v str>) -> bool {
        match v {
            Value::Null { label, .. } => {
                out.push(label);
                true
            }
            Value::Obj(m) => !m.is_empty() && m.values().all(|x| nulls(x, out)),
            _ => false,
        }
    }
    let mut labels = Vec::new();
    v.is_some_and(|v| nulls(v, &mut labels)) && labels.iter().all(|l| own(l))
}

/// A site's place: `file:line`, or the flag that gave the value.
fn place(site: &Site) -> String {
    match site.at.is_empty() {
        true => site.statement.clone(),
        false => site.at.clone(),
    }
}

/// [`value_site`], each step kept: the expression that wrote fact `id`,
/// then the chain of the input or `let` it reads.
fn value_chain(
    s: &mut Surface,
    c: &mut Compress,
    id: NodeId,
    focus: Option<&Focus>,
    rank: Option<&str>,
    depth: usize,
    w: &mut Chain,
) {
    let circuit = s.p.circuit;
    let Some(own) = site_of(s, c, id, depth) else {
        return;
    };
    let View::Fact { fact, alts, .. } = circuit.view(id) else {
        return;
    };
    let mut value = match fact.pred.as_str() {
        "arg" | "attr" => fact.args.get(3),
        _ => None,
    };
    let mut rhs = own
        .entry
        .as_deref()
        .map(|e| e.split_once(" = ").map_or(e, |(_, r)| r).to_string());
    if let (Some(f), Some(e)) = (focus.filter(|f| !f.keys.is_empty()), rhs.as_deref()) {
        rhs = field_of(e, &f.keys).or_else(|| shorthand(e, &f.keys));
    }
    if let Some(f) = focus.filter(|f| !f.keys.is_empty()) {
        value = f
            .keys
            .iter()
            .try_fold(value, |v, k| match v {
                Some(Value::Obj(m)) => Some(m.get(k)),
                _ => None,
            })
            .flatten();
    }
    // The value is a secret's, or a part of one: its expression says no
    // literal (R-124 amendment 2).
    let secret = match (value, fact.pred.as_str(), fact.args.get(3)) {
        (Some(v), "arg" | "attr", Some(whole)) => {
            s.p.redact.is_secret(v)
                || super::surface_in(
                    s.p.redact,
                    whole,
                    focus.map(|f| f.keys.as_slice()).unwrap_or_default(),
                    v,
                ) == "(sensitive)"
        }
        (Some(v), ..) => s.p.redact.is_secret(v),
        _ => false,
    };
    let expr = match (&rhs, value) {
        (Some(r), _) if secret => super::masked(r),
        (Some(r), _) => r.clone(),
        // A plain leaf of a secret object is `(sensitive)` too.
        (None, Some(v)) => match (fact.pred.as_str(), fact.args.get(3)) {
            ("arg" | "attr", Some(whole)) => super::surface_in(
                s.p.redact,
                whole,
                focus.map(|f| f.keys.as_slice()).unwrap_or_default(),
                v,
            ),
            _ => s.p.redact.surface(v),
        },
        (None, None) => own.statement.clone(),
    };
    w.out.push(Step {
        expr,
        at: place(&own),
        with: own.with.clone(),
        rank: rank.map(str::to_string),
        lost: false,
    });
    if depth >= FOLLOW {
        return;
    }
    let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a)) else {
        return;
    };
    let View::Times { children, .. } = circuit.view(alt) else {
        return;
    };
    // What the entry reads, when it is a path (`config.region`), and the
    // cell that passes the value on.
    let read: Option<Vec<String>> = rhs.as_deref().and_then(|rhs| {
        let plain = !rhs.is_empty()
            && !rhs.contains(|c: char| c.is_whitespace() || "()[]{}$,\"".contains(c));
        plain.then(|| crate::ir::path_keys(rhs))
    });
    let next = match value {
        Some(v) => passed_cell(c, circuit, children, v, read.as_deref()),
        None => None,
    };
    // An expression of one cell (`str.lower(cidrs.main)`): that cell's value.
    let next = next.or_else(|| read_cell(c, circuit, children, rhs.as_deref()?));
    // A stack key ends it: the deployment line says its value.
    let key = |p: NodeId| match circuit.view(p) {
        View::Fact { fact, .. } => {
            fact.args.first().and_then(Value::as_str) == Some("input")
                && fact.args.get(1).and_then(Value::as_str) == Some("")
                && fact
                    .args
                    .get(2)
                    .and_then(Value::as_str)
                    .is_some_and(|n| w.keys.contains(n))
        }
        _ => false,
    };
    if let Some((p, keys)) = next.filter(|(p, _)| !key(*p)) {
        let focus = (!keys.is_empty()).then_some(Focus { keys, value: None });
        winner_chain(s, c, p, focus.as_ref(), depth + 1, w);
    }
}

/// The one input or `let` cell among a firing's `children` that
/// expression `e` reads, with the keys below it; `None` when it reads
/// none, or several.
fn read_cell(
    c: &mut Compress,
    circuit: &Circuit,
    children: &[NodeId],
    e: &str,
) -> Option<(NodeId, Vec<String>)> {
    let paths: Vec<Vec<String>> = code_of(e)
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.'))
        .filter(|t| t.starts_with(|ch: char| ch.is_alphabetic() || ch == '_'))
        .map(crate::ir::path_keys)
        .collect();
    let mut cells: Vec<NodeId> = children.to_vec();
    for ch in children {
        let View::Fact { fact, alts, .. } = circuit.view(*ch) else {
            continue;
        };
        if matches!(fact.pred.as_str(), "attr" | "arg" | "want") {
            continue;
        }
        if let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a))
            && let View::Times { children, .. } = circuit.view(alt)
        {
            cells.extend(children.iter().copied());
        }
    }
    let mut found: Vec<(NodeId, Vec<String>)> = Vec::new();
    for id in cells {
        for p in &paths {
            if let Some(rest) = cell_reads(circuit, id, p)
                && !found.iter().any(|(x, _)| *x == id)
            {
                found.push((id, rest));
            }
        }
    }
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Expression `e` without its string literals' text, their
/// interpolations kept: what it reads (`name` of `"${name}-gke"`).
fn code_of(e: &str) -> String {
    let mut out = String::new();
    let (mut quoted, mut depth, mut escaped) = (false, 0usize, false);
    let mut chars = e.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted && depth == 0 {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    quoted = false;
                    out.push(' ');
                }
                '$' if chars.peek() == Some(&'{') => {
                    chars.next();
                    depth = 1;
                    out.push(' ');
                }
                _ => {}
            }
            continue;
        }
        match ch {
            '"' if depth == 0 => quoted = true,
            '{' if depth > 0 => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(' ');
                    continue;
                }
            }
            _ => {}
        }
        out.push(ch);
    }
    out
}

/// When fact `id` is an input or `let` cell and `read` names it (or a
/// path into it), the keys of `read` below the cell.
fn cell_reads(circuit: &Circuit, id: NodeId, read: &[String]) -> Option<Vec<String>> {
    let View::Fact { fact: f, .. } = circuit.view(id) else {
        return None;
    };
    if f.pred != "attr"
        || !matches!(
            f.args.first().and_then(Value::as_str),
            Some("input" | "let")
        )
    {
        return None;
    }
    let name = crate::ir::path_keys(f.args.get(2)?.as_str()?);
    let scoped: Vec<String> = match f.args.get(1)?.as_str()? {
        "" => Vec::new(),
        scope => scope
            .split('.')
            .map(str::to_string)
            .chain(name.clone())
            .collect(),
    };
    read.strip_prefix(name.as_slice())
        .or_else(|| {
            read.strip_prefix(scoped.as_slice())
                .filter(|_| !scoped.is_empty())
        })
        .map(<[String]>::to_vec)
}

/// The variable a shorthand field of object literal `e` reads at `key`,
/// `env` of `{ env, component: "network" }`.
fn shorthand(e: &str, keys: &[String]) -> Option<String> {
    let [k] = keys else { return None };
    let parse = crate::syntax::parser::parse_term(e.trim());
    let object = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::OBJECT)?;
    object
        .children()
        .filter(|n| n.kind() == SyntaxKind::OBJECT_FIELD)
        .map(|f| f.text().to_string().trim().to_string())
        .find(|t| t == k)
}

/// The expression object literal `e` gives at `keys` (`config.domain` of
/// `{ d: config.domain }` at `d`); `e` itself with no keys.
fn field_of(e: &str, keys: &[String]) -> Option<String> {
    let Some((k, rest)) = keys.split_first() else {
        return Some(e.to_string());
    };
    let parse = crate::syntax::parser::parse_term(e.trim());
    let object = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::OBJECT)?;
    let value = object
        .children()
        .filter(|n| n.kind() == SyntaxKind::OBJECT_FIELD)
        .find_map(|f| {
            let text = f.text().to_string();
            let (key, value) = text.split_once(':')?;
            let key = key.trim();
            let key = key
                .strip_prefix('"')
                .and_then(|k| k.strip_suffix('"'))
                .unwrap_or(key);
            (key == k).then(|| value.trim().to_string())
        })?;
    field_of(&value, rest)
}

/// The input or `let` cell among a firing's `children` that passes on
/// `v`: read directly (`attr("input", .., v)`), or through the relation
/// the compiler reads a cell by (`kubernetes::nodepool_max(3)`); an
/// object cell when the entry reads `v` at a path of it (`read`:
/// `gcp.project_id`), with the keys below the cell.
fn passed_cell(
    c: &mut Compress,
    circuit: &Circuit,
    children: &[NodeId],
    v: &Value,
    read: Option<&[String]>,
) -> Option<(NodeId, Vec<String>)> {
    let cell = |id: NodeId| -> Option<Vec<String>> {
        let View::Fact { fact: f, .. } = circuit.view(id) else {
            return None;
        };
        if f.pred != "attr"
            || !matches!(
                f.args.first().and_then(Value::as_str),
                Some("input" | "let")
            )
        {
            return None;
        }
        // Only the cell the entry reads: another one the firing reads may
        // hold the same value by chance (`public = false` beside a
        // `multi_az` that is false).
        let at = f.args.get(3)?;
        // `gcp.project_id` of the cell `gcp`: the keys after its name;
        // a used module's item by its scope too, `config.zone` (R-111).
        let name = crate::ir::path_keys(f.args.get(2)?.as_str()?);
        let scoped: Vec<String> = match f.args.get(1)?.as_str()? {
            "" => Vec::new(),
            scope => scope
                .split('.')
                .map(str::to_string)
                .chain(name.clone())
                .collect(),
        };
        let read = read?;
        let rest = read.strip_prefix(name.as_slice()).or_else(|| {
            read.strip_prefix(scoped.as_slice())
                .filter(|_| !scoped.is_empty())
        })?;
        if rest.is_empty() {
            return (at == v).then(Vec::new);
        }
        let mut x = at;
        for k in rest {
            let Value::Obj(m) = x else { return None };
            x = m.get(k)?;
        }
        (x == v).then(|| rest.to_vec())
    };
    if let Some(found) = children.iter().find_map(|ch| cell(*ch).map(|k| (*ch, k))) {
        return Some(found);
    }
    children.iter().find_map(|ch| {
        let View::Fact { fact, alts, .. } = circuit.view(*ch) else {
            return None;
        };
        if matches!(fact.pred.as_str(), "attr" | "arg" | "want") {
            return None;
        }
        let alt = *alts.iter().min_by_key(|a| c.size(circuit, **a))?;
        let View::Times { children, .. } = circuit.view(alt) else {
            return None;
        };
        children.iter().find_map(|x| cell(*x).map(|k| (*x, k)))
    })
}

/// Where fact node `id` is derived: its stated place, or the statement of
/// its shortest firing; through a firing of a rule the compiler wrote, the
/// first fact it read that has one.
fn site_of(s: &mut Surface, c: &mut Compress, id: NodeId, depth: usize) -> Option<Site> {
    if let Some(site) = s.sites.get(&(id, depth)) {
        return site.clone();
    }
    let site = site_found(s, c, id, depth);
    s.sites.insert((id, depth), site.clone());
    site
}

/// [`site_of`], found.
fn site_found(s: &mut Surface, c: &mut Compress, id: NodeId, depth: usize) -> Option<Site> {
    let circuit = s.p.circuit;
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
                    statement: s.fact_text(fact),
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
                if let Some((file, first, last, entry)) = s.stated_in(span) {
                    site.stmt = Some((file, first));
                    site.last = last;
                    site.entry = entry;
                }
                Some(site)
            }
            Leaf::Input { source } => Some(Site {
                statement: s.p.redact.text(source),
                ..Site::default()
            }),
            _ => None,
        };
    }
    if let Some(at) = table_row(circuit, alts) {
        return Some(Site {
            at,
            statement: s.fact_text(fact),
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
            statement: s.p.redact.text(source),
            ..Site::default()
        });
    }
    if let Some(r) = rule
        && !r.starts_with('Σ')
        && let Some(site) = s.site(r, bindings)
    {
        return Some(site);
    }
    if depth >= FOLLOW {
        return None;
    }
    children.iter().find_map(|ch| match circuit.view(*ch) {
        View::Fact { .. } => site_of(s, c, *ch, depth + 1),
        _ => None,
    })
}

impl Surface<'_, '_> {
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
    fn site(&mut self, id: &str, bindings: &[(String, Value)]) -> Option<Site> {
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

/// The cell an `attr` or `arg` of an input, a `let` or an output names,
/// as a [`Because`]'s text spells it (`input kubernetes.nodepool_max`).
pub fn cell_name(t: &str, a: &str, p: &str) -> String {
    cell(t, a, p)
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

/// The cell an `attr` or `arg` names: `T k3s.server.p` (R-111), or an
/// input, `let` or output by its name.
fn cell(t: &str, a: &str, p: &str) -> String {
    match t {
        "input" | "let" | "output" if a.is_empty() => format!("{t} {p}"),
        "input" | "let" | "output" => format!("{t} {a}.{p}"),
        _ => super::attribute(
            &Address {
                typ: t.to_string(),
                name: a.to_string(),
            },
            p,
        ),
    }
}

/// A contribution's rank as the program writes it: nothing for normal.
fn rank_text(rank: &Value) -> String {
    match rank.as_str() {
        Some("normal") | None => String::new(),
        Some(r) => format!(" @{r}"),
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
    /// Each `[_]` of a chain (R-162): how `with` names it and its core
    /// variable (`resolve::each_var`).
    each: Vec<(String, String)>,
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
        // A chain prints as written; only its index terms are noted, and
        // a `[_]` by its path: `k8s.deployment[_]`, then `containers[_]`.
        if c.kind() == CHAIN {
            self.text.push_str(&c.text().to_string());
            let mut segs: Vec<String> = Vec::new();
            for el in c.children_with_tokens() {
                match el {
                    NodeOrToken::Token(t) if t.kind() != DOT && !t.kind().is_trivia() => {
                        segs.push(t.text().to_string());
                    }
                    NodeOrToken::Node(ix) if ix.kind() == INDEX => {
                        let mut ts = ix.children();
                        let each = ts.next().and_then(|t| bare_name(&t)).as_deref() == Some("_")
                            && ts.next().is_none();
                        if !each {
                            if let Some(last) = segs.last_mut() {
                                last.push_str(&ix.text().to_string());
                            }
                            continue;
                        }
                        let name = match self.each.is_empty() {
                            true => segs.join("."),
                            false => segs.last().cloned().unwrap_or_default(),
                        };
                        let var = crate::syntax::resolve::each_var(ix.text_range().start().into());
                        self.each.push((format!("{name}[_]"), var));
                        segs.clear();
                    }
                    _ => {}
                }
            }
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

/// A refinement an aggregate's value is checked against
/// (`type_refine(T, P, C)`, `attr_refine(T, A, P, C)`): not a
/// contribution.
fn is_check(f: &Fact) -> bool {
    matches!(
        (f.pred.as_str(), f.args.len()),
        (crate::refine::TYPE_REFINE, 3) | (crate::refine::ATTR_REFINE, 4)
    )
}

/// A string literal holds an interpolation `${..}` (`$${` is a literal
/// `${`).
fn has_hole(text: &str) -> bool {
    pieces(text).is_some_and(|ps| ps.iter().any(|p| matches!(p, Piece::Hole(..))))
}

/// One firing's bindings and its lowered rule: what a term of the
/// statement evaluates against.
struct Cx<'a> {
    /// The firing's bindings, borrowed: a firing over a manifest binds
    /// its documents.
    env: std::collections::HashMap<&'a str, &'a Value>,
    rule: Option<&'a RuleStmt>,
}

impl Cx<'_> {
    /// The address of the resource the source variable `name` ranges
    /// over (`r in T`), `T["A"]`; `None` for anything else.
    fn address_of(&self, name: &str) -> Option<String> {
        let var = capitalise(name);
        let v = *self.env.get(var.as_str())?;
        let typ = self.want_type(&var)?;
        match (typ, v) {
            (Value::Str(t), Value::Str(n)) => Some(
                Address {
                    typ: t,
                    name: n.clone(),
                }
                .to_string(),
            ),
            _ => None,
        }
    }

    /// The type `want(T, var)` in the rule's body gives `var`.
    fn want_type(&self, var: &str) -> Option<Value> {
        self.rule.and_then(|r| {
            r.body.iter().find_map(|l| match l {
                Lit::Pos(a)
                    if a.pred == "want"
                        && matches!(a.args.get(1), Some(Term::Var(x)) if x == var) =>
                {
                    self.core(&a.args[0])
                }
                _ => None,
            })
        })
    }

    /// A variable's value; one that ranges over a type's resources
    /// (`r in T`) as the resource's address.
    fn show_var(&self, var: &str, v: &Value, redact: &Redactor) -> String {
        let typ = self.want_type(var);
        match (typ, v) {
            (Some(Value::Str(t)), Value::Str(name)) if !redact.is_secret(v) => {
                super::address(&Address {
                    typ: t,
                    name: name.clone(),
                })
            }
            _ => self.document_row(v).unwrap_or_else(|| redact.surface(v)),
        }
    }

    /// A value a loader's document the rule reads holds whole: its row
    /// and size (R-131), `crds.yml:412  (24.0 KB)`.
    fn document_row(&self, v: &Value) -> Option<String> {
        if !structured(v) {
            return None;
        }
        self.found()
            .filter(|a| crate::tables::is_document(&a.pred))
            .find_map(|a| {
                let [.., at, doc] = a.args.as_slice() else {
                    return None;
                };
                let (Some(at), Some(doc)) = (self.bound(at), self.bound(doc)) else {
                    return None;
                };
                let Value::Str(at) = &*at else {
                    return None;
                };
                let at = place_in(at, &doc, v)?;
                let size = serde_json::to_vec(&engine::value_to_json(v)).map_or(0, |b| b.len());
                Some(format!("{at}  ({})", crate::query::size(size)))
            })
    }

    /// [`Cx::core`], a variable's or a literal's value borrowed.
    fn bound<'t>(&'t self, t: &'t Term) -> Option<std::borrow::Cow<'t, Value>> {
        use std::borrow::Cow;
        match t {
            Term::Val(v) => Some(Cow::Borrowed(v)),
            Term::Var(x) => self.env.get(x.as_str()).map(|v| Cow::Borrowed(*v)),
            t => self.core(t).map(Cow::Owned),
        }
    }

    /// A lowered term's value under the bindings.
    fn core(&self, t: &Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => self.env.get(x.as_str()).map(|v| (*v).clone()),
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
                    // A decimal, `0.5`, is a float (R-75).
                    QUANTITY if t.text().bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
                        crate::value::Float::parse(t.text()).ok().map(Value::Float)
                    }
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
                        None => (*self.env.get(capitalise(key.text()).as_str())?).clone(),
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
        for p in pieces(text)? {
            match p {
                Piece::Text(l) => {
                    out.push_str(&crate::syntax::resolve::unescape(&format!("\"{l}\"")).ok()?)
                }
                Piece::Hole(h, _) => {
                    let parse = crate::syntax::parser::parse_term(h);
                    let t = parse.syntax().children().next()?;
                    // A variable `r in T` binds interpolates as its
                    // address, as an untyped reference does (R-42).
                    if let Some(at) = self.address_of(h.trim()) {
                        out.push_str(&at);
                        continue;
                    }
                    match self.eval(&t)? {
                        Value::Str(s) => out.push_str(&s),
                        v if crate::stuck::has_null(&v) => return None,
                        // A reference interpolates as its address (R-42).
                        Value::Ref { typ, name, attr } => {
                            out.push_str(&crate::ir::Address { typ, name }.attr(&attr))
                        }
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
                return self
                    .env
                    .get(capitalise(head).as_str())
                    .map(|v| (*v).clone());
            }
            // A variable bound to an object: its field.
            if let Some(mut v) = self
                .env
                .get(capitalise(head).as_str())
                .copied()
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
            Some(Value::Str(s)) => s == name || s.ends_with(&crate::ir::scoped("", name)),
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
    crate::functions::body(name)?(args)
}
