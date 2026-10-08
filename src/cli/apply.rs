use super::*;

/// Ask on the terminal whether to apply `n` changes to `deployment`
/// (`destroy`: to delete its `n` objects), or (`new`) tick `tick`, which
/// adds what no plan listed: only `y` or `yes` proceeds.
/// With no terminal to ask on, a refusal naming `--yes`, never a wait.
/// Answered no, `false`: a decline, not an error.
pub(super) fn confirm(
    n: usize,
    new: bool,
    destroy: bool,
    deployment: &str,
    tick: usize,
    style: report::Style,
) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    let stdin = std::io::stdin();
    let verb = match destroy {
        true => "destroy",
        false => "apply",
    };
    if !stdin.is_terminal() {
        bail!(
            "{verb} {deployment}: nothing to ask on at tick {tick} (stdin is not a terminal); \
             pass --yes to {verb} without asking"
        );
    }
    let ask = match (new, destroy, n) {
        (true, _, _) => format!("Apply tick {tick} to {deployment}?"),
        (false, true, 1) => format!("Destroy this object of {deployment}?"),
        (false, true, _) => format!("Destroy these {n} objects of {deployment}?"),
        (false, false, 1) => format!("Apply this change to {deployment}?"),
        (false, false, _) => format!("Apply these {n} changes to {deployment}?"),
    };
    print!("{} [y/N] ", style.paint(report::Paint::Bold, &ask));
    std::io::stdout().flush()?;
    let answer = answer()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// A line from the terminal, the answer to a question. A signal while it
/// waits (Ctrl-C at the prompt) is the stop it asks for (`interrupt`):
/// nothing was applied for the question. The line is read on a thread of
/// its own, which a stop leaves blocked on stdin until the process exits.
pub(super) fn answer() -> Result<String> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("dform-prompt".into())
        .spawn(move || {
            let mut line = String::new();
            let r = std::io::stdin().lock().read_line(&mut line).map(|_| line);
            let _ = tx.send(r);
        })?;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(line) => return Ok(line?),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::interrupt::requested().is_some() {
                    // The prompt's line ends here, not the shell's.
                    println!();
                    crate::interrupt::check()?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("internal: the prompt's reader is gone")
            }
        }
    }
}

/// Ask whether to apply a plan that empties `e` (R-80), on a terminal
/// only: there is no `--yes` for it, only `--allow-empty`. Answered no,
/// `false`.
pub(super) fn confirm_emptied(
    e: &zset::Emptied,
    deployment: &str,
    style: report::Style,
) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!(
            "apply {deployment}: {}; nothing to ask on (stdin is not a terminal): confirm it \
             on a terminal, or pass --allow-empty {} if it is meant",
            e.what(),
            e.flag()
        );
    }
    let what = e.what();
    let ask = format!("T{}. Apply it anyway?", &what[1..]);
    print!("{} [y/N] ", style.paint(report::Paint::Warn, &ask));
    std::io::stdout().flush()?;
    let answer = answer()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The apply's lock, checked still held before a question: a person never
/// answers one while the lease is lost or no longer renewed.
pub(super) fn still_held(session: &Option<Session>) -> Result<()> {
    match session {
        Some(s) => s.lock.check(),
        None => Ok(()),
    }
}

/// An apply whose confirmation of `tick` was answered no: at tick 1
/// nothing was applied, and nothing is said; later, what the earlier ticks
/// did. The audit log's `apply_end` says `declined`, and at which tick.
pub(super) fn declined(deployment: &str, tick: usize) -> Outcome {
    let why = (tick > 1).then(|| {
        format!(
            "apply {deployment}: not confirmed at tick {tick}; ticks 1 to {} were applied, \
             and the next apply resumes from there",
            tick - 1
        )
    });
    Outcome::Declined { tick, why }
}

pub(super) fn stopped(s: Stopped) -> Outcome {
    Outcome::Stopped {
        tick: s.tick,
        why: s.to_string(),
    }
}

