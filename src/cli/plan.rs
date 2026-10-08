//! `dform plan`: what apply would do, and what it waits on.

use super::evaluated::{Context, Evaluated};
use super::outputs::{grants_text, readers_of};
use super::planning::{Reporter, needs_text, unreachable_text};
use super::{Outcome, Refused, open_s3};
use crate::deployment::{self, Planned};
use crate::{report, store, zset};
use anyhow::Result;
use std::path::PathBuf;

/// `dform plan TARGET`.
#[derive(Debug, Clone)]
pub(super) struct Plan {
    /// `--out`: the plan file written.
    pub(super) out: Option<PathBuf>,
    pub(super) json: bool,
    /// `plan --destroy`: the plan against an empty wanted set.
    pub(super) destroy: bool,
    pub(super) why: report::Why,
    /// `--new-master` (R-163).
    pub(super) new_master: bool,
}

impl Plan {
    /// The plan printed, as text or JSON, with the plan file when one is
    /// written or the plan needs an approval; refused when the program
    /// denies it.
    pub(super) fn run(&self, run: Evaluated) -> Result<Outcome> {
        let Evaluated { cx, ev, .. } = run;
        let deployment::Evaluation {
            located,
            st,
            moves,
            violations,
            redact,
            mut policy,
            evaluator,
            ..
        } = ev;
        let planned = policy
            .take()
            .ok_or_else(|| anyhow::anyhow!("internal: a plan without its policy pass"))??;
        // Of a destroy, only the denies over its plan refuse it.
        let violations = match self.destroy {
            true => Vec::new(),
            false => violations,
        };
        let r = Reporter::new(&cx, &evaluator, &located, &st);
        let mut report = r.report(
            &planned.plan,
            &planned.res,
            &planned.sections,
            1,
            &moves,
            &planned.denies,
        );
        report.every_site = self.json;
        r.explain(&mut report, &planned.plan, &planned.res, 1);
        let file = self.file(&cx, &located, &r, &planned, &st)?;
        if self.json {
            self.print_json(&cx, &located, &report, &planned, &violations, file.as_ref())?;
        } else {
            self.print_text(&cx, &r, &report, &planned, file.as_ref(), &evaluator)?;
        }
        // The report listed the conflicts and the denies over the plan
        // (R-111): stderr names only what it did not, once. An up to date
        // plan lists none (a secret's age, R-161, denies one).
        let denies = &planned.denies;
        let mut unshown: Vec<String> = violations
            .iter()
            .filter(|v| !report::is_conflict(v))
            .cloned()
            .collect();
        if report.undeformed && !self.json {
            unshown.extend(denies.iter().cloned());
        }
        cx.cli.cmd.blocked(&unshown, &redact)?;
        if !violations.is_empty() || !denies.is_empty() {
            let conflicts = violations.iter().filter(|v| report::is_conflict(v)).count();
            return Err(Refused::new(
                "blocked by constraints",
                conflicts,
                violations.len() - conflicts + denies.len(),
            )
            .into());
        }
        if let (Some(out), Some(file)) = (&self.out, &file) {
            file.save(out)?;
            eprintln!(
                "plan file: {} (plan digest: {})",
                out.display(),
                file.digest.as_deref().unwrap_or_default()
            );
            cx.audit.append(
                "plan",
                serde_json::json!({
                    "digest": file.digest,
                    "documents": crate::diff::documents(&evaluator.tables.sources()),
                    "file": out.display().to_string(),
                    "inputs": file.inputs,
                    "needs_approval": file.needs_approval,
                    "who": crate::audit::who(),
                }),
            )?;
        }
        Ok(Outcome::Done)
    }

