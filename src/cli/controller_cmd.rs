use super::*;

/// `dform controller run`: a run per event (`controller::Hook`), until
/// `--once` or `--max-events` says stop. A run that fails is logged and the
/// controller goes on watching; the first one failing ends it.
pub(super) fn run_controller(cli: Cli) -> Result<Outcome> {
    let Cmd::Controller {
        poll,
        once,
        max_events,
        max_ticks,
    } = cli.cmd.clone()
    else {
        unreachable!("run_controller is for `controller`");
    };
    let files = cli.files.clone();
    let program = loader::load_program(&files)?;
    let cfg = crate::stack::config(&program)?;
    let name = cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    // One controller per deployment: of a keyed stack, the one the target
    // names, every key value spelled out.
    let missing: Vec<&str> = cfg
        .keys
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !cli.keys.iter().any(|(x, _)| x == k))
        .collect();
    if !missing.is_empty() {
        bail!(
            "controller run names its deployment: stack {name} is keyed by {}, and the target \
             gives no {}: `dform controller run {name} {}`",
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
    let own = crate::stack::instance(&cfg, &name, &program, &inputs::set_facts(&[], &set)?)?.name();
    let registered = crate::stack::registry(&cli.root)?
        .get(&own)
        .is_some_and(|e| e.bootstrap);
    if cfg.bootstrap || registered {
        bail!("stack {own} is role = bootstrap: it stays batch, and the controller never runs it");
    }
    controller::log(format_args!(
        "controller {own}: {}, poll {poll}ms",
        files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let apply = Cli {
        cmd: Cmd::Apply {
            plan_file: None,
            chaos: vec![],
            max_ticks,
            parallel: 1,
            approval: None,
            // The controller runs unattended: it never asks.
            yes: true,
            allow_empty: Vec::new(),
            why: report::Why::None,
            destroy: false,
            new_master: false,
        },
        ..cli
    };
    let mut hook = controller::Hook::default();
    let mut events = 0;
    loop {
        // What the run registers for diagnostics is dropped with it; the
        // program's files stay parsed (`loader`) until they change.
        let sources = crate::diag::Scope::new();
        if let Err(e) = run(apply.clone(), Some(&mut hook)) {
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
        events += 1;
        // Tests measure the source registry after every event.
        if let Some(path) = std::env::var_os("DFORM_TEST_SOURCES") {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{}", crate::diag::source_count())?;
        }
        // SIGTERM (what systemd and Kubernetes send) or Ctrl-C: after the
        // event, which stopped at its next Apply call, the lease released.
        let stop = |signal: i32| {
            controller::log(format_args!(
                "controller {own}: {}: stopped",
                crate::interrupt::name(signal)
            ));
            Ok(Outcome::Interrupted { signal })
        };
        if let Some(signal) = crate::interrupt::requested() {
            return stop(signal);
        }
        if once || max_events.is_some_and(|n| events >= n) {
            return Ok(Outcome::Done);
        }
        while !hook.changed() {
            if crate::interrupt::sleep(std::time::Duration::from_millis(poll))
                && let Some(signal) = crate::interrupt::requested()
            {
                return stop(signal);
            }
        }
    }
}
