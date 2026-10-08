//! `dform controller run` (R-41, experimental): a run per event.

use super::apply::Apply;
use super::run_inputs::split_kv;
use super::{Cli, Cmd, Outcome, run};
use crate::{controller, inputs, loader, state};
use anyhow::{Result, bail};

/// `dform controller run TARGET`.
#[derive(Debug, Clone)]
pub(super) struct Controller {
    /// How often to look at the sources and the world, in milliseconds.
    pub(super) poll: u64,
    pub(super) once: bool,
    pub(super) max_events: Option<usize>,
    pub(super) max_ticks: usize,
}

impl Controller {
    /// A run per event (`controller::Hook`), until `--once` or
    /// `--max-events` says stop. A run that fails is logged and the
    /// controller goes on watching; the first one failing ends it.
    pub(super) fn run(self, cli: Cli) -> Result<Outcome> {
        let own = self.deployment(&cli)?;
        controller::log(format_args!(
            "controller {own}: {}, poll {}ms",
            cli.files
                .iter()
                .map(|f| f.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            self.poll
        ));
        let apply = Cli {
            cmd: Cmd::Apply(Apply::unattended(self.max_ticks)),
            ..cli
        };
        let mut hook = controller::Hook::default();
        let mut events = 0;
        loop {
            self.event(&apply, &mut hook, events)?;
            events += 1;
            // SIGTERM (what systemd and Kubernetes send) or Ctrl-C: after
            // the event, which stopped at its next Apply call, the lease
            // released.
            if let Some(signal) = crate::interrupt::requested() {
                return Ok(stopped(&own, signal));
            }
            if self.once || self.max_events.is_some_and(|n| events >= n) {
                return Ok(Outcome::Done);
            }
            while !hook.changed() {
                if crate::interrupt::sleep(std::time::Duration::from_millis(self.poll))
                    && let Some(signal) = crate::interrupt::requested()
                {
                    return Ok(stopped(&own, signal));
                }
            }
        }
    }

    /// The deployment the controller runs: one per deployment, of a keyed
    /// stack the one the target names, every key value spelled out; never
    /// a `role = bootstrap` one.
    fn deployment(&self, cli: &Cli) -> Result<String> {
        let files = &cli.files;
        let program = loader::load_program(files)?;
        let cfg = crate::stack::config(&program)?;
        let name = cfg
            .name
            .clone()
            .unwrap_or_else(|| state::stack_name(&files[0]));
        let missing: Vec<&str> = cfg
            .keys
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !cli.keys.iter().any(|(x, _)| x == k))
            .collect();
        if !missing.is_empty() {
            bail!(
                "controller run names its deployment: stack {name} is keyed by {}, and the \
                 target gives no {}: `dform controller run {name} {}`",
                cfg.keys
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                missing.join(", "),
                cfg.keys
                    .iter()
                    .map(|(k, _)| format!("{k}=..."))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let set = cli
            .set
            .iter()
            .map(|kv| split_kv(kv).map(|(k, v)| (k.to_string(), v)))
            .collect::<Result<Vec<_>>>()?;
        let own =
            crate::stack::instance(&cfg, &name, &program, &inputs::set_facts(&[], &set)?)?.name();
        let registered = crate::stack::registry(&cli.root)?
            .get(&own)
            .is_some_and(|e| e.bootstrap);
        if cfg.bootstrap || registered {
            bail!(
                "stack {own} is role = bootstrap: it stays batch, and the controller never runs it"
            );
        }
        Ok(own)
    }

    /// One event: the apply run, its error logged; the first event's
    /// error ends the controller.
    fn event(&self, apply: &Cli, hook: &mut controller::Hook, events: usize) -> Result<()> {
        // What the run registers for diagnostics is dropped with it; the
        // program's files stay parsed (`loader`) until they change.
        let sources = crate::diag::Scope::new();
        if let Err(e) = run(apply.clone(), Some(hook)) {
            let text = crate::diag::report(&e, false);
            controller::log(format_args!(
                "error: {}",
                text.lines()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("error: ")
            ));
            if events == 0 {
                // The error is rendered after this returns.
                std::mem::forget(sources);
                return Err(e);
            }
            hook.failed()?;
        }
        drop(sources);
        // Tests measure the source registry after every event.
        if let Some(path) = std::env::var_os("DFORM_TEST_SOURCES") {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{}", crate::diag::source_count())?;
        }
        Ok(())
    }
}

/// The controller stopped by `signal`, said in its log.
fn stopped(own: &str, signal: i32) -> Outcome {
    controller::log(format_args!(
        "controller {own}: {}: stopped",
        crate::interrupt::name(signal)
    ));
    Outcome::Interrupted { signal }
}
