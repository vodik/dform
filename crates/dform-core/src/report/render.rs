//! The plan as text (`Report::render`): its headline, each tick's section with
//! its changes in the tree of modules and copies they are in, what `later` holds,
//! the pending groups, the policy block, the conflicts and warnings.

use super::deformation::{Deformation, Line};
use super::errors::diag_lines;
use super::groups::{Group, group_address, group_copy};
use super::labels::{address, address_text, attribute, marker_of, relation_name};
use super::layout::{Row, layout};
use super::lines::both;
use super::mask::{Shown, confusables_in};
use super::policy;
use super::style::{Paint, Style};
use super::tally::{by_kind, count};
use super::tree::Site;
use super::waits::{owners, provisional_text, waited};
use super::{Report, Why};
use crate::ir::Address;
use crate::provider::ActionKind;
use std::collections::{BTreeMap, BTreeSet};

impl Report {
    /// The changes the plan counts: every definite one that is not a
    /// no-op, and every held one a later tick of this plan makes (one
    /// waiting on what this plan does not resolve is `later`'s).
    fn counted(&self) -> impl Iterator<Item = &Deformation> {
        self.definite
            .iter()
            .filter(|d| !matches!(d.kind, ActionKind::Noop))
            .chain(
                self.pending
                    .iter()
                    .filter(|b| b.resolves_after.is_some())
                    .flat_map(|b| b.deformations.iter())
                    // An object state holds, as state has it until its
                    // provider reads it at the boundary (R-177).
                    .filter(|d| !matches!(d.kind, ActionKind::Noop)),
            )
    }

    /// The changes by kind, in summary order.
    pub(super) fn kinds(&self) -> Vec<(&'static str, usize)> {
        by_kind(self.counted())
    }

    /// How many changes the plan has, in every tick (the summary's count).
    pub fn changes(&self) -> usize {
        self.counted().count()
    }

