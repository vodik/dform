//! `plan --why=none` (R-79): the bare diff, laid out as the plan was
//! before it was grouped by tick, so a script that reads it keeps
//! working: `definite:`, each `pending on` block, the pending groups, the
//! undetermined policies, the diagnostics and the apply order, with no
//! provenance. Only its words follow the tool's (R-88): changes, not
//! deformations; up to date, not undeformed.

use super::{
    ActionKind, Deformation, Line, Op, Paint, Redactor, Report, Shown, Style, kind_name, moved_text,
};
use crate::ir::Address;
use std::collections::BTreeSet;

fn nulls_text(on: &[String], style: Style) -> String {
    on.iter()
        .map(|n| style.paint(Paint::Null, &format!("?{}", crate::ir::label(n))))
        .collect::<Vec<_>>()
        .join(" ")
}

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
        let kinds: Vec<(&str, usize)> = ["create", "update", "replace", "drift", "delete", "adopt"]
            .into_iter()
            .map(|k| {
                let n = self
                    .definite
                    .iter()
                    .filter(|d| kind_name(&d.kind) == k)
                    .count();
                (k, n)
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        let n: usize = kinds.iter().map(|(_, n)| n).sum();
        let mut out = format!("plan: {n} change{}", if n == 1 { "" } else { "s" });
        if !kinds.is_empty() {
            let ks: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
            out.push_str(&format!(" ({})", ks.join(", ")));
        }
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
            let n = self.conflicts.len();
            out.push_str(&format!(", {n} conflict{}", if n == 1 { "" } else { "s" }));
        }
        out
    }

    /// The bare diff, painted with `style`.
    pub fn render_bare(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let header = |s: &str| format!("{}\n", bold(s));
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            out.push_str(&format!("stack {} is up to date\n", self.stack));
            return out;
        }
        out.push_str(&self.bare_summary());
        out.push('\n');
        if !self.definite.is_empty() {
            out.push_str(&header("definite:"));
            self.write_bare(&mut out, &self.definite, style);
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
                for (r, v, from) in &d.witnesses {
                    let from = if from.is_empty() {
                        String::new()
                    } else {
                        let names: Vec<String> = from.iter().map(|f| bold(f)).collect();
                        format!("  from {}", names.join("; "))
                    };
                    out.push_str(&format!("    {r} {}{from}\n", style.shown(v)));
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
            let addr = Redactor::default().cell(&crate::zset::reference(&copy));
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
        _ => "",
    };
    let addr = Redactor::default().cell(&crate::zset::reference(&d.addr));
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
                out.push_str(&format!("{indent}{} was {}\n", l.path, shown(&l.before)))
            }
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => out.push_str(&format!(
                "{indent}{}: {} -> {}\n",
                l.path,
                shown(&l.before),
                shown(&l.after)
            )),
            ActionKind::Noop => {}
        },
    }
}