/// An apply of a plan file or an approval that stopped after `tick`: the
/// next tick adds `new` changes no printed plan named (`unnamed`, the
/// groups the last plan held them as). The audit log's `apply_end` says `stopped`.
#[derive(Debug)]
pub(super) struct Stopped {
    pub(super) tick: usize,
    pub(super) new: usize,
    pub(super) unnamed: Vec<String>,
    /// The changes are what `later` held waiting on a provider's settings,
    /// named but planned only now (R-45): the plan file or approval did
    /// not see their diff.
    pub(super) on_provider: bool,
    /// The tick re-planned at its boundary differs from the tick the plan
    /// file or approval showed (After R-156): what it differs in, each an
    /// address or an attribute as the plan prints it.
    pub(super) differs: Vec<String>,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = if self.new == 1 { "" } else { "s" };
        let groups = match self.unnamed.as_slice() {
            [] => String::new(),
            gs => format!(" ({})", gs.join("; ")),
        };
        let what = match self.on_provider {
            _ if !self.differs.is_empty() => format!(
                "differs from the plan it applies: {}",
                self.differs.join(", ")
            ),
            true => format!(
                "plans {} change{s} `later` held for a provider's settings, which the \
                 approved plan did not show",
                self.new
            ),
            false => format!(
                "adds {} change{s} the plan could not name{groups}",
                self.new
            ),
        };
        write!(
            f,
            "apply stopped after tick {}: tick {} {what}; run apply again to plan them \
             against the world as it now is",
            self.tick,
            self.tick + 1,
        )
    }
}

/// The nulls a tick waits on that waiting can resolve (R-81,
/// `deployment::waitable`): of the stuck rules' and the held resources'.
pub(super) fn waiting_on(
    sections: &stuck::Sections,
    st: &state::State,
    externs: &crate::externs::Externs,
) -> Vec<String> {
    let on: BTreeSet<String> = sections
        .blocking
        .iter()
        .chain(sections.pending.values().flatten())
        .cloned()
        .collect();
    deployment::waitable(&on, st, &externs.not_yet())
}

/// Every provider call sent again since the last time (R-81) to the audit
/// log, a `retry` entry each, its error redacted.
pub(super) fn log_retries(
    audit: &crate::audit::Log,
    redact: &query::Redactor,
    backend: &crate::plugin::Providers,
    tick: usize,
) -> Result<()> {
    for r in backend.take_retries() {
        audit.append(
            "retry",
            crate::audit::retry(tick, &r, redact.text(&r.error)),
        )?;
    }
    Ok(())
}

/// Keep in state each `memo.first` value the apply read that state does
/// not keep yet (R-60), a secret one sealed with the stack's key.
pub(super) fn keep_memos(
    st: &mut state::State,
    externs: &crate::externs::Externs,
    master: &crate::custody::Master,
) -> Result<()> {
    crate::memo::keep(st, externs.memos(), master, &crate::memo::now())
}

/// `moved/3` rewrites applied to state before the plan.
pub(super) fn print_moves(moves: &[(ir::Address, ir::Address)]) {
    print!("{}", report::moved_text(moves));
}

/// The actions of `plan` a run that does not hold the master cannot make
/// (R-164), each with the paths that need it (`Providers::needs_master`),
/// and what depends on one (no paths): what references it, and the
/// delete of what it references.
pub(super) fn needing_master(
    plan: &crate::provider::Plan,
    desired: &[ir::Resource],
    st: &state::State,
    backend: &crate::plugin::Providers,
) -> std::collections::BTreeMap<ir::Address, Vec<String>> {
    let mut out = std::collections::BTreeMap::new();
    if !crate::secrets::standin::active() {
        return out;
    }
    let doc = |a: &ir::Address| {
        desired
            .iter()
            .find(|r| r.addr == *a)
            .map(|r| crate::engine::value_to_json(&r.attrs))
    };
    for a in &plan.actions {
        let paths = backend.needs_master(a, doc(&a.addr).as_ref());
        if !paths.is_empty() {
            out.insert(a.addr.clone(), paths);
        }
    }
    let deps = |a: &ir::Address| -> BTreeSet<ir::Address> {
        let mut d: BTreeSet<ir::Address> = desired
            .iter()
            .find(|r| r.addr == *a)
            .map(|r| r.deps.clone())
            .unwrap_or_default();
        d.extend(
            st.get(a)
                .into_iter()
                .flat_map(|e| e.deps.iter().filter_map(|k| state::parse_key(k))),
        );
        d
    };
    loop {
        let more: Vec<ir::Address> = plan
            .actions
            .iter()
            .filter(|a| !out.contains_key(&a.addr))
            .filter(|a| match a.kind {
                ActionKind::Noop | ActionKind::Pending => false,
                ActionKind::Delete | ActionKind::DeleteDeposed | ActionKind::Forget => {
                    out.keys().any(|n| deps(n).contains(&a.addr))
                }
                _ => deps(&a.addr).iter().any(|d| out.contains_key(d)),
            })
            .map(|a| a.addr.clone())
            .collect();
        if more.is_empty() {
            return out;
        }
        for m in more {
            out.insert(m, Vec::new());
        }
    }
}

