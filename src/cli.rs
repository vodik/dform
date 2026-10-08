//! The command line (`dform`, docs/reference.md). `main` takes the backend the
//! run reaches its providers through. A run ([`Cli`]) is one command
//! ([`Cmd`]), each in a module of its own, started at the step of the run
//! it needs ([`Stage`]): `run_with` is the dispatch.

use crate::plugin;
use crate::report;
use crate::store;
use anyhow::{Result, bail};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod apply;
mod args;
mod cmd;
mod complete;
mod controller_cmd;
mod dev;
mod evaluated;
mod explain;
mod matrix;
mod order;
mod outputs;
mod plan;
mod planning;
mod provider_cmd;
mod run_inputs;
mod secrets;
mod source;
mod stack;
mod state_cmd;
mod test;

use self::args::{Args, Command, EXPERIMENTAL};
use self::cmd::{Cmd, Stage};
use self::evaluated::{Evaluated, Objects};
use self::order::{Dependency, InOrder, apply_order};

/// How this run reaches its providers (`main`'s `launch`).
static LAUNCH: std::sync::OnceLock<&'static (dyn plugin::Launch + Sync)> =
    std::sync::OnceLock::new();

fn launch() -> &'static dyn plugin::Launch {
    *LAUNCH
        .get()
        .expect("internal: cli::main sets the providers' backend")
}

/// One run: the target's files and key, the project's state root, what
/// the run is given, and the command.
#[derive(Debug, Clone)]
struct Cli {
    cmd: Cmd,
    files: Vec<PathBuf>,
    /// The target's key values, as `k=v`; also in `set`.
    keys: Vec<(String, String)>,
    /// `--set` as typed (without the target's keys).
    user_set: Vec<String>,
    set: Vec<String>,
    input_files: Vec<PathBuf>,
    data: Vec<String>,
    show_noop: bool,
    providers: Vec<String>,
    world: Option<PathBuf>,
    /// The state root, `dform.state/` at the project root.
    root: PathBuf,
    /// The working directory is in a project (`resolve`).
    in_project: bool,
    /// The program's project manifest.
    manifest: Option<crate::project::Manifest>,
    inventory: Option<PathBuf>,
    audit_sink: Option<String>,
    /// How plan text is painted on stdout (`--color`).
    style: report::Style,
    /// How a result set prints on stdout: `style`, at the terminal's width.
    table: report::table::Options,
    /// `apply` with no target in a project of several stacks: every one,
    /// in dependency order (`run_command`).
    every_stack: Vec<PathBuf>,
    /// The project module `plan`, `apply` or `test` runs on (R-114): with
    /// no target, the root's project.df.
    matrix: Option<PathBuf>,
    /// Where a plan's text goes.
    held: Held,
}

/// Where a plan's text goes: stdout, or held for the project's plan
/// (R-114), which says each deployment's state before their plans.
#[derive(Debug, Clone, Default)]
struct Held(Option<Arc<std::sync::Mutex<HeldPlan>>>);

/// A plan held: its text, and its summary line (`plan: 3 changes ..`).
#[derive(Debug, Default)]
struct HeldPlan {
    text: String,
    summary: Option<String>,
}

impl Held {
    fn new() -> Held {
        Held(Some(Arc::default()))
    }

    /// `text` to stdout, or held.
    fn print(&self, text: &str) {
        match &self.0 {
            Some(h) => h
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .text
                .push_str(text),
            None => print!("{text}"),
        }
    }

    /// The plan's summary line.
    fn summary(&self, summary: String) {
        if let Some(h) = &self.0 {
            h.lock().unwrap_or_else(|e| e.into_inner()).summary = Some(summary);
        }
    }

    /// What was held: the text and the summary, if a plan was made.
    fn take(&self) -> (String, Option<String>) {
        match &self.0 {
            Some(h) => {
                let mut h = h.lock().unwrap_or_else(|e| e.into_inner());
                (std::mem::take(&mut h.text), h.summary.take())
            }
            None => (String::new(), None),
        }
    }
}

