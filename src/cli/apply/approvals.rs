//! An apply's approvals (docs/reference.md, "Approvals").

use super::ticks::Tick;
use crate::ast::Atom;
use crate::cli::evaluated::Context;
use crate::said::{Said, Teller};
use crate::{deployment, executor};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

/// What an apply's approvals are verified against (docs/reference.md,
/// "Approvals"): a token verifies against the stack's trust root (loaded
/// once), for this plan's digest and this deployment, by an approver
/// `approver_allowed` admits when the program restricts them.
pub(super) struct Approvals<'a> {
    pub(super) cx: &'a Context,
    pub(super) located: &'a deployment::Located,
    pub(super) roots: std::cell::OnceCell<crate::approval::Roots>,
    /// The program restricts who may approve (`approver_allowed`).
    pub(super) restricts: bool,
    /// The approval this apply was given, verified.
    pub(super) approved: Option<crate::approval::Verified>,
}

/// What an approval is of: a plan file, an apply's plan, a destroy's.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Asked {
    File,
    Apply,
    Destroy,
}

impl Approvals<'_> {
    /// `token` verified for the plan of digest `digest`, which needs
    /// `needs` approved.
    pub(super) fn verify(
        &self,
        token: &str,
        needs: &[(String, String)],
        digest: &str,
        facts: &BTreeSet<Atom>,
    ) -> Result<crate::approval::Verified> {
        let stack_cfg = &self.located.loaded.cfg;
        if stack_cfg.approvals.is_empty() {
            bail!(
                "stack {} has no approvals trust root (dform.toml: `[stacks.{}] approvals = \
                 'jwks(\"https://...\")'`)",
                self.cx.deployment,
                self.located.loaded.stack
            );
        }
        let roots = match self.roots.get() {
            Some(r) => r,
            None => {
                let r = crate::approval::load_roots(&stack_cfg.approvals, &self.cx.cache)?;
                self.roots.get_or_init(|| r)
            }
        };
        let instance = &self.located.instance;
        let expect = crate::approval::Expect {
            stack: &instance.stack,
            key: &instance.key,
            digest,
            now: crate::approval::now(),
        };
        let allowed = |w: &str, d: &str| crate::approval::approver_allowed(facts, w, d);
        let allowed: Option<executor::Allowed> = if self.restricts { Some(&allowed) } else { None };
        executor::approve(token, needs, roots, &expect, allowed)
    }

    /// A batch apply's approval, before its Apply calls: at tick 1 the
    /// token given (`--approval FILE`), verified, or, with none, a refusal
    /// if anything needs one; at a later tick, a new deformation that needs
    /// one must be one the approver may approve. Each verdict at tick 1
    /// goes to the audit log.
    pub(super) fn entry(
        &mut self,
        teller: &Teller,
        tick: usize,
        t: &Tick,
        token: Option<&Path>,
        asked: Asked,
    ) -> Result<()> {
        if tick > 1 {
            return self.later(tick, t);
        }
        let (needs, facts, audit) = (&t.needs, &t.planned.res.facts, &self.cx.audit);
        let digest = t.digest.as_deref().unwrap_or_default();
        let Some(path) = token else {
            if needs.is_empty() {
                return audit
                    .append("approval", serde_json::json!({ "result": "not required" }))
                    .map(drop);
            }
            let error = format!("{} an approval, and no --approval was given", listed(needs));
            let mut entry = serde_json::json!({ "result": "refused", "digest": digest });
            crate::audit::error(&mut entry, &error);
            audit.append("approval", entry)?;
            let (verb, how) = match asked {
                Asked::File => (
                    "apply",
                    "apply it with --approval FILE, a signed approval of that digest",
                ),
                Asked::Apply => (
                    "apply",
                    "write the plan with `plan --out PLAN`, have its digest approved, and \
                     `apply PLAN --approval FILE`",
                ),
                Asked::Destroy => (
                    "destroy",
                    "have that digest approved (`plan --destroy` prints it) and \
                     `destroy --approval FILE`",
                ),
            };
            bail!("{verb} refused: {error}; the plan's digest is {digest}: {how}");
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read --approval {}: {e}", path.display()))?;
        match self.verify(&text, needs, digest, facts) {
            Ok(v) => {
                audit.append(
                    "approval",
                    serde_json::json!({ "result": "approved", "digest": digest, "attestation": v }),
                )?;
                teller.say(Said::Approved {
                    by: v.statement.approver.clone(),
                    digest: digest.to_string(),
                });
                self.approved = Some(v);
                Ok(())
            }
            Err(e) => {
                let mut entry = serde_json::json!({ "result": "refused", "digest": digest });
                crate::audit::error(&mut entry, &e.to_string());
                audit.append("approval", entry)?;
                let verb = match asked {
                    Asked::Destroy => "destroy",
                    _ => "apply",
                };
                bail!("{verb} refused: {e}")
            }
        }
    }

    /// At a later tick, a new deformation that needs an approval must be
    /// one the approver of this apply's may approve.
    fn later(&self, tick: usize, t: &Tick) -> Result<()> {
        let (needs, facts) = (&t.needs, &t.planned.res.facts);
        if needs.is_empty() {
            return Ok(());
        }
        let Some(v) = &self.approved else {
            bail!(
                "apply stopped at tick {tick}: {} an approval, and the apply has none",
                listed(needs)
            );
        };
        let who = &v.statement.approver;
        let refused: Vec<(String, String)> = needs
            .iter()
            .filter(|(d, _)| self.restricts && !crate::approval::approver_allowed(facts, who, d))
            .cloned()
            .collect();
        if !refused.is_empty() {
            bail!(
                "apply stopped at tick {tick}: approver_allowed({who:?}, D) does not hold for {}",
                refused
                    .iter()
                    .map(|(d, _)| d.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }
}

/// The deformations that need an approval, and why, as a refusal names
/// them: `D (R) needs`.
fn listed(needs: &[(String, String)]) -> String {
    let names: Vec<String> = needs.iter().map(|(d, r)| format!("{d} ({r})")).collect();
    match names.len() {
        1 => format!("{} needs", names[0]),
        _ => format!("{} need", names.join(", ")),
    }
}
