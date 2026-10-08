//! What an apply asks on the terminal, and how it ends when the answer is
//! no or a plan file bounds it.

use crate::cli::Outcome;
use crate::{report, zset};
use anyhow::{Result, bail};

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
fn answer() -> Result<String> {
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

impl From<Stopped> for Outcome {
    fn from(s: Stopped) -> Outcome {
        Outcome::Stopped {
            tick: s.tick,
            why: s.to_string(),
        }
    }
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