/// The command line `args` (the program's name first), its providers
/// reached through `launch`: the `dform` binary's are processes over gRPC
/// (`dform-grpc`); `dform-direct`'s the mock linked in (`dform-mock`).
pub fn main(
    launch: &'static (dyn plugin::Launch + Sync),
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::process::ExitCode {
    if LAUNCH.set(launch).is_err() {
        panic!("internal: cli::main runs once per process");
    }
    crate::timing::begin();
    let args = Args::parse_from(args);
    let color = args.inputs.color;
    // Ctrl-C and SIGTERM ask the run to stop and unwind (`interrupt`):
    // dform's own, not a provider's or the language server's.
    let _signals = (!matches!(args.cmd, Command::ServeProvider { .. } | Command::Lsp))
        .then(crate::interrupt::install);
    let result = match &args.cmd {
        Command::ServeProvider { name } => serve_provider(name).map(|()| Outcome::Done),
        Command::Lsp => dform_lsp::serve_stdio(dform_lsp::Options {
            version: env!("CARGO_PKG_VERSION"),
            real: launch,
        })
        .map(|()| Outcome::Done),
        Command::Version => {
            println!("dform {}", env!("CARGO_PKG_VERSION"));
            println!("tzdb {}", crate::time::tzdb_version());
            println!(
                "wasm host {}",
                if dform_host::WASM {
                    "in (experimental)"
                } else {
                    "out (build with --features wasm)"
                }
            );
            Ok(Outcome::Done)
        }
        _ => args.resolve().and_then(run_command),
    };
    crate::timing::finish();
    // Every exit goes through `exit_code`: a signal's 128 + its number
    // once every destructor of the run has run; a decline or a stop said
    // what it had to (`run`).
    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            use std::io::IsTerminal;
            let color = color.style(std::io::stderr().is_terminal()).color;
            match e.downcast_ref::<Refused>().filter(|r| r.footer) {
                Some(r) => eprintln!("{r}"),
                None => eprint!("{}", crate::diag::report(&e, color)),
            }
            Outcome::of_error(&e)
        }
    };
    std::process::ExitCode::from(exit_code(&outcome))
}

/// The command line's definitions, for the manual (`man`).
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

/// `dform __provider NAME`: serve the built-in provider NAME until stdin
/// closes. The mock is the only one; the Kubernetes provider is its own
/// executable (`dform-provider-k8s`).
fn serve_provider(name: &str) -> Result<()> {
    match name {
        "fake" => dform_grpc::server::serve(dform_mock::Mock::process()),
        _ => bail!("no built-in provider {name:?}: dform serves only `fake`"),
    }
}