    /// The ticks, each with its changes, what it waits on and the
    /// objects it deletes once their replacements stand; the first is
    /// this report's tick. Held changes waiting on what this plan does
    /// not schedule are not in any.
    pub(super) fn sections(&self) -> BTreeMap<usize, Section<'_>> {
        let mut out: BTreeMap<usize, Section> = BTreeMap::new();
        let shown: Vec<&Deformation> = self.definite.iter().collect();
        if !shown.is_empty() {
            out.entry(self.tick).or_default().changes = shown;
        }
        for b in &self.pending {
            let Some(t) = b.resolves_after else { continue };
            let s = out.entry(t + 1).or_default();
            s.changes.extend(b.deformations.iter());
            s.waits.extend(b.on.iter().cloned());
            for k in b.provisional.iter().flatten() {
                if !s.provisional.contains(k) {
                    s.provisional.push(k.clone());
                }
            }
        }
        for d in &self.definite {
            if matches!(d.kind, ActionKind::Replace { create_first: true }) {
                out.entry(self.tick + 1).or_default().deposed.push(&d.addr);
            }
        }
        for g in &self.groups {
            let Some(t) = g.resolves_after else { continue };
            let s = out.entry(t + 1).or_default();
            s.groups.push(g);
            s.waits.extend(g.on.iter().cloned());
        }
        out
    }

    /// What `later` holds: rules that may derive an unknown number of
    /// resources, and held changes whose nulls this plan does not resolve
    /// (an undetermined deny or check is the policy block's).
    fn has_later(&self) -> bool {
        self.groups.iter().any(|g| g.resolves_after.is_none())
            || self.pending.iter().any(|b| b.resolves_after.is_none())
    }

    /// The last line, only when there is something to decide: apply
    /// refuses this plan, `apply: refused  2 conflicts, 1 deny`.
    pub fn apply_line(&self) -> Option<String> {
        let mut why = Vec::new();
        if !self.conflicts.is_empty() {
            why.push(count(self.conflicts.len(), "conflict"));
        }
        if !self.denies.is_empty() {
            why.push(match self.denies.len() {
                1 => "1 deny".to_string(),
                n => format!("{n} denies"),
            });
        }
        let verb = match self.removing {
            true => "destroy",
            false => "apply",
        };
        (!why.is_empty()).then(|| format!("{verb}: refused  {}", why.join(", ")))
    }

    /// The host labels the plan's values hold that a reader may mistake for
    /// others (R-134): each with its attribute and where it was written;
    /// a warning at every level, `-q` included.
    pub fn confusable_lines(&self) -> Vec<String> {
        fn walk(l: &Line, addr: &Address, prefix: &str, out: &mut Vec<String>) {
            let path = match prefix.is_empty() {
                true => l.path.clone(),
                false => format!("{prefix}.{}", l.path),
            };
            let at = l
                .site
                .as_ref()
                .filter(|s| !s.at.is_empty())
                .map(|s| format!("  {}", s.at))
                .unwrap_or_default();
            let attr = attribute(addr, &path);
            for v in [&l.after, &l.before] {
                if let Shown::Value(j) = v {
                    confusables_in(j, &attr, &at, out);
                }
            }
            for x in &l.leaves {
                walk(x, addr, &path, out);
            }
        }
        let mut out = Vec::new();
        let all = self
            .definite
            .iter()
            .chain(self.pending.iter().flat_map(|b| b.deformations.iter()));
        for d in all {
            for l in &d.lines {
                walk(l, &d.addr, "", &mut out);
            }
        }
        out
    }

    /// The `warning` section's lines (R-80), unindented: each rule the
    /// plan deletes every resource of, with what it deletes and the leaf
    /// that changed since the last apply; each relation it empties.
    pub(super) fn warning_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for w in &self.warnings {
            match &w.statement {
                Some(statement) => {
                    out.push(format!("{}  {statement}", w.name));
                    let shown: Vec<String> =
                        w.deleted.iter().take(3).map(|a| address_text(a)).collect();
                    let more = match w.deleted.len().saturating_sub(3) {
                        0 => String::new(),
                        n => format!(" and {n} more"),
                    };
                    let n = w.deleted.len();
                    out.push(format!(
                        "    deletes all {n} it derived at the last apply: {}{more}",
                        shown.join(", ")
                    ));
                    if let Some(b) = &w.because {
                        out.push(format!("    because {b}"));
                    }
                }
                None => {
                    let rows = match w.rows {
                        1 => "1 row".to_string(),
                        n => format!("{n} rows"),
                    };
                    out.push(format!(
                        "{}  had {rows} at the last apply, has none now",
                        relation_name(&w.name)
                    ));
                }
            }
        }
        out
    }

    /// The report as text, uncoloured: what `plan` prints with no colour,
    /// every golden, and the controller's log line.
    pub fn text(&self) -> String {
        self.render(Style::PLAIN)
    }

    /// The report as text, painted with `style`: the summary, each tick
    /// with its changes, `later`, the diagnostics, and what apply does.
    pub fn render(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            let mut rows = Vec::new();
            self.write_kept(&mut rows, style);
            out.push_str(&layout(&rows, style));
            if !self.nested {
                out.push_str(&format!("stack {} is up to date\n", self.stack));
            }
            return out;
        }
        let mut rows: Vec<Row> = match self.nested {
            true => Vec::new(),
            false => vec![Row::plain(self.summary())],
        };
        for (t, s) in self.sections() {
            rows.push(Row::plain(String::new()));
            // A held object state has, unread until the boundary
            // (R-177), is listed as state has it, not counted.
            let n = s
                .changes
                .iter()
                .filter(|d| self.show_noop || !matches!(d.kind, ActionKind::Noop))
                .count();
            // A rule the tick before decides may add changes: how many
            // is not known yet (R-156).
            let head = match (self.resumed && t == self.tick, s.groups.is_empty(), n) {
                (true, _, _) => format!("tick {t}  {n} remaining, resumed"),
                (false, true, _) => format!("tick {t}  {}", count(n, "change")),
                (false, false, 0) => format!("tick {t}  ? changes"),
                (false, false, _) => format!("tick {t}  {n}+ changes"),
            };
            rows.push(Row::new(&head, bold(&head)));
            for (i, w) in waited(&s.waits).into_iter().enumerate() {
                let lead = if i == 0 {
                    "  waits on  "
                } else {
                    "            "
                };
                rows.push(Row::plain(format!("{lead}{w}")));
            }
            if !s.provisional.is_empty() {
                rows.push(Row::plain(format!(
                    "  {}",
                    provisional_text(&s.provisional)
                )));
            }
            self.write_level(&mut rows, &s.changes, "  ", style);
            for a in &s.deposed {
                let addr = address(a);
                let plain = format!("  - {addr}  (deposed)");
                let painted = format!(
                    "  {} {}  (deposed)",
                    style.paint(Paint::Delete, "-"),
                    style.address(&ActionKind::Delete, &addr)
                );
                rows.push(Row::new(&plain, painted));
            }
            self.write_groups(&mut rows, &s.groups, style);
        }
        if !self.kept.is_empty() {
            rows.push(Row::plain(String::new()));
            self.write_kept(&mut rows, style);
        }
        // The policy block (R-200): after the ticks, before what waits.
        // Its columns are its own: laid out apart, it moves no column of
        // the ticks'.
        let policy = policy::rows(&self.policy, self.why, style);
        if !policy.is_empty() {
            rows.push(Row::plain(String::new()));
            for line in layout(&policy, style).lines() {
                rows.push(Row::plain(line.to_string()));
            }
        }
        if self.has_later() {
            rows.push(Row::plain(String::new()));
            let head = "later";
            rows.push(Row::new(head, bold(head)));
            self.write_later(&mut rows, style);
        }
        // What the program states and derives nothing of (R-120): each
        // statement as a group row is, its place and why on the right.
        if !self.not_planned.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "not planned";
            rows.push(Row::new(head, style.paint(Paint::Warn, head)));
            for n in &self.not_planned {
                let addr = address(&n.addr);
                let plain = format!("  {addr}");
                let painted = format!("  {}", style.paint(Paint::Warn, &addr));
                let at = n.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                let right = match at.is_empty() {
                    true => vec![n.reason.clone()],
                    false => vec![format!("{at}  {}", n.reason), n.reason.clone()],
                };
                rows.push(Row::new(&plain, painted).with(right));
            }
        }
        let confusable = self.confusable_lines();
        if !self.warnings.is_empty() || !confusable.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "warning";
            rows.push(Row::new(head, style.paint(Paint::Warn, head)));
            for line in self.warning_lines().into_iter().chain(confusable) {
                rows.push(Row::plain(format!("  {line}")));
            }
        }
        if !self.denies.is_empty() {
            rows.push(Row::plain(String::new()));
            rows.push(Row::new("denied", style.paint(Paint::Error, "denied")));
            let wide = self
                .denied
                .iter()
                .map(|d| address_text(&d.addr).chars().count())
                .max()
                .unwrap_or(0);
            for (i, d) in self.denies.iter().enumerate() {
                let row = self.denied.get(i);
                let message = row.map(|r| r.message.as_str()).unwrap_or(d);
                let left = format!("  {message}");
                let right = row
                    .map(|r| {
                        let at = r.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                        let addr = address_text(&r.addr);
                        let pad = " ".repeat(wide - addr.chars().count());
                        format!("{addr}{pad}    {at}").trim().to_string()
                    })
                    .into_iter()
                    .collect();
                rows.push(Row::new(&left, style.paint(Paint::Error, &left)).with(right));
            }
        }
        if !self.approvals.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "held for approval";
            rows.push(Row::new(head, style.paint(Paint::Held, head)));
            let wide = self
                .approvals
                .iter()
                .map(|a| a.reason.chars().count())
                .max()
                .unwrap_or(0);
            for a in &self.approvals {
                let at = a.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                let pad = " ".repeat(wide - a.reason.chars().count());
                let right = format!("{}{pad}    {at}", a.reason).trim_end().to_string();
                rows.push(Row::plain(format!("  {}", address_text(&a.addr))).with(vec![right]));
            }
        }
        out.push_str(&layout(&rows, style));
        let header = |s: &str| format!("{}\n", bold(s));
        let diags = [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)];
        for (title, ds) in diags {
            if ds.is_empty() {
                continue;
            }
            out.push('\n');
            let conflict = title == "conflicts";
            let error = |s: &str| match conflict {
                true => style.paint(Paint::Error, s),
                false => s.to_string(),
            };
            out.push_str(&header(&error(title)));
            for d in ds {
                out.push_str(&diag_lines(d, self.why, style, conflict));
            }
        }
        if self.undeformed {
            if !self.nested {
                out.push_str(&format!("\nstack {} is up to date\n", self.stack));
            }
            return out;
        }
        if let Some(line) = self.apply_line() {
            out.push('\n');
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// The objects left as they are but for a value given at their
    /// creation only (R-198), each `=` with what it keeps.
    pub(super) fn write_kept(&self, rows: &mut Vec<Row>, style: Style) {
        for d in &self.kept {
            self.write_change(rows, d, "", style);
        }
    }

    /// `later`'s rows: each group no tick of this plan decides, each held
    /// change this plan does not schedule (R-156).
    fn write_later(&self, rows: &mut Vec<Row>, style: Style) {
        let unscheduled: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| g.resolves_after.is_none())
            .collect();
        self.write_groups(rows, &unscheduled, style);
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds: Vec<&Deformation> = b.deformations.iter().collect();
            let on = waited(&b.on.iter().cloned().collect()).join(", ");
            // A header like a tick's (R-111).
            rows.push(Row::plain(format!("  waits on  {on}")));
            if let Some(keys) = &b.provisional {
                rows.push(Row::plain(format!("  {}", provisional_text(keys))));
            }
            self.write_level(rows, &ds, "  ", style);
        }
    }

    /// Group rows (R-67, R-156): each by the address its rule names (a
    /// copy that may derive once, its resources under it), with what it
    /// reads or waits on; `later`'s, or a tick's whose boundary decides it.
    fn write_groups(&self, rows: &mut Vec<Row>, groups: &[&Group], style: Style) {
        let site = |s: &Option<Site>| s.as_ref().map(|s| s.at.clone()).unwrap_or_default();
        let full = self.why >= Why::How;
        let both = |at: String, cond: String, reason: &str| both(full, at, cond, reason);
        // A copy that may derive (R-67) is said once, its resources under it.
        let copy_of = group_copy;
        let mut copies: BTreeSet<String> = BTreeSet::new();
        for g in groups {
            let copy = copy_of(g);
            if let Some(c) = &copy {
                if !copies.insert(c.clone()) {
                    continue;
                }
                let reads = g.reads.clone().unwrap_or_default();
                let shown = address_text(c);
                let plain = format!("  {shown}");
                let painted = format!("  {}", style.paint(Paint::Bold, &shown));
                let reads = reads.strip_prefix("resource ").unwrap_or(&reads);
                rows.push(Row::new(&plain, painted).with(vec![format!("if {reads}")]));
                for m in groups.iter().filter(|m| copy_of(m).as_ref() == Some(c)) {
                    let pattern = address_text(&group_address(m));
                    let plain = format!("    {pattern}");
                    let painted = format!("    {}", style.paint(Paint::Warn, &pattern));
                    rows.push(Row::new(&plain, painted));
                }
                continue;
            }
            let full_pattern = group_address(g);
            let unknown = full_pattern.ends_with("[?]") || full_pattern.contains("${");
            let pattern = address_text(&full_pattern);
            let on = waited(&g.on.iter().cloned().collect()).join(", ");
            let cond = match (&g.reads, unknown) {
                (Some(r), true) => format!("one per {r}"),
                (Some(r), false) => format!("if {r}"),
                (None, _) => format!("waits on {on}"),
            };
            let mut right = both(site(&g.site), cond, &g.reason);
            // Too many values for the column: the resources they are of.
            if g.reads.is_none() {
                let owners = owners(&g.on.iter().cloned().collect()).join(", ");
                if owners != on {
                    right.extend(both(site(&g.site), format!("waits on {owners}"), &g.reason));
                }
            }
            let plain = format!("  {pattern}");
            let painted = format!("  {}", style.paint(Paint::Warn, &pattern));
            rows.push(Row::new(&plain, painted).with(right));
        }
    }

    /// The scopes `addr` is in, innermost first, as the plan's tree
    /// nests it: each copy, and each used module's instance of `shared`
    /// (`module k3s`).
    pub(super) fn enclosing(&self, addr: &Address, shared: &BTreeSet<String>) -> Vec<Address> {
        let mut out = Vec::new();
        let mut name = addr.name.as_str();
        while let Some((scope, _)) = crate::ir::scope_split(name) {
            match self.instances.address(scope) {
                Some(copy) => out.push(copy),
                None if shared.contains(scope) => out.push(Address {
                    typ: MODULE.to_string(),
                    name: scope.to_string(),
                }),
                None => {}
            }
            name = scope;
        }
        out
    }

    /// Changes in order, a copy's under it (R-67): `+ network blue` at
    /// the place of its first resource, the resources indented beneath, a
    /// copy inside it nested again.
    fn write_level(&self, rows: &mut Vec<Row>, ds: &[&Deformation], indent: &str, style: Style) {
        self.walk_level(ds, None, 0, &mut |depth, node| {
            let indent = format!("{indent}{}", "  ".repeat(depth));
            match node {
                Node::Header { addr, kind } => {
                    let addr = address(&addr);
                    let plain = format!("{indent}{} {addr}", marker_of(&kind));
                    let painted = format!(
                        "{indent}{} {}",
                        style.marker(&kind),
                        style.paint(Paint::Bold, &addr)
                    );
                    rows.push(Row::new(&plain, painted));
                }
                Node::Change(d) => self.write_change(rows, d, &indent, style),
            }
        });
    }

    /// The tree of this report's tick (R-200, the path tree): each change
    /// in order, each copy and each used module's instance two or more of
    /// them share a header at the place of its first, what is in it one
    /// level deeper. The plan prints it with sites ([`Report::render`]),
    /// the apply with each call's status (R-206, `progress::Block`).
    pub fn outline(&self) -> Vec<(usize, Node<'_>)> {
        let ds: Vec<&Deformation> = self.definite.iter().collect();
        let mut out = Vec::new();
        self.walk_level(&ds, None, 0, &mut |depth, node| out.push((depth, node)));
        out
    }

    /// The changes `ds` under `outer` at `depth`, each copy's header with
    /// its kind: its `deformation` row's (`zset::Instances::row_kind`),
    /// `-` when the program wants none of its resources, `+` when one is
    /// created, else `~`.
    fn walk_level<'r>(
        &'r self,
        ds: &[&'r Deformation],
        outer: Option<&Address>,
        depth: usize,
        f: &mut dyn FnMut(usize, Node<'r>),
    ) {
        // The scopes the changes here share: each copy, and a used
        // module's instance two or more of them are in (R-200: the plan
        // is the path tree printed).
        let shared = shared_modules(ds.iter().map(|d| &d.addr), &self.instances);
        let enclosing = |a: &Address| self.enclosing(a, &shared);
        // The copy or module directly under `outer` a change is in, if any.
        let under = |d: &Deformation| -> Option<Address> {
            let chain = enclosing(&d.addr);
            let at = match outer {
                None => chain.len(),
                Some(o) => chain.iter().position(|a| a == o)?,
            };
            at.checked_sub(1).map(|i| chain[i].clone())
        };
        let mut done: BTreeSet<Address> = BTreeSet::new();
        for d in ds {
            let Some(copy) = under(d) else {
                f(depth, Node::Change(d));
                continue;
            };
            if !done.insert(copy.clone()) {
                continue;
            }
            let members: Vec<&Deformation> = ds
                .iter()
                .filter(|m| enclosing(&m.addr).contains(&copy))
                .copied()
                .collect();
            let kinds: Vec<&str> = members
                .iter()
                .filter_map(|m| crate::zset::deformation_kind(&m.kind, false))
                .collect();
            let kind = match self.instances.row_kind(&copy, &kinds) {
                "delete" => ActionKind::Delete,
                "create" => ActionKind::Create,
                _ => ActionKind::Update,
            };
            f(
                depth,
                Node::Header {
                    addr: copy.clone(),
                    kind,
                },
            );
            self.walk_level(&members, Some(&copy), depth + 1, f);
        }
    }
}

