//! What plan and apply print of a plan ([`Reporter`]): its report, the
//! plan file it makes or is checked against, and the texts beside it.

use super::evaluated::Context;
use super::run_inputs::{answer_inputs, answer_text, env_inputs};
use crate::provider::ActionKind;
use crate::{deployment, engine, ir, query, report, state, stuck, zset};
use anyhow::{Result, bail};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A plan as plan and apply print it, record it in a plan file, and check
/// it against one: what every report of the run shares.
pub(super) struct Reporter<'a> {
    cx: &'a Context,
    evaluator: &'a deployment::Evaluator,
    located: &'a deployment::Located,
    /// The copies state remembers as the run starts: a removed copy's
    /// deletes print under it (R-67).
    kept: BTreeMap<String, String>,
    /// An apply that resumes one interrupted: its first tick is what
    /// remained (R-122).
    pub(super) resuming: Cell<bool>,
    /// Said once a run: what a run without the master cannot compare.
    drift_said: Cell<bool>,
    /// What a site's place is relative to.
    pub(super) top: Option<PathBuf>,
    /// What the plan may empty (R-80): the apply's `--allow-empty` and the
    /// stack's `allow_empty`.
    allow_empty: Vec<String>,
}

impl<'a> Reporter<'a> {
    pub(super) fn new(
        cx: &'a Context,
        evaluator: &'a deployment::Evaluator,
        located: &'a deployment::Located,
        st: &state::State,
    ) -> Reporter<'a> {
        let mut allow_empty = match &cx.cli.cmd {
            super::Cmd::Apply(a) => a.allow_empty.clone(),
            _ => Vec::new(),
        };
        if let Some(t) = located
            .loaded
            .manifest
            .as_ref()
            .and_then(|m| m.stacks.get(&located.loaded.stack))
        {
            allow_empty.extend(t.allow_empty.iter().cloned());
        }
        Reporter {
            cx,
            evaluator,
            located,
            kept: st.instances.clone(),
            resuming: Cell::new(false),
            drift_said: Cell::new(false),
            top: site_root(&cx.cli.files),
            allow_empty,
        }
    }

    fn schema(&self) -> &'a crate::schema::Schema {
        self.evaluator.schema()
    }

    /// How much each printed change says of why (R-79).
    pub(super) fn why(&self) -> report::Why {
        self.cx.cli.cmd.why()
    }

    /// The report of `plan` at `tick`.
    pub(super) fn report(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        sections: &stuck::Sections,
        tick: usize,
        moved: &[(ir::Address, ir::Address)],
        denies: &[String],
    ) -> report::Report {
        let backend = &*self.evaluator.backend;
        let mut report = report::report(&report::Input {
            plan,
            res,
            sections,
            program: &self.evaluator.program,
            schema: self.schema(),
            stack: &self.located.loaded.stack,
            show_noop: self.cx.cli.show_noop,
            tick,
            moved,
            denies,
            kept: &self.kept,
        });
        report.resumed = self.resuming.get() && tick == 1;
        // A destroy's deletes say no reason; a plan's say why they are
        // gone (`Report::explain`, After R-149 amendments 4 and 5).
        report.removing = self.cx.cli.cmd.destroys();
        custody_marks(&mut report, plan, backend);
        if !self.drift_said.replace(true) {
            drift_unknown(&self.cx.deployment, plan, backend);
        }
        report
    }

    /// Each change's leaf that changed since the last apply: the last
    /// apply's program evaluated again (`diff`'s reading), at the commit it
    /// recorded or with the inputs it recorded. Only this executable can
    /// evaluate it (not a test linking dform in).
    fn because(
        &self,
        report: &mut report::Report,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
    ) {
        let cli = &self.cx.cli;
        let dform = std::env::current_exe()
            .ok()
            .is_some_and(|e| e.file_stem().is_some_and(|s| s == "dform"));
        if self.why() == report::Why::None || !dform || cli.files.is_empty() {
            return;
        }
        let addresses: Vec<ir::Address> = plan
            .actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop))
            .map(|a| a.addr.clone())
            .collect();
        if addresses.is_empty() {
            return;
        }
        let keys: Vec<String> = self
            .located
            .loaded
            .cfg
            .keys
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        let (Some(entries), Ok(rerun)) = (
            self.cx.entries.get(),
            cli.rerun(&self.located.instance.key, keys),
        ) else {
            return;
        };
        let then = crate::timing::time(
            || "evaluated the last apply's program, for why".into(),
            || {
                crate::diff::last_apply(
                    entries, &cli.files, &rerun, &cli.set, &cli.data, &addresses,
                )
            },
        );
        let Some(then) = then else {
            return;
        };
        let redact = query::Redactor::new(&res.facts, self.schema());
        report.because(&then, &crate::diff::snapshot(res, &redact, &addresses));
    }

    /// What the plan empties since the last apply (R-80), less what this
    /// apply's `--allow-empty` and the stack's `allow_empty` name.
    pub(super) fn emptied(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        because: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<zset::Emptied> {
        // A destroy empties everything; its question says so.
        let Some(then) = self
            .cx
            .last_derived
            .as_ref()
            .filter(|_| !self.cx.cli.cmd.destroys())
        else {
            return Vec::new();
        };
        let deleted: BTreeSet<String> = plan
            .actions
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::Delete))
            .map(|a| a.addr.to_string())
            .collect();
        let rows_now = |rel: &str| res.facts.iter().filter(|f| f.pred == rel).count();
        zset::emptied(then, &deleted, &rows_now, because)
            .into_iter()
            .filter(|e| !e.allowed(&self.allow_empty))
            .collect()
    }

    /// `report` explained at the run's level of why: at tick 1 also the
    /// leaves that changed since the last apply and what the plan empties.
    pub(super) fn explain(
        &self,
        report: &mut report::Report,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        tick: usize,
    ) {
        report.keys = self
            .located
            .instance
            .key
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        report.explain(
            self.why(),
            res,
            &query::Redactor::new(&res.facts, self.schema()),
        );
        if tick == 1 {
            self.because(report, plan, res);
            let leaf: BTreeMap<String, String> = report
                .definite
                .iter()
                .filter_map(|d| Some((d.addr.to_string(), d.because.clone()?)))
                .collect();
            report.warnings = self.emptied(plan, res, &|a| leaf.get(a).cloned());
        }
        if let Some(top) = &self.top {
            report.relative_to(top);
        }
    }

    /// The report as printed: `--why=none` is the bare diff, laid out as
    /// before R-79.
    pub(super) fn rendered(&self, report: &report::Report) -> String {
        match self.why() {
            report::Why::None => report.render_bare(self.cx.cli.style),
            _ => report.render(self.cx.cli.style),
        }
    }

    /// The plan of `tick` printed: the lines the default level folds away
    /// say no site.
    pub(super) fn show(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        sections: &stuck::Sections,
        tick: usize,
        moved: &[(ir::Address, ir::Address)],
        denies: &[String],
    ) {
        let mut report = self.report(plan, res, sections, tick, moved, denies);
        report.every_site = false;
        self.explain(&mut report, plan, res, tick);
        print!("{}", self.rendered(&report))
    }

    /// The delta of the plan at `tick`, as a plan file records it.
    pub(super) fn delta(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        sections: &stuck::Sections,
        tick: usize,
        key: Option<&zset::file::Key>,
    ) -> Vec<zset::file::Entry> {
        let report = self.report(plan, res, sections, tick, &[], &[]);
        let redact = query::Redactor::new(&res.facts, self.schema());
        zset::file::delta(plan, sections, &report, self.schema(), &redact, key)
    }

    /// `apply PLAN`: the delta re-evaluated at tick 1 must be the file's; a
    /// later tick's is compared at its boundary, which stops before a tick
    /// that differs (`zset::file::tick_differences`).
    pub(super) fn check_saved(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        sections: &stuck::Sections,
    ) -> Result<()> {
        let Some((path, saved)) = &self.cx.saved else {
            return Ok(());
        };
        let key = self.cx.file_key();
        let now = self.delta(plan, res, sections, 1, key);
        let mut diff = saved.stale(&now);
        // A secret answer the plan read (a location's), read again: by its
        // version when its source keeps versions (R-172), else by its
        // digest. One that is "not yet" now is waited on, not compared.
        let now: BTreeMap<String, serde_json::Value> = answer_inputs(&self.evaluator.externs, key)
            .into_iter()
            .filter_map(|a| Some((a.get("sensitive")?.as_str()?.to_string(), a)))
            .collect();
        for a in &saved.inputs.answers {
            let Some(label) = a.get("sensitive").and_then(|l| l.as_str()) else {
                continue;
            };
            let Some(n) = now.get(label) else { continue };
            // A file of given secrets by its path (R-108), not its
            // extern's label.
            let file = label
                .split_once('/')
                .filter(|(pred, _)| crate::tables::is_sealed(pred))
                .and_then(|(_, rest)| rest.rsplit_once('#'))
                .map(|(location, _)| location);
            match (a.get("version"), n.get("version")) {
                // A secret manager's (R-172), by its version.
                (Some(was), Some(is)) if was != is => diff.push(format!(
                    "{}: version {} in the plan, {} now: it moved in its secret manager since \
                     the plan",
                    answer_text(label),
                    was.as_str().unwrap_or_default(),
                    is.as_str().unwrap_or_default()
                )),
                _ if n["digest"] != a["digest"] => diff.push(match file {
                    Some(f) => format!("{f}: a given secret changed since the plan"),
                    None => format!("{}: changed since the plan", answer_text(label)),
                }),
                _ => {}
            }
        }
        if diff.is_empty() {
            return Ok(());
        }
        eprintln!(
            "plan file {} is stale: re-evaluation after refresh does not reproduce its delta:",
            path.display()
        );
        for d in &diff {
            eprintln!("- {d}");
        }
        bail!("stale plan: run plan again");
    }

    /// The plan file of a plan: its delta at tick 1, the inputs, the pinned
    /// commits, the extern answers and the `requires_approval` rows.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_file(
        &self,
        plan: &crate::provider::Plan,
        res: &engine::EvalResult,
        sections: &stuck::Sections,
        resources: &[ir::Resource],
        st: &state::State,
        key: Option<&zset::file::Key>,
        inputs: zset::file::Inputs,
    ) -> Result<zset::file::PlanFile> {
        let (externs, backend) = (&self.evaluator.externs, &*self.evaluator.backend);
        let report = self.report(plan, res, sections, 1, &[], &[]);
        let redact = query::Redactor::new(&res.facts, self.schema());
        let mut deformations =
            zset::file::delta(plan, sections, &report, self.schema(), &redact, key);
        for e in deformations
            .iter_mut()
            .filter(|e| e.action.starts_with("replace"))
        {
            let addr = ir::Address {
                typ: e.typ.clone(),
                name: e.name.clone(),
            };
            e.dependents = resources
                .iter()
                .filter(|r| r.deps.contains(&addr))
                .map(|r| r.addr.to_string())
                .collect();
        }
        let mut unresolved: BTreeSet<String> = deformations
            .iter()
            .flat_map(|e| e.on.iter().cloned())
            .collect();
        for a in &plan.actions {
            for c in &a.changes {
                if let Some((crate::provider::NULL_KEY, l)) =
                    c.after.as_ref().and_then(crate::provider::marker)
                {
                    unresolved.insert(l.to_string());
                }
            }
        }
        Ok(zset::file::PlanFile {
            version: zset::file::VERSION,
            stack: self.located.loaded.stack.clone(),
            inputs: zset::file::Inputs {
                env: env_inputs(externs.env_labels().into_iter(), key),
                answers: answer_inputs(externs, key),
                ..inputs
            },
            world_digest: zset::file::world_digest(&backend.world_facts(st)?),
            deformations,
            pending_groups: zset::file::groups(res, &redact, key),
            nulls: zset::file::Nulls {
                resolved: zset::file::resolved(&res.facts, &redact, key),
                unresolved: unresolved.into_iter().collect(),
            },
            ticks: report
                .ticks
                .iter()
                .map(|(t, xs)| zset::file::Tick {
                    tick: *t,
                    addresses: xs.clone(),
                })
                .collect(),
            externs: externs.recorded(),
            needs_approval: crate::approval::needs(&res.facts)
                .into_iter()
                .map(|(deformation, reason)| zset::file::NeedsApproval {
                    deformation,
                    reason,
                })
                .collect(),
            digest: None,
            unkeyed: key.is_none(),
            guarded: zset::file::guarded(res),
        })
    }
}