/// The command line `args` in this process, as often as a caller likes:
/// `main` returning the error instead of printing it. A process reaches its
/// providers one way: every call passes the same `launch`, and `main` has
/// not set another. For tests that drive many runs (`tests/model.rs`). A
/// decline or a stop is `Ok`, as `main` exits 0 for one.
pub fn run_in_process(
    launch: &'static (dyn plugin::Launch + Sync),
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<()> {
    let set = *LAUNCH.get_or_init(|| launch);
    if !std::ptr::addr_eq(set, launch) {
        bail!("internal: this process reaches its providers through another backend");
    }
    run_command(Args::try_parse_from(args)?.resolve()?).map(|_| ())
}

/// The command line's run: `apply X` applies the stacks X reads first
/// ([`apply_order`]), each a run of its own; one declined or stopped ends
/// the command there, before the stacks that read it.
fn run_command(cli: Cli) -> Result<Outcome> {
    if let Some(module) = cli.matrix.clone() {
        return matrix::run(cli, &module);
    }
    let order = apply_order(&cli)?;
    if order.is_empty() {
        return run(cli, None);
    }
    InOrder::new(cli, order).run()
}

/// An apply's audit session: its log, and the stack's lock, held until the
/// apply's end is logged and released then ([`run`]).
struct Session {
    log: crate::audit::Log,
    lock: crate::store::Guard,
}

/// How a run ended: what the exit status says ([`exit_code`], R-147;
/// docs/reference.md "Exit status"). A decline and a stop are outcomes a
/// person chose or a plan file bounds, not errors: nothing is said as an
/// error, and each has its own status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It ran to its end: an apply applied every tick.
    Done,
    /// The confirmation of `tick` was answered no; nothing of that tick
    /// was applied. `why` says what earlier ticks did, when there were
    /// any.
    Declined { tick: usize, why: Option<String> },
    /// A plan file or an approval applied its ticks to `tick` and stopped
    /// before one that adds what it did not show; `why` says so.
    Stopped { tick: usize, why: String },
    /// SIGINT or SIGTERM asked it to stop (`interrupt`, R-137): no new
    /// Apply call was made, the calls in flight were awaited and their
    /// answers logged; the next apply resumes. `main` exits 128 +
    /// `signal`, after every destructor ran.
    Interrupted { signal: i32 },
    /// The program refused the plan: its conflicts and denies, printed.
    Refused { conflicts: usize, denies: usize },
    /// Another run holds the stack's lock (named in the error).
    Locked,
    /// It failed (the error printed).
    Failed,
}

impl Outcome {
    /// The outcome of a run that ended in `e` (already printed): a
    /// refusal or a held lock by their types, else a failure.
    fn of_error(e: &anyhow::Error) -> Outcome {
        if let Some(r) = e.downcast_ref::<Refused>() {
            return Outcome::Refused {
                conflicts: r.conflicts,
                denies: r.denies,
            };
        }
        if e.is::<store::Held>() || e.is::<store::Locked>() {
            return Outcome::Locked;
        }
        Outcome::Failed
    }

    /// The word `--json` says it with (`outcome`).
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Declined { .. } => "declined",
            Outcome::Stopped { .. } => "stopped",
            Outcome::Interrupted { .. } => "interrupted",
            Outcome::Refused { .. } => "refused",
            Outcome::Locked => "locked",
            Outcome::Failed => "failed",
        }
    }
}

/// The exit status of a run that ended in `o` (docs/reference.md "Exit
/// status"); 2, a usage error, is the argument parser's own.
pub fn exit_code(o: &Outcome) -> u8 {
    match o {
        Outcome::Done => 0,
        Outcome::Failed => 1,
        Outcome::Declined { .. } => 3,
        Outcome::Refused { .. } => 4,
        Outcome::Stopped { .. } => 5,
        Outcome::Locked => 6,
        Outcome::Interrupted { signal } => 128u8.saturating_add(*signal as u8),
    }
}

/// The program refused the plan (its conflicts and denies were printed):
/// exit status 4. `what` is the message: a plan's as before R-147; an
/// apply's its last line ([`Refused::apply`]).
#[derive(Debug)]
pub struct Refused {
    what: String,
    conflicts: usize,
    denies: usize,
    /// `what` is an apply's last line, printed as it is (not an error's).
    footer: bool,
}

impl Refused {
    fn new(what: &str, conflicts: usize, denies: usize) -> Refused {
        Refused {
            what: what.to_string(),
            conflicts,
            denies,
            footer: false,
        }
    }

