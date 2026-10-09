//! `plan --why=none` (R-79): the bare diff, laid out as the plan was
//! before it was grouped by tick, so a script that reads it keeps
//! working: `definite:`, each `pending on` block, the pending groups, the
//! undetermined policies, the diagnostics and the apply order, with no
//! provenance. Only its words follow the tool's (R-88): changes, not
//! deformations; up to date, not undeformed.

use super::Report;
use super::deformation::{Deformation, Line, Op};
use super::mask::Shown;
use super::render::moved_text;
use super::style::{Paint, Style};
use super::tally::{by_kind, changes_text, count};
use crate::ir::Address;
use crate::provider::ActionKind;
use std::collections::BTreeSet;

use crate::stuck::nulls_text;

impl Report {
    /// The held changes and the pending groups: what `pending` counts.
    fn pending_count(&self) -> usize {
        self.pending
            .iter()
            .map(|b| b.deformations.len())
            .sum::<usize>()
            + self.groups.len()
    }

    /// `plan: 3 changes (2 create, 1 update), 5 pending, 2 undetermined`:
    /// the definite changes counted, the rest pending.
    fn bare_summary(&self) -> String {
        let kinds = by_kind(self.definite.iter());
        let mut out = changes_text(kinds.iter().map(|(_, n)| n).sum(), &kinds);
        if self.show_noop {
            out.push_str(&format!(", {} no-op", self.noops));
        }
        let pending = self.pending_count();
        if pending > 0 {
            out.push_str(&format!(", {pending} pending"));
        }
        let undetermined = self.policies.len();
        if undetermined > 0 {
            out.push_str(&format!(", {undetermined} undetermined"));
        }
        if !self.conflicts.is_empty() {
            out.push_str(&format!(", {}", count(self.conflicts.len(), "conflict")));
        }
        out
    }

    /// The bare diff, painted with `style`.
    pub fn render_bare(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let header = |s: &str| format!("{}\n", bold(s));
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            self.write_bare(&mut out, &self.kept, style);
            out.push_str(&format!("stack {} is up to date\n", self.stack));
            return out;
        }
        out.push_str(&self.bare_summary());
        out.push('\n');
        if !self.definite.is_empty() {
            out.push_str(&header("definite:"));
            self.write_bare(&mut out, &self.definite, style);
        }
        // What is left as it is but for a value given at creation (R-198).
        if !self.kept.is_empty() {
            out.push_str(&header("kept:"));
            self.write_bare(&mut out, &self.kept, style);
        }
        for b in &self.pending {
            let after = b
                .resolves_after
                .map(|t| format!(" (resolves after tick {t})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{} {}{}\n",
                bold("pending on"),
                nulls_text(&b.on, style),
                bold(&format!("{after}:"))
            ));
            self.write_bare(&mut out, &b.deformations, style);
        }
        if !self.groups.is_empty() {
            out.push_str(&header("pending groups:"));
            for g in &self.groups {
                let after = g
                    .resolves_after
                    .map(|t| format!(", resolves after tick {t}"))
                    .unwrap_or_default();
                let line = format!(
                    "? {} x unknown, on {}{after}  ({})",
                    g.pattern,
                    nulls_text(&g.on, Style::PLAIN),
                    g.reason
                );
                out.push_str(&style.paint(Paint::Warn, &line));
                out.push('\n');
            }
        }
        if !self.policies.is_empty() {
            out.push_str(&header("undetermined:"));
            for p in &self.policies {
                let when = match (p.may_derive, p.after) {
                    (false, Some(t)) => format!(", decided after tick {t}"),
                    (true, Some(t)) => format!(", may derive after tick {t}"),
                    (true, None) => ", may derive after a boundary".into(),
                    (false, None) => String::new(),
                };
                if p.refinement {
                    out.push_str(&format!(
                        "? refinement on {} deferred: {}{when}\n",
                        nulls_text(&p.on, style),
                        p.message
                    ));
                    continue;
                }
                out.push_str(&format!(
                    "? deny \"{}\" on {}{when}  ({})\n",
                    p.message,
                    nulls_text(&p.on, style),
                    p.reason
                ));
            }
        }
        if !self.warnings.is_empty() {
            let head = "warning: this plan empties what the last apply derived";
            out.push_str(&style.paint(Paint::Warn, head));
            out.push('\n');
            for line in self.warning_lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        // A confusable host label is a warning at every level (R-134).
        let confusable = self.confusable_lines();
        if !confusable.is_empty() {
            out.push_str(&style.paint(Paint::Warn, "warning: a host a reader may mistake"));
            out.push('\n');
            for line in confusable {
                out.push_str(&format!("  {line}\n"));
            }
        }
        for (title, ds) in [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)] {
            if ds.is_empty() {
                continue;
            }
            let conflict = title == "conflicts";
            let error = |s: &str| match conflict {
                true => style.paint(Paint::Error, s),
                false => s.to_string(),
            };
            out.push_str(&header(&error(&format!("{title}:"))));
            for d in ds {
                let rank = d
                    .rank
                    .as_ref()
                    .map(|r| format!(" at rank {r}"))
                    .unwrap_or_default();
                out.push_str(&error(&format!(
                    "! {}{rank}: {}",
                    d.addr.attr(&d.path),
                    d.reason
                )));
                out.push('\n');
                for w in &d.witnesses {
                    let (r, v, from) = (&w.rank, &w.value, &w.from);
                    let from = if from.is_empty() {
                        String::new()
                    } else {
                        let names: Vec<String> = from.iter().map(|f| bold(f)).collect();
                        format!("  from {}", names.join("; "))
                    };
                    // An object or a list as the plan lays it out, on
                    // one line.
                    let v = match &w.laid {
                        Some(laid) => laid.line(),
                        None => style.shown(v),
                    };
                    out.push_str(&format!("    {r} {v}{from}\n"));
                }
            }
        }
        if !self.denies.is_empty() {
            out.push_str(&header(&style.paint(Paint::Error, "denied:")));
            for d in &self.denies {
                out.push_str(&style.paint(Paint::Error, &format!("! {d}")));
                out.push('\n');
            }
        }
        if !self.ticks.is_empty() || !self.unscheduled.is_empty() {
            // One address per line, so each pastes into `why` or a program.
            out.push_str(&header("apply order:"));
            let unscheduled = (!self.unscheduled.is_empty())
                .then(|| ("unscheduled".to_string(), &self.unscheduled));
            let ticks = self.ticks.iter().map(|(t, xs)| (format!("tick {t}"), xs));
            for (head, xs) in ticks.chain(unscheduled) {
                out.push_str(&format!("  {head}\n"));
                for x in xs {
                    out.push_str(&format!("    {x}\n"));
                }
            }
        }
        if self.undeformed {
            out.push_str(&format!("stack {} is up to date\n", self.stack));
        }
        out
    }

    /// Changes in order, a copy's under it (R-67), as
    /// [`Report::write_level`] lays them out.
    fn write_bare(&self, out: &mut String, ds: &[Deformation], style: Style) {
        let all: Vec<&Deformation> = ds.iter().collect();
        self.bare_level(out, &all, None, "", style);
    }

    fn bare_level(
        &self,
        out: &mut String,
        ds: &[&Deformation],
        outer: Option<&Address>,
        indent: &str,
        style: Style,
    ) {
        let under = |d: &Deformation| -> Option<Address> {
            let chain = self.instances.enclosing(&d.addr);
            let at = match outer {
                None => chain.len(),
                Some(o) => chain.iter().position(|a| a == o)?,
            };
            at.checked_sub(1).map(|i| chain[i].clone())
        };
        let mut done: BTreeSet<Address> = BTreeSet::new();
        for d in ds {
            let Some(copy) = under(d) else {
                write_change(out, d, indent, style);
                continue;
            };
            if !done.insert(copy.clone()) {
                continue;
            }
            let members: Vec<&Deformation> = ds
                .iter()
                .filter(|m| self.instances.enclosing(&m.addr).contains(&copy))
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
            let addr = copy.to_string();
            out.push_str(&format!(
                "{indent}{} {}\n",
                style.marker(&kind),
                style.paint(Paint::Bold, &addr)
            ));
            self.bare_level(out, &members, Some(&copy), &format!("{indent}  "), style);
        }
    }
}