/// Each change of `report` a run that does not hold the master plans,
/// marked (R-164): `secret changed, needs the key` when it would send a
/// stand-in, `secrets unchanged` when every secret leaf it derives was
/// proven unchanged (`.., a write-only one needs the key` when an update
/// would send one of those whole: the world does not answer it).
fn custody_marks(
    report: &mut report::Report,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    for d in report.definite.iter_mut() {
        let Some(a) = plan.actions.iter().find(|a| a.addr == d.addr) else {
            continue;
        };
        let need = backend.needs_master(a, None);
        let proven = backend.proven(&a.addr);
        d.custody = match (need.is_empty(), proven.is_empty()) {
            // An update sends a write-only secret whole, unchanged or not.
            (false, _) if need.iter().all(|p| proven.contains(p)) => {
                Some("secrets unchanged, a write-only one needs the key".into())
            }
            (false, _) => Some("secret changed, needs the key".into()),
            // Unchanged in the program; the world's own value of one it
            // answers is compared only by a run with the key.
            (true, false) if !backend.answered(&a.addr, &proven).is_empty() => {
                Some("secrets unchanged, drift unknown without the key".into())
            }
            (true, false) => Some("secrets unchanged".into()),
            (true, true) => None,
        };
    }
}

/// What a run without the master cannot see (After R-164): a secret leaf
/// it proved unchanged in the program whose value the world answers (a
/// Secret's `stringData` key, not a write-only one) may have been changed
/// in the world by someone else; only a run with the key compares it. Said
/// on stderr, each leaf, so the plan never reads as "no drift".
fn drift_unknown(
    deployment: &str,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    let leaves: Vec<String> = plan
        .actions
        .iter()
        .flat_map(|a| {
            backend
                .answered(&a.addr, &backend.proven(&a.addr))
                .into_iter()
                .map(|p| crate::report::attribute(&a.addr, &p))
        })
        .collect();
    if leaves.is_empty() {
        return;
    }
    eprintln!(
        "{deployment}: drift unknown without the key: {} the world holds {} compared with it only \
         by a run with the master: {}",
        match leaves.len() {
            1 => "1 secret".to_string(),
            n => format!("{n} secrets"),
        },
        if leaves.len() == 1 { "is" } else { "are" },
        leaves.join(", ")
    );
}

/// The bare diff's `needs approval:` section: each change and why.
pub(super) fn needs_text(needs: &[zset::file::NeedsApproval]) -> String {
    if needs.is_empty() {
        return String::new();
    }
    let mut out = String::from("needs approval:\n");
    for n in needs {
        out.push_str(&format!("  {}  ({})\n", n.deformation, n.reason));
    }
    out
}

/// A destroy's objects no Delete can reach ([`Planned::unreachable`]),
/// as the plan says a change and its reason.
pub(super) fn unreachable_text(unreachable: &[(ir::Address, String)]) -> String {
    if unreachable.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nunreachable  stay in state\n");
    for (a, why) in unreachable {
        out.push_str(&format!("  {}\n      {why}\n", report::address(a)));
    }
    out
}

/// The project's root, else the program's directory: what a site's place
/// is relative to.
pub(super) fn site_root(files: &[PathBuf]) -> Option<PathBuf> {
    files.first().and_then(|f| {
        let f = std::path::absolute(f).ok()?;
        crate::project::manifest_root(&f).or_else(|| f.parent().map(Path::to_path_buf))
    })
}