/// The scopes of `addrs` that are no copy (a used module's instance) and
/// hold two or more of them: what the plan's tree groups under a header.
fn shared_modules<'a>(
    addrs: impl Iterator<Item = &'a Address>,
    instances: &crate::zset::Instances,
) -> BTreeSet<String> {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for a in addrs {
        let mut name = a.name.as_str();
        while let Some((scope, _)) = crate::ir::scope_split(name) {
            if instances.address(scope).is_none() {
                *n.entry(scope).or_default() += 1;
            }
            name = scope;
        }
    }
    n.into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(s, _)| s.to_string())
        .collect()
}

/// `moved OLD -> NEW`, one line per rename `moved/3` applied to state.
pub fn moved_text(moves: &[(Address, Address)]) -> String {
    moves
        .iter()
        .map(|(old, new)| format!("moved {old} -> {new}\n"))
        .collect()
}

/// The header of a used module's instance in the plan's tree, `module
/// k3s` (R-200), as `why` names the scope.
const MODULE: &str = "module";

/// One line of a tick's tree ([`Report::outline`]).
#[derive(Debug, Clone)]
pub enum Node<'r> {
    /// A copy, or a used module's instance (`module k3s`), the changes
    /// after it at a deeper level its own; `kind` gives its mark.
    Header {
        addr: Address,
        kind: ActionKind,
    },
    Change(&'r Deformation),
}

/// One tick of the report.
#[derive(Default)]
pub(super) struct Section<'a> {
    pub(super) changes: Vec<&'a Deformation>,
    pub(super) waits: BTreeSet<String>,
    pub(super) deposed: Vec<&'a Address>,
    /// The rules that may derive an unknown number of resources once
    /// the tick before has run (R-156).
    pub(super) groups: Vec<&'a Group>,
    /// The settings of the providers whose connection an earlier tick
    /// makes, which planned its changes against their offline schemas
    /// (R-193), as `later`'s provisional block says them.
    pub(super) provisional: Vec<String>,
}