fn write_change(out: &mut String, d: &Deformation, indent: &str, style: Style) {
    let note = match d.kind {
        ActionKind::Drift => {
            "  (drift: a fresh null where the world has a value; its identity is stale)"
        }
        ActionKind::DeleteDeposed => "  (deposed)",
        ActionKind::Replace { .. } => "  (replace)",
        ActionKind::Forget => crate::report::FORGOTTEN,
        _ => "",
    };
    let addr = d.addr.to_string();
    out.push_str(&format!(
        "{indent}{} {}{note}\n",
        style.marker(&d.kind),
        style.paint(Paint::Bold, &addr)
    ));
    // Keep plan output readable.
    let max = 40usize;
    let inner = format!("{indent}  ");
    for (i, l) in d.lines.iter().enumerate() {
        if i == max {
            out.push_str(&format!(
                "{inner}... ({} more changes)\n",
                d.lines.len() - max
            ));
            break;
        }
        write_line(out, &d.kind, l, &inner, style);
    }
    for k in &d.kept {
        let note = k.note.as_deref().map(|n| format!("  {}", style.note(n)));
        out.push_str(&format!(
            "{inner}{} differs {}{}\n",
            k.line.path,
            style.note(crate::report::KEPT),
            note.unwrap_or_default()
        ));
    }
}

fn write_line(out: &mut String, kind: &ActionKind, l: &Line, indent: &str, style: Style) {
    let shown = |v: &Shown| style.shown(v);
    match l.op {
        Op::Add | Op::Remove => {
            let (sign, v) = if l.op == Op::Add {
                (style.paint(Paint::Create, "+"), &l.after)
            } else {
                (style.paint(Paint::Delete, "-"), &l.before)
            };
            if l.leaves.is_empty() {
                out.push_str(&format!("{indent}{sign} {} = {}\n", l.path, shown(v)));
                return;
            }
            out.push_str(&format!("{indent}{sign} {}\n", l.path));
            let inner = if l.op == Op::Add {
                ActionKind::Create
            } else {
                ActionKind::Delete
            };
            for x in &l.leaves {
                write_line(out, &inner, x, &format!("{indent}    "), style);
            }
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => {
                out.push_str(&format!("{indent}{} = {}\n", l.path, shown(&l.after)))
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                out.push_str(&format!("{indent}{} = {}\n", l.path, shown(&l.before)))
            }
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => out.push_str(&format!(
                "{indent}{} = {} -> {}\n",
                l.path,
                shown(&l.before),
                shown(&l.after)
            )),
            ActionKind::Noop | ActionKind::Forget => {}
        },
    }
}
