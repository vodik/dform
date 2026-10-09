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
use crate::query::Redactor;
use crate::spell;
use crate::syntax::SyntaxNode;
use crate::syntax::resolve::capitalise;
use crate::value::Value;
use anyhow::Result;
use compress::Compress;
use docrow::{place_in, structured};
use rowan::NodeOrToken;
use sites::{base_parts, base_place, cell, rank_text, table_row};
use statement::{Cx, Shown, collapse, is_check, statement_at};
use std::collections::{BTreeMap, BTreeSet};

mod chains;
mod compress;
mod docrow;
mod sites;
mod statement;
pub use chains::Step;
pub use docrow::DocRow;
pub use sites::{Site, cell_name};

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
            ("want", [Value::Str(t), Value::Str(a)]) => crate::report::address(&Address {
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
                .map(|f| Some(format!("{f}={}", spell::bare(m.get(f)?))))
                .collect::<Option<Vec<_>>>()?
                .join(","),
            (Some(ks), k) if ks.len() == 1 => format!("{}={}", ks[0], spell::bare(k)),
            _ => spell::bare(k),
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
            Some(f) if spell::atom(&f.atom()) == pattern => self.fact_text(&f),
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

// --- where a fact is derived, on one line (`plan`'s right column, R-79) ----