    /// An apply's (`verb`: or a destroy's) refusal, said as the plan's
    /// footer says it (R-122): `apply: refused  2 conflicts, 1 deny`,
    /// then where it stopped (`at`) when that was not before its first
    /// call.
    fn apply(verb: &str, conflicts: usize, denies: usize, at: Option<String>) -> Refused {
        let mut why = Vec::new();
        if conflicts > 0 {
            why.push(match conflicts {
                1 => "1 conflict".to_string(),
                n => format!("{n} conflicts"),
            });
        }
        if denies > 0 {
            why.push(match denies {
                1 => "1 deny".to_string(),
                n => format!("{n} denies"),
            });
        }
        let what = format!("{verb}: refused  {}", why.join(", "));
        Refused {
            what: match at {
                Some(at) => format!("{what}; {at}"),
                None => what,
            },
            conflicts,
            denies,
            footer: true,
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for Refused {}

/// One run of the command line. `hook`: controller mode's part of an apply
/// (`controller::Hook`). An apply's end, whatever it is, goes to the audit
/// log, and then its lock is released; what the outcome says, if
/// anything, is said last.
fn run(cli: Cli, hook: Option<&mut crate::controller::Hook>) -> Result<Outcome> {
    let mut session = None;
    // What resumes an interrupted run: the command that was interrupted.
    let verb = cli.cmd.verb();
    let r = interrupted(run_with(cli, hook, &mut session));
    let Some(s) = session else {
        if let Ok(Outcome::Interrupted { signal }) = &r {
            eprintln!("interrupted ({})", crate::interrupt::name(*signal));
        }
        return r;
    };
    let end = match &r {
        Ok(Outcome::Done) => serde_json::json!({ "result": "ok" }),
        Ok(Outcome::Declined { tick, .. }) => {
            serde_json::json!({ "result": "declined", "tick": tick })
        }
        Ok(Outcome::Stopped { tick, .. }) => {
            serde_json::json!({ "result": "stopped", "tick": tick })
        }
        Ok(Outcome::Interrupted { signal }) => serde_json::json!({
            "result": "stopped",
            "why": "interrupted",
            "signal": crate::interrupt::name(*signal),
        }),
        Ok(o) => serde_json::json!({ "result": o.word() }),
        Err(e) => {
            let mut end = serde_json::json!({ "result": "failed" });
            crate::audit::error(&mut end, &e.to_string());
            end
        }
    };
    let logged = s.log.append("apply_end", end);
    let released = s.lock.release();
    let outcome = match r {
        Ok(o) => o,
        Err(e) => {
            // The apply's error is the one reported; what else failed is
            // said beside it.
            for e in [logged.err(), released.err()].into_iter().flatten() {
                eprintln!("warning: {e:#}");
            }
            return Err(e);
        }
    };
    logged?;
    released?;
    if let Outcome::Declined { why: Some(why), .. } | Outcome::Stopped { why, .. } = &outcome {
        eprintln!("{why}");
    }
    if let Outcome::Interrupted { .. } = outcome {
        eprintln!("interrupted: the next {verb} resumes it");
    }
    Ok(outcome)
}

/// A run that a signal asked to stop is interrupted, however it unwound
/// (the executor's refusal of the next call, a prompt, a wait); an error
/// beside it, not the stop itself, is said as a warning.
fn interrupted(r: Result<Outcome>) -> Result<Outcome> {
    let Some(signal) = crate::interrupt::requested() else {
        return r;
    };
    if let Err(e) = &r
        && !e.is::<crate::interrupt::Interrupted>()
    {
        eprintln!("warning: {e:#}");
    }
    Ok(Outcome::Interrupted { signal })
}

/// One run's steps, each the start of the commands of its [`Stage`]: the
/// program loaded, the deployment located, its master read and the
/// deployment evaluated.
fn run_with(
    mut cli: Cli,
    mut hook: Option<&mut crate::controller::Hook>,
    session: &mut Option<Session>,
) -> Result<Outcome> {
    // A plan file's run is checked once its inputs (its world) are read.
    if !cli.in_project && !cli.cmd.applies_plan_file() && cli.needs_project() {
        return Err(crate::project::not_in_a_project(Path::new(".")));
    }
    if cli.cmd.experimental() {
        eprintln!("{EXPERIMENTAL}");
    }
    if cli.cmd.stage() == Stage::Alone {
        return Cmd::run_alone(cli);
    }
    let saved = match &cli.cmd {
        Cmd::Apply(a) => a.plan_file.clone(),
        _ => None,
    };
    let saved = match saved {
        Some(p) => Some((p.clone(), run_inputs::with_plan_inputs(&mut cli, &p)?)),
        None => None,
    };
    if !cli.in_project && cli.needs_project() {
        return Err(crate::project::not_in_a_project(Path::new(".")));
    }
    let loaded = cli.load(hook.as_deref_mut())?;
    // `stack rekey`: the run is of the old deployment (its state, its
    // world), the provenance of its names is listed, and its state moves.
    let rekey = match cli.cmd.clone() {
        Cmd::Rekey(r) => Some(r.args(&mut cli, &loaded.cfg)?),
        _ => None,
    };
    if cli.cmd.needs_provider() {
        loaded.require_provider()?;
    }
    if cli.cmd.stage() == Stage::Program {
        return match &cli.cmd {
            Cmd::Test(c) => c.run(&cli, &loaded),
            Cmd::Strata(c) => c.run(&cli, &loaded),
            Cmd::Effects(c) => c.run(&cli, &loaded),
            Cmd::Graph(c) => c.run_strata(&loaded),
            c => unreachable!("{c:?} runs at another stage"),
        };
    }
    let objects = Objects::locate(cli, loaded, rekey.as_ref(), hook.as_deref_mut())?;
    if objects.cli.cmd.stage() == Stage::Objects {
        return match &objects.cli.cmd {
            Cmd::Log(c) => c.run(&objects),
            Cmd::Unlock(c) => c.run(&objects),
            Cmd::StateShow(c) => c.run(&objects),
            Cmd::Output(c) => c.run(&objects),
            Cmd::ForgetHost(c) => c.run(&objects),
            Cmd::StateMv(c) => c.run(&objects),
            c => unreachable!("{c:?} runs at another stage"),
        };
    }
    Evaluated::new(objects, saved, rekey, hook)?.run(session)
}

/// Opens an s3 location's store with the environment's credentials.
/// `writes`: the run writes there, so the bucket's conditional writes are
/// checked first (once per bucket: a pass is kept in the state root's
/// `cache/`).
fn open_s3(root: &Path, writes: bool) -> impl Fn(&store::S3Spec) -> Result<Arc<dyn store::Store>> {
    let cache = root.join("cache");
    move |spec| {
        let s = dform_s3::S3Store::open(spec, "")?;
        if writes {
            s.check_conditions(Some(&cache))?;
        }
        Ok(Arc::new(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every outcome has its own status (R-147); the match has no
    /// wildcard, so a new outcome is a compile error here until it is
    /// given one.
    #[test]
    fn each_outcome_has_its_own_exit_status() {
        let all = [
            Outcome::Done,
            Outcome::Failed,
            Outcome::Declined { tick: 1, why: None },
            Outcome::Refused {
                conflicts: 1,
                denies: 1,
            },
            Outcome::Stopped {
                tick: 1,
                why: String::new(),
            },
            Outcome::Locked,
            Outcome::Interrupted {
                signal: libc::SIGINT,
            },
            Outcome::Interrupted {
                signal: libc::SIGTERM,
            },
        ];
        for o in &all {
            match o {
                Outcome::Done
                | Outcome::Failed
                | Outcome::Declined { .. }
                | Outcome::Refused { .. }
                | Outcome::Stopped { .. }
                | Outcome::Locked
                | Outcome::Interrupted { .. } => {}
            }
        }
        let codes: Vec<u8> = all.iter().map(exit_code).collect();
        assert_eq!(codes, [0, 1, 3, 4, 5, 6, 130, 143]);
        let words: Vec<&str> = all.iter().map(Outcome::word).collect();
        assert_eq!(
            words,
            [
                "done",
                "failed",
                "declined",
                "refused",
                "stopped",
                "locked",
                "interrupted",
                "interrupted"
            ]
        );
    }
}
