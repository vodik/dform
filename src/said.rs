//! What an apply says on stdout around its ticks' blocks (R-206, After
//! R-206): each tick's plan, the policies after a tick, a provider
//! configured at a boundary, what a re-planned tick differs in, and the
//! questions. The apply's ticks decide (`cli::apply`); a [`Teller`] over
//! the [`Said`] events words them, as `progress`'s printer words the
//! block on stderr: a fixed list of events prints the same every time
//! (tests/apply_said.rs), and what a run said is read back as events
//! where the promise is the decision, not its words
//! (`DFORM_TEST_SAID=FILE`, a JSON line per event, as
//! `DFORM_TEST_ANSWERS` answers the questions).

use dform_core::report::{self, Paint, Style};
use dform_core::zset::file::Difference;
use std::io::Write;
use std::path::PathBuf;

/// What a tick asks before it runs. Only `y` or `yes` proceeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Tick 1: apply the plan's `n` changes to `deployment` (`destroy`:
    /// delete its `n` objects)?
    Plan {
        n: usize,
        destroy: bool,
        deployment: String,
    },
    /// A later tick that adds to the plan shown, asked on its header line
    /// (`tick 2  1 change   apply? [y/N]`): its block's header takes the
    /// question's place.
    Tick {
        tick: usize,
        n: usize,
        destroy: bool,
    },
    /// The plan empties what the last apply left (R-80): `what`, as
    /// `zset::Emptied::what` says it.
    Emptied { what: String },
}

impl Question {
    /// The question as it is asked, before ` [y/N] `, and its paint.
    pub fn text(&self) -> (String, Paint) {
        let verb = |destroy: bool| match destroy {
            true => "destroy",
            false => "apply",
        };
        match self {
            Question::Tick { tick, n, destroy } => (
                format!(
                    "{}   {}?",
                    report::progress::title(*tick, *n),
                    verb(*destroy)
                ),
                Paint::Bold,
            ),
            Question::Plan {
                n,
                destroy,
                deployment,
            } => (
                match (destroy, n) {
                    (true, 1) => format!("Destroy this object of {deployment}?"),
                    (true, _) => format!("Destroy these {n} objects of {deployment}?"),
                    (false, 1) => format!("Apply this change to {deployment}?"),
                    (false, _) => format!("Apply these {n} changes to {deployment}?"),
                },
                Paint::Bold,
            ),
            Question::Emptied { what } => {
                (format!("T{}. Apply it anyway?", &what[1..]), Paint::Warn)
            }
        }
    }
}

/// What an apply says on stdout, in the order it says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// The resources the program moved (`moved` blocks), as the plan
    /// says them, before tick 1.
    Moved(String),
    /// Tick `tick`'s plan, as the plan prints it (`text`): a later tick's
    /// from that tick to the end, its values as the boundary learned
    /// them. `boundary`: another tick follows it; `idle`: a later tick
    /// with no change of its own (it only waits), which the report gives
    /// no section of its own.
    Plan {
        tick: usize,
        boundary: bool,
        idle: bool,
        text: String,
    },
    /// What tick 1 carries over from an interrupted apply, as
    /// `executor::carried_over` says it.
    Carried(String),
    /// The approval given (`--approval FILE`), verified: its approver and
    /// the plan's digest.
    Approved { by: String, digest: String },
    /// The policy block under tick `after`, as the boundary after it
    /// re-checked them (`report::policy::after`).
    Policies { after: usize, text: String },
    /// A provider configured at the boundary after tick `after`, each
    /// setting as it may be shown.
    Configured {
        provider: String,
        after: usize,
        settings: Vec<String>,
    },
    /// Tick `tick` re-planned at its boundary differs from the plan shown
    /// (After R-156).
    Differs {
        tick: usize,
        differences: Vec<Difference>,
    },
    /// A question, asked; its answer follows.
    Asked(Question),
    /// The answer to the question asked: yes, or not.
    Answered(bool),
    /// What chaos did to the fake world in a tick (`dev --chaos`).
    Chaos(String),
}