/// Why a run that does not hold the master stopped: what it did not make.
pub(super) fn needing_text(
    deployment: &str,
    needing: &std::collections::BTreeMap<ir::Address, Vec<String>>,
    without: Option<&str>,
) -> String {
    let n = match needing.len() {
        1 => "1 change".to_string(),
        n => format!("{n} changes"),
    };
    let mut out = format!(
        "apply {deployment}: stopped; {n} need its master ({}), and were not made:",
        without.unwrap_or("not held")
    );
    for (a, paths) in needing {
        let what = match paths.as_slice() {
            [] => "depends on one of these".to_string(),
            [p] if p.is_empty() => "holds a secret only the master derives".to_string(),
            ps => format!(
                "{} only the master derives",
                ps.iter()
                    .filter(|p| !p.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        out.push_str(&format!("\n  {}: {what}", report::address(a)));
    }
    out.push_str("\nevery other change was made; apply again with the master to make these");
    out
}

/// What an approval is of: a plan file, an apply's plan, a destroy's.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Asked {
    File,
    Apply,
    Destroy,
}

/// A batch apply's approval, before its Apply calls: at tick 1 the token
/// given (`--approval FILE`), verified (`verify`), or, with none, a
/// refusal if anything needs one; at a later tick, a new deformation that
/// needs one must be one the approver may approve (`allowed`). Each
/// verdict at tick 1 goes to the audit log.
#[allow(clippy::too_many_arguments)]
pub(super) fn approve_entry(
    tick: usize,
    needs: &[(String, String)],
    digest: Option<&str>,
    token: Option<&Path>,
    asked: Asked,
    approved: &mut Option<crate::approval::Verified>,
    audit: &crate::audit::Log,
    verify: &dyn Fn(&str, &str) -> Result<crate::approval::Verified>,
    allowed: &dyn Fn(&str, &str) -> bool,
) -> Result<()> {
    let list = |needs: &[(String, String)]| {
        let names: Vec<String> = needs.iter().map(|(d, r)| format!("{d} ({r})")).collect();
        match names.len() {
            1 => format!("{} needs", names[0]),
            _ => format!("{} need", names.join(", ")),
        }
    };
    if tick > 1 {
        if needs.is_empty() {
            return Ok(());
        }
        let Some(v) = approved else {
            bail!(
                "apply stopped at tick {tick}: {} an approval, and the apply has none",
                list(needs)
            );
        };
        let who = &v.statement.approver;
        let refused: Vec<(String, String)> = needs
            .iter()
            .filter(|(d, _)| !allowed(who, d))
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
        return Ok(());
    }
    let digest = digest.unwrap_or_default();
    let Some(path) = token else {
        if needs.is_empty() {
            return audit
                .append("approval", serde_json::json!({ "result": "not required" }))
                .map(drop);
        }
        let error = format!("{} an approval, and no --approval was given", list(needs));
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
    match verify(&text, digest) {
        Ok(v) => {
            audit.append(
                "approval",
                serde_json::json!({ "result": "approved", "digest": digest, "attestation": v }),
            )?;
            println!("approved by {}: plan digest {digest}", v.statement.approver);
            *approved = Some(v);
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
