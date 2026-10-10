//! The derivation in the program's own terms (`Printer::source_tree`): a fired rule
//! as its statement at `file:line`, its bindings by the source's names, a derived
//! fact as its address, an aggregate as the contributions it merged.

use super::printer::{Focus, Printer, Walk, leaf_text, plan_text, world_text};
use super::sites::{Site, base_place, cell, rank_text};
use super::statement::{Cx, Shown, collapse, is_check, statement_at};
use crate::address::Address;
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::circuit::{Fact, Leaf, NodeId, View};
use crate::spell;
use crate::syntax::SyntaxNode;
use crate::syntax::resolve::capitalise;
use crate::value::Value;
use rowan::NodeOrToken;
use std::collections::{BTreeMap, BTreeSet};

impl Printer<'_> {
    pub(super) fn surface<'a>(&'a self, rules: &'a [RuleStmt]) -> Surface<'a, 'a> {
        Surface {
            p: self,
            rules,
            w: Walk::default(),
            files: BTreeMap::new(),
            list_keys: Default::default(),
            sites: Default::default(),
        }
    }

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

pub(super) struct Surface<'a, 'b> {
    pub(super) p: &'a Printer<'b>,
    pub(super) rules: &'a [RuleStmt],
    pub(super) w: Walk,
    /// Each source file a printed rule is in, parsed once.
    pub(super) files: BTreeMap<String, SyntaxNode>,
    /// The keyed lists' keys, `type_list_key(T, L, Keys)`, read once.
    list_keys: std::cell::OnceCell<BTreeMap<(String, String), Vec<String>>>,
    /// Each fact node's site at a depth ([`site_of`]), found once: the
    /// leaves of one large value (a manifest's document) share their
    /// statement, whose bindings print the whole value.
    pub(super) sites: std::collections::HashMap<(NodeId, usize), Option<Site>>,
}

impl Surface<'_, '_> {
    pub(super) fn push(&mut self, line: String) {
        self.w.out.push_str(&line);
        self.w.out.push('\n');
    }

    /// Print fact node `id`; `contribution`: as a contribution to the
    /// aggregate above it, its value and rank.
    pub(super) fn fact(
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
            self.cell_read(id)
                .or_else(|| self.written(id))
                .unwrap_or(id)
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

    /// The program's row fact node `id` is, where a type's lifecycle is
    /// seeded (`zset::WRITTEN`): `lifecycle(r, w)` is the program's
    /// statement, not the rule that makes `lifecycle` of it.
    pub(super) fn written(&self, id: NodeId) -> Option<NodeId> {
        let c = self.p.circuit;
        let View::Fact {
            fact, alts: [a], ..
        } = c.view(id)
        else {
            return None;
        };
        let View::Times { children, .. } = c.view(*a) else {
            return None;
        };
        (fact.pred == "lifecycle")
            .then(|| {
                children.iter().copied().find(
                    |ch| matches!(c.view(*ch), View::Fact { fact, .. } if fact.pred == crate::zset::WRITTEN),
                )
            })
            .flatten()
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
    pub(super) fn fact_text(&self, f: &Fact) -> String {
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
                // dform's own relation, as the program names what it holds.
                spell::internal(&a, false, &|t| self.term(t))
                    .or_else(|| crate::modules::private_text(&a, &|a| r.surface_atom(a), "   "))
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
            (Some(ks), Value::Obj(_)) => crate::report::fold::label(ks, k)?,
            // A list keyed by one field written by that field's value.
            (Some(ks), k) if ks.len() == 1 => {
                let one = Value::Obj([(ks[0].clone(), k.clone())].into());
                crate::report::fold::label(ks, &one)?
            }
            _ => spell::bare(k),
        };
        Some((format!("{list}[{label}]"), content))
    }

    /// A fact the firing found absent, as the program would say it: `not`
    /// the fact as [`Surface::fact_text`] spells it when it is ground, each
    /// value as the plan spells it (a reference by its address) otherwise;
    /// a row of dform's own as [`spell::internal`] says it.
    pub(super) fn absent(&self, a: &Atom) -> String {
        if let Some(text) = spell::internal(a, true, &|t| self.term(t)) {
            return text;
        }
        let ground: Option<Vec<Value>> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        let fact = match ground {
            Some(args) => self.fact_text(&Fact::new(&a.pred, args)),
            None => self.p.redact.surface_atom(a),
        };
        format!("not {fact}")
    }

    /// A column of a row, as the plan spells a value.
    fn term(&self, t: &Term) -> String {
        match t {
            Term::Val(v) => self.p.redact.surface(v),
            t => spell::term(t),
        }
    }

    /// Where a given fact came from, when firing `a` is a single source leaf.
    pub(super) fn given(&self, a: NodeId) -> Option<String> {
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

    pub(super) fn firing(&mut self, a: NodeId, pad: &str, focus: Option<&Focus>) {
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
                // A `not` says it found the row absent; a row of dform's
                // own is said in words.
                Leaf::Absent { atom } => match self.absent(atom) {
                    t if t.starts_with("not ") => format!("{t}   (absent)"),
                    t => t,
                },
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
    pub(super) fn source_line(&mut self, id: &str) -> Option<(String, String, Option<Shown>)> {
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

    pub(super) fn statement(&mut self, id: &str, bindings: &[(String, Value)], pad: &str) {
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