impl Said {
    /// The event as `DFORM_TEST_SAID` records it.
    pub fn json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Said::Moved(text) => json!({ "said": "moved", "text": text }),
            Said::Plan {
                tick,
                boundary,
                idle,
                text,
            } => json!({
                "said": "plan", "tick": tick, "boundary": boundary, "idle": idle, "text": text,
            }),
            Said::Carried(text) => json!({ "said": "carried", "text": text }),
            Said::Approved { by, digest } => {
                json!({ "said": "approved", "by": by, "digest": digest })
            }
            Said::Policies { after, text } => {
                json!({ "said": "policies", "after": after, "text": text })
            }
            Said::Configured {
                provider,
                after,
                settings,
            } => json!({
                "said": "configured", "provider": provider, "after": after, "settings": settings,
            }),
            Said::Differs { tick, differences } => json!({
                "said": "differs",
                "tick": tick,
                "differences": differences.iter().map(|d| json!({
                    "mark": d.mark.to_string(),
                    "address": report::address(&d.addr),
                    "path": d.path,
                    "what": d.what,
                })).collect::<Vec<_>>(),
            }),
            Said::Asked(q) => {
                let mut j = match q {
                    Question::Plan { n, destroy, .. } => {
                        json!({ "question": "plan", "changes": n, "destroy": destroy })
                    }
                    Question::Tick { tick, n, destroy } => json!({
                        "question": "tick", "tick": tick, "changes": n, "destroy": destroy,
                    }),
                    Question::Emptied { what } => json!({ "question": "emptied", "what": what }),
                };
                j["said"] = "asked".into();
                j
            }
            Said::Answered(yes) => json!({ "said": "answered", "yes": yes }),
            Said::Chaos(note) => json!({ "said": "chaos", "note": note }),
        }
    }
}

/// The printer of what an apply says: `quiet` (`-q`) says each tick's
/// bare plan under `tick N:` and no more than it must; otherwise each
/// later tick's plan stands apart from the block above it.
#[derive(Debug, Clone, Copy)]
pub struct Teller {
    pub quiet: bool,
    pub style: Style,
}

impl Teller {
    /// Event `s`, written to `out`.
    pub fn write(&self, s: &Said, out: &mut dyn Write) -> std::io::Result<()> {
        match s {
            Said::Moved(text) | Said::Carried(text) => write!(out, "{text}"),
            Said::Plan {
                tick,
                boundary,
                idle,
                text,
            } => {
                match self.quiet {
                    true if *tick > 1 || *boundary => writeln!(out, "tick {tick}:")?,
                    true => {}
                    // Apart from the block above it; a tick that only
                    // waits says which tick the report is of.
                    false if *tick > 1 => {
                        writeln!(out)?;
                        if *idle {
                            writeln!(out, "{}", report::progress::title(*tick, 0))?;
                        }
                    }
                    false => {}
                }
                write!(out, "{text}")
            }
            Said::Approved { by, digest } => {
                writeln!(out, "approved by {by}: plan digest {digest}")
            }
            Said::Policies { text, .. } => write!(out, "\n{text}"),
            // A setting's value is said at `-v`; `-q` says none of it.
            Said::Configured {
                provider,
                after,
                settings,
            } => match self.quiet {
                true => Ok(()),
                false => writeln!(
                    out,
                    "provider {provider}: configured after tick {after}: {}",
                    settings.join(", ")
                ),
            },
            Said::Differs { tick, differences } => {
                writeln!(out, "tick {tick} differs from the plan shown:")?;
                for d in differences {
                    writeln!(out, "{}", d.line())?;
                }
                Ok(())
            }
            Said::Asked(q) => {
                let (text, paint) = q.text();
                write!(out, "{} [y/N] ", self.style.paint(paint, &text))?;
                out.flush()
            }
            // The terminal echoed it.
            Said::Answered(_) => Ok(()),
            Said::Chaos(note) => writeln!(out, "chaos: {note}"),
        }
    }

    /// Event `s` said: on stdout, and recorded where `DFORM_TEST_SAID`
    /// names a file.
    pub fn say(&self, s: Said) {
        let _ = self.write(&s, &mut std::io::stdout().lock());
        if let Some(path) = record() {
            let line = format!("{}\n", s.json());
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| f.write_all(line.as_bytes()));
        }
    }
}

/// The file `DFORM_TEST_SAID` names: what a run says, recorded for a
/// test to read back as events.
fn record() -> Option<PathBuf> {
    std::env::var_os("DFORM_TEST_SAID")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}