    /// The plan file, when one is written or the plan needs an approval:
    /// its digest is what an approver signs. A plan that writes no file
    /// reads the master here, the key made now if need be.
    fn file(
        &self,
        cx: &Context,
        located: &deployment::Located,
        r: &Reporter,
        planned: &Planned,
        st: &crate::state::State,
    ) -> Result<Option<zset::file::PlanFile>> {
        let needs = crate::approval::needs(&planned.res.facts);
        if self.out.is_none() && needs.is_empty() {
            return Ok(None);
        }
        let loaded;
        let key = match (cx.writes, &cx.key) {
            (true, k) => k.as_ref(),
            (false, _) => {
                // The key may be made now: a bucket is checked first, as
                // for any run that writes.
                if let (None, store::Location::S3(spec)) = (&cx.cli.world, &located.location) {
                    open_s3(&cx.root, true)(spec)?;
                }
                loaded = cx
                    .dep
                    .master(
                        &cx.mixing,
                        crate::custody::Want {
                            make: true,
                            new_master: self.new_master,
                        },
                    )?
                    .key;
                loaded.as_ref()
            }
        };
        let inputs = match &cx.inputs {
            Some(i) => i.clone(),
            None => cx.plan_inputs(key)?,
        };
        let mut f = r.plan_file(
            &planned.plan,
            &planned.res,
            &planned.sections,
            &planned.resources,
            st,
            key,
            inputs,
        )?;
        f.digest = Some(f.digest());
        Ok(Some(f))
    }

    /// `plan --json`: one document, its outcome as the exit status says it
    /// (R-147).
    fn print_json(
        &self,
        cx: &Context,
        located: &deployment::Located,
        report: &report::Report,
        planned: &Planned,
        violations: &[String],
        file: Option<&zset::file::PlanFile>,
    ) -> Result<()> {
        let mut j = report.json();
        j["deployment"] = serde_json::json!(cx.deployment);
        j["outcome"] = match violations.is_empty() && planned.denies.is_empty() {
            true => Outcome::Done.word(),
            false => "refused",
        }
        .into();
        j["key_defaults"] = serde_json::json!(located.instance.defaulted);
        if !planned.unreachable.is_empty() {
            j["unreachable"] = planned
                .unreachable
                .iter()
                .map(|(a, why)| serde_json::json!({ "address": a.to_string(), "reason": why }))
                .collect();
        }
        if let Some(f) = file {
            j["needs_approval"] = serde_json::to_value(&f.needs_approval)?;
            j["digest"] = serde_json::to_value(&f.digest)?;
        }
        // The names declared more than once (R-104), as the plan file
        // lists them.
        let guarded = zset::file::guarded(&planned.res);
        if !guarded.is_empty() {
            j["guarded"] = serde_json::to_value(&guarded)?;
        }
        println!("{}", serde_json::to_string_pretty(&j)?);
        Ok(())
    }

    /// The plan as text, held for the project's plan (R-114): the report,
    /// what no Delete can reach, the grants of secret outputs (R-166), and
    /// the digest to approve.
    fn print_text(
        &self,
        cx: &Context,
        r: &Reporter,
        report: &report::Report,
        planned: &Planned,
        file: Option<&zset::file::PlanFile>,
        evaluator: &deployment::Evaluator,
    ) -> Result<()> {
        let held = &cx.cli.held;
        held.summary(report.summary());
        held.print(&r.rendered(report));
        held.print(&unreachable_text(&planned.unreachable));
        // Who each secret output no provider holds is sealed to: the grant
        // (R-166).
        let unheld = crate::stack::unheld_secret_outputs(
            &planned.res.facts,
            &crate::stack::secret_output_types(&evaluator.program),
        );
        if !unheld.is_empty() && cx.cli.world.is_none() {
            let readers = readers_of(&cx.root, &cx.deployment, &open_s3(&cx.root, false))?;
            held.print(&grants_text(&unheld, &readers));
        }
        // The digest to approve, when a change is held for an approval
        // (the bare diff lists those changes after it); a plan file's
        // digest is on stderr beside its path.
        if let Some(f) = file.filter(|f| !f.needs_approval.is_empty()) {
            if r.why() == report::Why::None {
                held.print(&needs_text(&f.needs_approval));
            }
            held.print(&format!(
                "plan digest: {}\n",
                f.digest.as_deref().unwrap_or_default()
            ));
        }
        Ok(())
    }
}
