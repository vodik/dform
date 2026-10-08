//! The command line (`dform`, docs/reference.md). `main` takes the backend the
//! run reaches its providers through.

use crate::ast::Atom;
use crate::ast::Term;
use crate::chaos::Chaos;
use crate::controller;
use crate::deployment::{self, Planned};
use crate::engine;
use crate::executor;
use crate::graph;
use crate::inputs;
use crate::ir;
use crate::loader;
use crate::partition;
use crate::plugin::{self, Providers};
use crate::provider::ActionKind;
use crate::query;
use crate::report::{self, waits_on};
use crate::schema;
use crate::state;
use crate::store;
use crate::stuck;
use crate::value::Value;
use crate::watch;
use crate::zset;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod apply;
mod args;
mod complete;
mod controller_cmd;
mod dev;
mod explain;
mod matrix;
mod order;
mod outputs;
mod planning;
mod run_inputs;
mod secrets;
mod source;
mod stack;
mod state_cmd;
mod test;

use self::{
    apply::*, args::*, complete::*, controller_cmd::*, dev::*, explain::*, order::*, outputs::*,
    planning::*, run_inputs::*, secrets::*, source::*, stack::*, state_cmd::*, test::*,
};

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

/// What a run does.
#[derive(Debug, Clone)]
enum Cmd {
    Eval,
    Plan {
        out: Option<PathBuf>,
        json: bool,
        /// `plan --destroy`: the plan against an empty wanted set.
        destroy: bool,
        why: report::Why,
        /// `--new-master` (R-163).
        new_master: bool,
    },
    Test,
    Apply {
        plan_file: Option<PathBuf>,
        chaos: Vec<String>,
        max_ticks: usize,
        parallel: u64,
        approval: Option<PathBuf>,
        /// `--yes`: no confirmation.
        yes: bool,
        /// `--allow-empty`: what the plan may empty without asking (R-80).
        allow_empty: Vec<String>,
        /// What each change of the printed plan says of why.
        why: report::Why,
        /// `destroy`: the deployment is removed (R-149).
        destroy: bool,
        /// `--new-master` (R-163).
        new_master: bool,
    },
    Query {
        pattern: String,
        json: bool,
    },
    Why {
        pattern: String,
        tree: bool,
        all: bool,
        core: bool,
        /// `-vv`: a long value whole (R-176).
        whole: bool,
        json: bool,
    },
    Diff {
        since: String,
        json: bool,
        /// How much each change says of why it was planned.
        why: report::Why,
    },
    Explain {
        addresses: Vec<String>,
    },
    Show {
        addr: String,
    },
    Strata,
    Effects {
        json: bool,
    },
    Fmt {
        paths: Vec<PathBuf>,
        check: bool,
    },
    Doc,
    Graph {
        what: Option<String>,
    },
    Controller {
        poll: u64,
        once: bool,
        max_events: Option<usize>,
        max_ticks: usize,
    },
    SecretsList {
        json: bool,
    },
    SecretsRotate {
        key: String,
    },
    SecretsCycle,
    SecretsSet {
        name: String,
        remove: bool,
    },
    ForgetHost {
        host: String,
    },
    Log {
        verify: bool,
        since: Option<String>,
        json: bool,
    },
    StackList,
    Rekey {
        stack: String,
        pairs: Vec<String>,
    },
    Handover {
        stack: String,
        to: String,
    },
    Unlock,
    StateShow {
        addr: Option<String>,
        from_log: bool,
    },
    Output {
        name: Option<String>,
        json: bool,
    },
    StateMv {
        from: String,
        to: String,
    },
    ProviderCheck {
        path: String,
    },
    ProviderSchema {
        provider: String,
    },
    Init {
        name: Option<String>,
    },
    Completions {
        shell: Shell,
    },
    Complete {
        words: Vec<String>,
    },
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
        _ => resolve(args).and_then(run_command),
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
    run_command(resolve(Args::try_parse_from(args)?)?).map(|_| ())
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
    let named: Vec<String> = order.iter().map(|d| d.name.clone()).collect();
    let target = named.last().cloned().unwrap_or_default();
    let deps = &named[..named.len() - 1];
    // The `stacks:` line (R-79): the deployments in apply order. Each is
    // planned, confirmed and applied in turn, so its ticks are its own
    // plan's, printed under its name.
    if cli.every_stack.is_empty() {
        println!(
            "stacks: {}, then {target} below, in apply order: {target} reads {}; each is \
             planned, confirmed and applied in turn",
            deps.join(", then "),
            if deps.len() == 1 {
                "its outputs"
            } else {
                "their outputs"
            }
        );
    } else {
        println!(
            "stacks: the project's {}, in apply order: {}; each is planned, confirmed and \
             applied in turn",
            named.len(),
            named.join(", then ")
        );
    }
    // A `--set` goes to each stack of the run that declares the input; one
    // none declares stays the target's, which names the error.
    let named_input = |kv: &String| {
        kv.split_once('=')
            .map_or(kv.as_str(), |(k, _)| k)
            .to_string()
    };
    let sets = |d: &Dependency| -> Vec<String> {
        cli.user_set
            .iter()
            .filter(|kv| d.inputs.contains(&named_input(kv)))
            .cloned()
            .collect()
    };
    // The project has no target to name the error: a `--set` no stack
    // declares is one now.
    if !cli.every_stack.is_empty()
        && let Some(kv) = cli
            .user_set
            .iter()
            .find(|kv| !order.iter().any(|d| d.inputs.contains(&named_input(kv))))
    {
        bail!(
            "--set {kv}: no stack of the project declares input {}",
            named_input(kv)
        );
    }
    for d in &order[..order.len() - 1] {
        println!(
            "{}",
            cli.style
                .paint(report::Paint::Bold, &format!("== {}", d.name))
        );
        let mut dep = cli.clone();
        dep.user_set = sets(d);
        dep.set = dep.user_set.clone();
        dep.set
            .extend(d.keys.iter().map(|(k, v)| format!("{k}={v}")));
        dep.keys = d.keys.clone();
        dep.input_files = Vec::new();
        dep.files = vec![d.file.clone()];
        match run(dep, None)? {
            Outcome::Done => {}
            o => return Ok(o),
        }
    }
    println!(
        "{}",
        cli.style
            .paint(report::Paint::Bold, &format!("== {target}"))
    );
    if let Some(last) = order.last().filter(|_| !cli.every_stack.is_empty()) {
        // The project's last stack: run as a dependency is, by its file.
        let user_set = sets(last);
        let mut cli = cli;
        cli.files = vec![last.file.clone()];
        cli.keys = last.keys.clone();
        cli.set = user_set.clone();
        cli.set
            .extend(last.keys.iter().map(|(k, v)| format!("{k}={v}")));
        cli.user_set = user_set;
        return run(cli, None);
    }
    let mut cli = cli;
    let own = order.last().map(|d| &d.inputs);
    cli.user_set.retain(|kv| {
        let k = named_input(kv);
        own.is_some_and(|i| i.contains(&k)) || !order.iter().any(|d| d.inputs.contains(&k))
    });
    cli.set = cli.user_set.clone();
    cli.set
        .extend(cli.keys.iter().map(|(k, v)| format!("{k}={v}")));
    run(cli, None)
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
fn run(cli: Cli, hook: Option<&mut controller::Hook>) -> Result<Outcome> {
    let mut session = None;
    // What resumes an interrupted run: the command that was interrupted.
    let verb = match cli.cmd {
        Cmd::Apply { destroy: true, .. } => "destroy",
        _ => "apply",
    };
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

fn run_with(
    mut cli: Cli,
    mut hook: Option<&mut controller::Hook>,
    session: &mut Option<Session>,
) -> Result<Outcome> {
    // A plan file's run is checked once its inputs (its world) are read.
    let planned = matches!(
        cli.cmd,
        Cmd::Apply {
            plan_file: Some(_),
            ..
        }
    );
    if !cli.in_project && !planned && needs_project(&cli) {
        return Err(crate::project::not_in_a_project(Path::new(".")));
    }
    if let Cmd::Controller { .. } | Cmd::Handover { .. } = cli.cmd {
        eprintln!("{EXPERIMENTAL}");
    }
    if let Cmd::Controller { .. } = cli.cmd {
        return run_controller(cli);
    }
    match &cli.cmd {
        Cmd::Handover { stack, to } => {
            let (from, home, times) = place_of(&cli, stack)?;
            let opener = open_s3(&cli.root, true);
            seal_before_handover(&cli, stack, &from, to, &opener, times)?;
            let moved = crate::stack::handover(&cli.root, stack, &from, &home, to, &opener, times)?;
            crate::audit::Log::new(moved.open(&opener)?, cli.audit_sink.clone()).append(
                "handover",
                serde_json::json!({ "stack": stack, "to": to, "who": crate::audit::who() }),
            )?;
            println!("stack {stack} handed over to {to}: {moved}");
            return Ok(Outcome::Done);
        }
        Cmd::StackList => return stack_list(&cli).map(|()| Outcome::Done),
        Cmd::Init { name } => {
            for line in crate::project::init(Path::new("."), name.as_deref())? {
                println!("{line}");
            }
            return Ok(Outcome::Done);
        }
        Cmd::Completions { shell } => {
            print!("{}", completion_script(*shell));
            return Ok(Outcome::Done);
        }
        Cmd::Complete { words } => return complete(words).map(|()| Outcome::Done),
        Cmd::ProviderCheck { path } => {
            let (lines, failed) = plugin::check::run(launch(), path)?;
            for l in &lines {
                println!("{l}");
            }
            if failed > 0 {
                bail!(
                    "provider {path}: {failed} of {} checks deviate",
                    lines.len()
                );
            }
            println!("provider {path}: conforms");
            return Ok(Outcome::Done);
        }
        Cmd::ProviderSchema { provider } => {
            let backend = Providers::start(
                launch(),
                std::slice::from_ref(provider),
                &plugin::Config::default(),
            )?;
            for a in &backend.schema().facts {
                println!("{}", partition::fmt_atom(a));
            }
            return Ok(Outcome::Done);
        }
        Cmd::Fmt { paths, check } => {
            let paths = if paths.is_empty() {
                let project =
                    crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
                crate::project::df_files(&project)
            } else {
                paths.clone()
            };
            return fmt_files(&paths, *check).map(|()| Outcome::Done);
        }
        Cmd::Doc => return doc(&cli.files).map(|()| Outcome::Done),
        _ => {}
    }
    let plan_file = match &cli.cmd {
        Cmd::Apply { plan_file, .. } => plan_file.clone(),
        _ => None,
    };
    let saved = match plan_file {
        Some(p) => Some((p.clone(), with_plan_inputs(&mut cli, &p)?)),
        None => None,
    };
    if !cli.in_project && needs_project(&cli) {
        return Err(crate::project::not_in_a_project(Path::new(".")));
    }

    let files = cli.files.clone();
    if files.is_empty() {
        bail!("internal: a run with no program");
    }
    // The program, as `deployment::load` reads it: the input files'
    // facts added; `stack` statements and providers'
    // `use`s over the manifest's defaults, `--provider` over the
    // latter.
    let target = deployment::Target {
        files: files.clone(),
        input_files: cli.input_files.clone(),
        providers: cli.providers.clone(),
    };
    let loaded = crate::timing::time(
        || "loaded and compiled".into(),
        || {
            deployment::load(
                &target,
                env!("CARGO_PKG_VERSION"),
                &|p: &Path| std::fs::read_to_string(p),
                &mut Watch::new(hook.as_deref_mut(), &cli.cmd),
            )
        },
    )?;
    cli.manifest = loaded.manifest.clone();
    // A key's value is the target's, else its input's default.
    check_keys(&cli, &loaded.cfg, &loaded.stack)?;
    // `stack rekey`: the run is of the old deployment (its state, its
    // world), the provenance of its names is listed, and its state moves.
    let rekey = match cli.cmd.clone() {
        Cmd::Rekey { stack, pairs } => {
            Some(rekey_args(&mut cli, &loaded.cfg, &files, &stack, &pairs)?)
        }
        _ => None,
    };
    let providers = loaded.providers.clone();
    // `destroy` and `plan --destroy` (R-149): the plan against an empty
    // wanted set.
    let destroying = matches!(
        cli.cmd,
        Cmd::Plan { destroy: true, .. } | Cmd::Apply { destroy: true, .. }
    );
    let verb = match destroying {
        true => "destroy",
        false => "apply",
    };
    // What evaluates against providers starts the program's, and there is
    // no default.
    if matches!(
        cli.cmd,
        Cmd::Eval
            | Cmd::Plan { .. }
            | Cmd::Test
            | Cmd::Apply { .. }
            | Cmd::Query { .. }
            | Cmd::Why { .. }
            | Cmd::Show { .. }
            | Cmd::Controller { .. }
            | Cmd::SecretsList { .. }
            | Cmd::SecretsRotate { .. }
            | Cmd::SecretsCycle
            | Cmd::SecretsSet { .. }
    ) {
        loaded.require_provider()?;
    }
    // Strata, effects and tests read only the named providers' schemas:
    // none when the program names only built-in ones.
    let none = loaded.starts_none();
    let load_schema = |providers: &[String]| match none {
        true => Ok(schema::Schema::default()),
        false => load_schema(providers),
    };
    if let Cmd::Test = cli.cmd {
        return run_tests(
            &loaded.program,
            &loaded.stack,
            &providers,
            none,
            &cli,
            &files,
        )
        .map(|()| Outcome::Done);
    }
    if let Cmd::Strata = cli.cmd {
        return print_strata(
            &files,
            &loaded.program,
            &load_schema(&providers)?,
            &cli.table,
        )
        .map(|()| Outcome::Done);
    }
    if let Cmd::Effects { json } = &cli.cmd {
        return print_effects(
            &loaded.program,
            &load_schema(&providers)?,
            *json,
            &cli.table,
        )
        .map(|()| Outcome::Done);
    }
    if let Cmd::Graph { what: Some(w) } = &cli.cmd
        && w == "strata"
    {
        let graph = partition::build(&loaded.program, &load_schema(&providers)?)?;
        return match partition::stratify(&graph) {
            partition::Verdict::Stratified { strata } => {
                print!("{}", graph::strata(&graph, Some(&strata)));
                Ok(Outcome::Done)
            }
            partition::Verdict::Rejected {
                scc,
                negative_edges,
            } => {
                print!("{}", graph::strata(&graph, None));
                bail!("{}", partition::cycle_error(&graph, &scc, &negative_edges))
            }
        };
    }

    let root = cli.root.clone();
    let set = cli
        .set
        .iter()
        .map(|kv| split_kv(kv).map(|(k, v)| (k.to_string(), v)))
        .collect::<Result<Vec<_>>>()?;
    // A run that writes the deployment's objects checks first that a
    // bucket keeps the conditions of a write; a plan only reads.
    let writes = hook.is_some()
        || matches!(
            cli.cmd,
            Cmd::Apply { .. }
                | Cmd::Plan { out: Some(_), .. }
                | Cmd::StateMv { .. }
                | Cmd::Unlock
                | Cmd::Rekey { .. }
        );
    // The deployment this run is of: the stack, or one value of its key
    // (rekey's old one), and where its objects are. A command that only
    // reads or moves them needs its key, not the program's other inputs.
    let locating = crate::timing::span(|| "located the deployment".into());
    let located = loaded.locate(
        &deployment::Selection {
            root: root.clone(),
            set,
            instance: rekey.as_ref().map(|r| r.from.clone()),
            world: cli.world.clone(),
            inventory: cli.inventory.clone(),
            objects_only: matches!(
                cli.cmd,
                Cmd::StateShow { .. }
                    | Cmd::Output { .. }
                    | Cmd::StateMv { .. }
                    | Cmd::Log { .. }
                    | Cmd::Unlock
            ),
        },
        &open_s3(&root, writes),
        &mut Watch::new(hook.as_deref_mut(), &cli.cmd),
    )?;
    drop(locating);
    let deployment = located.deployment.clone();
    // A keyed stack's plan and apply say first which deployment they are
    // of; `-v` adds which of its key values are defaults.
    let text_plan = matches!(cli.cmd, Cmd::Plan { json: false, .. });
    if !located.instance.key.is_empty()
        && hook.is_none()
        && (text_plan || matches!(cli.cmd, Cmd::Apply { .. }))
    {
        let verbose = matches!(&cli.cmd,
            Cmd::Plan { why, .. } | Cmd::Apply { why, .. } if *why >= report::Why::How);
        cli.held.print(&match verbose {
            true => format!("deployment: {}\n", located.instance.describe()),
            false => format!("deployment: {}\n", located.instance.name()),
        });
    }
    let dep = located.dep.clone();
    // The deployment's audit log, beside its state.
    let audit = dep.audit(
        cli.audit_sink
            .clone()
            .or(located.loaded.cfg.audit_sink.clone()),
        located
            .loaded
            .manifest
            .as_ref()
            .map_or(crate::audit::SINK_TIMEOUT, |m| m.audit_sink_timeout()),
        located
            .loaded
            .manifest
            .as_ref()
            .is_some_and(|m| m.audit_sink_all()),
    );
    match &cli.cmd {
        Cmd::Log {
            verify,
            since,
            json,
        } => {
            return print_log(&audit, &deployment, *verify, since.as_deref(), *json)
                .map(|()| Outcome::Done);
        }
        Cmd::Unlock => {
            println!("{}", dep.unlock()?);
            return Ok(Outcome::Done);
        }
        Cmd::StateShow { addr, from_log } => {
            return state_show(&dep, addr.as_deref(), *from_log, &cli.table)
                .map(|()| Outcome::Done);
        }
        Cmd::Output { name, json } => {
            return print_output(
                &dep,
                &located.loaded.program,
                name.as_deref(),
                *json,
                &cli.table,
            )
            .map(|()| Outcome::Done);
        }
        Cmd::ForgetHost { host } => return forget_host(&dep, host, &audit).map(|()| Outcome::Done),
        Cmd::StateMv { from, to } => {
            return state_mv(&dep, from, to, &audit).map(|()| Outcome::Done);
        }
        _ => {}
    }
    // The deployment's master (R-163): its key file read, one made only
    // for a deployment with no state, by a run that writes (a plan file,
    // an apply) or derives (`random.*`); a plan that writes a file, and an
    // apply, digest secrets with it (the plan file's, the audit log's).
    let writes = matches!(
        (&saved, &cli.cmd),
        (Some(_), _) | (None, Cmd::Plan { out: Some(_), .. }) | (None, Cmd::Apply { .. })
    );
    let derives = located
        .loaded
        .lowered
        .as_ref()
        .is_some_and(|l| crate::functions::random::called(&l.program));
    let new_master = matches!(
        cli.cmd,
        Cmd::Plan {
            new_master: true,
            ..
        } | Cmd::Apply {
            new_master: true,
            ..
        }
    );
    // Who holds it: dform.toml's `[secrets]` (R-164).
    let mixing =
        crate::custody::Mixing::of(located.loaded.manifest.as_ref(), &located.instance.stack)?;
    // What only reads the deployment's secrets: a query, a why, `secrets`.
    let reads = matches!(
        cli.cmd,
        Cmd::Query { .. }
            | Cmd::Why { .. }
            | Cmd::SecretsList { .. }
            | Cmd::SecretsRotate { .. }
            | Cmd::SecretsCycle
            | Cmd::SecretsSet { .. }
    );
    let master = match &cli.cmd {
        Cmd::Plan { .. }
        | Cmd::Apply { .. }
        | Cmd::Query { .. }
        | Cmd::Why { .. }
        | Cmd::SecretsList { .. }
        | Cmd::SecretsRotate { .. }
        | Cmd::SecretsCycle
        | Cmd::SecretsSet { .. } => {
            // A given secret sealed to the master's own key makes one
            // (R-108).
            let gives = matches!(cli.cmd, Cmd::SecretsSet { remove: false, .. })
                && crate::custody::given::to_master(&mixing);
            // The key may be made now: a bucket is checked first, as for
            // any run that writes.
            if let (true, None, store::Location::S3(spec)) = (
                writes || derives || gives || new_master,
                &cli.world,
                &located.location,
            ) {
                open_s3(&root, true)(spec)?;
            }
            let mut m = dep.master(
                &mixing,
                crate::custody::Want {
                    make: writes || derives || gives,
                    new_master,
                },
            )?;
            // A query, a why or `secrets` only reads: it says what this
            // master derives, whatever state was applied with.
            m.accept |= reads;
            m
        }
        _ => crate::custody::Master::none(),
    };
    if new_master && let Some(id) = &master.id {
        eprintln!(
            "new master (--new-master): random.* derive from {} (id {}); every value derived \
             from another master changes",
            master.source,
            crate::report::short_id(id)
        );
    }
    // dform.toml's `[secrets]` says otherwise than `state.master`: the next
    // apply that holds the master seals it again (After R-164).
    if let (Some(r), Some(_), Cmd::Plan { .. } | Cmd::Apply { .. }) =
        (&master.reseal, &master.key, &cli.cmd)
        && (!r.added.is_empty() || !r.removed.is_empty() || r.passphrase.is_some())
    {
        let when = match cli.cmd {
            Cmd::Apply { .. } => "this apply",
            _ => "the next apply",
        };
        let place = match &located.location {
            store::Location::S3(_) => "in the bucket",
            store::Location::Local(_) => "beside its state",
        };
        // Once per run, however often the deployment is opened in it.
        static SAID: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
        if SAID
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(deployment.to_string())
        {
            eprintln!(
                "{deployment}: {}{}  (dform.toml [secrets])",
                r.describe(&mixing, when, place),
                match r.removed.is_empty() {
                    true => "",
                    false => {
                        "; sealing to a recipient no longer revokes what it opened before: \
                         `dform secrets cycle` makes a master it never held, and each secret \
                         moves to it as it is rotated"
                    }
                }
            );
        }
    }
    // A run that does not hold the master (R-164) plans in full, each
    // change that needs it marked, and an apply makes what needs it not.
    if let Some(why) = &master.without {
        eprintln!(
            "{deployment}: planned without its master ({why}): a secret it derives is a \
             stand-in, proven unchanged or marked `needs the key`"
        );
    }
    // The key the run's digests are keyed with: a plan that writes a
    // file, and an apply. A plan file is checked in its own form: one
    // written without the master by digests anyone can compute (R-164).
    // After a cycle it is the first epoch's master, carried (R-165).
    let key = match writes {
        true => master.digest.clone(),
        false => None,
    };
    if let (Some((path, f)), None) = (&saved, &key)
        && !f.unkeyed
    {
        bail!(
            "plan file {}: its digests are keyed with {deployment}'s master, which this run does \
             not hold ({}): apply it with the passphrase, or plan again without it",
            path.display(),
            master.without.as_deref().unwrap_or("no master")
        );
    }
    let file_key: Option<&zset::file::Key> = match &saved {
        Some((_, f)) if f.unkeyed => None,
        _ => key.as_ref(),
    };
    let secret_inputs: BTreeSet<String> = located
        .loaded
        .declared
        .iter()
        .filter(|d| d.scope.is_empty())
        .filter(|d| matches!(&d.decl.ty, crate::ast::TypeExpr::Apply(n, _) if n == "secret"))
        .map(|d| d.decl.name.clone())
        .collect();
    // A secret input's value inline in argv (R-108): said, not refused, as
    // CI passes a masked variable so.
    let secret_named = secret_inputs_of(&located.loaded.declared);
    for (k, v) in cli.user_set.iter().filter_map(|kv| kv.split_once('=')) {
        if !v.starts_with('@') && secret_named.iter().any(|(a, _)| a == k) {
            eprintln!(
                "warning: --set {k}: argv is readable by every user on this host through /proc \
                 and lands in shell history; use --set {k}=@FILE or `dform secrets set {} {k}`",
                written(&located.instance)
            );
        }
    }
    // Other stacks' published outputs, read once, as facts; a plan file
    // records their digests, and an apply of it refuses when one moved.
    let mut read_outputs = located.read_outputs(&open_s3(&root, false))?;
    // A secret output sealed to this deployment (R-166): opened with its
    // master, its value read as a secret input's is; the apply's log says
    // which.
    let (opened, unsealed) = open_sealed(&mut read_outputs, &deployment, &master);
    // A reader the producer does not seal to yet is one from its apply:
    // registered, its master's public key published (made above), so the
    // producer's next apply seals to it.
    if let (Cmd::Apply { .. }, false, None) = (&cli.cmd, unsealed.is_empty(), &cli.world) {
        crate::stack::register(
            &root,
            &deployment,
            &located.location,
            located.loaded.cfg.bootstrap,
        )?;
    }
    let outputs_read: Vec<zset::file::OutputsDigest> = read_outputs
        .iter()
        .map(|r| zset::file::OutputsDigest {
            deployment: r.name.clone(),
            digest: r.digest.clone(),
        })
        .collect();
    let plan_inputs = |key: Option<&zset::file::Key>| -> Result<zset::file::Inputs> {
        Ok(zset::file::Inputs {
            stack_outputs: outputs_read.clone(),
            ..plan_inputs(&cli, &files, &secret_inputs, key)?
        })
    };
    // What the plan file records, when one is written or read.
    let inputs = match writes {
        true => Some(plan_inputs(file_key)?),
        false => None,
    };
    if let (Some((path, saved)), Some(inputs)) = (&saved, &inputs) {
        // The environment variables the plan read, as they are now.
        let labels = saved
            .inputs
            .env
            .iter()
            .filter_map(|e| e.get("sensitive")?.as_str().map(str::to_string));
        let now = zset::file::Inputs {
            env: env_inputs(labels, file_key),
            ..inputs.clone()
        };
        let diff = saved.input_differences(&now);
        if !diff.is_empty() {
            eprintln!(
                "plan file {} is stale: its inputs are not this run's:",
                path.display()
            );
            for d in &diff {
                eprintln!("- {d}");
            }
            bail!("stale plan: run plan again");
        }
    }
    if let Some(h) = hook.as_deref_mut() {
        if located.loaded.cfg.bootstrap {
            bail!(
                "stack {deployment} is role = bootstrap: it stays batch, and the controller never runs it"
            );
        }
        h.audit = Some(audit.clone());
        h.open(
            &dep,
            &located.paths.world,
            root.parent().unwrap_or(Path::new("")),
        )?;
    } else if let (Cmd::Apply { .. }, Some((to, _))) = (&cli.cmd, &located.handed) {
        bail!(
            "stack {deployment} was handed over to {to}: the controller runs it \
             (`dform controller run {deployment}`), not a batch apply"
        );
    }
    let chaos = match &cli.cmd {
        Cmd::Apply { chaos, .. } => Chaos::parse(chaos)?,
        _ => Chaos::default(),
    };
    // Chaos `stop-after`: the executor's, counted across the run's ticks.
    let stop_after = chaos.stop_after.map(std::cell::Cell::new);
    // What providers and trust roots fetch is cached in the state root's
    // cache/, a world fixture's beside it.
    let cache = match &cli.world {
        Some(w) => w.parent().unwrap_or(Path::new("")).to_path_buf(),
        None => root.join("cache"),
    };
    // query and why read the policy pass, so a deny over the plan can be
    // asked for and explained; a plan prints what it would do, conflicts
    // included (E §2.8: a conflict is a fact, not an abort), and then
    // refuses. Any other run is blocked by a violation.
    let explains = matches!(
        cli.cmd,
        Cmd::Query { .. }
            | Cmd::Why { .. }
            | Cmd::Diff { .. }
            | Cmd::Explain { .. }
            | Cmd::SecretsList { .. }
            | Cmd::SecretsRotate { .. }
            | Cmd::SecretsCycle
            | Cmd::SecretsSet { .. }
    );
    // The audit log as the run began, read once: the guardrail and why
    // since the last apply both read it.
    let entries_read: std::cell::OnceCell<Option<Vec<serde_json::Value>>> =
        std::cell::OnceCell::new();
    let entries = || entries_read.get_or_init(|| audit.entries().ok()).as_ref();
    // What the last apply derived (R-80): the plan's guardrail compares
    // against it, and its policy pass reads it as `derived_at_last_apply`.
    let last_derived = match &cli.cmd {
        Cmd::Plan { .. } | Cmd::Apply { .. } | Cmd::Why { .. } | Cmd::Query { .. } => {
            entries().and_then(|es| zset::Derived::last(es))
        }
        _ => None,
    };
    let opts = deployment::Options {
        launch: launch(),
        data: build_extra_facts(&cli.data)?,
        chaos: match &cli.cmd {
            Cmd::Apply { chaos, .. } => chaos.clone(),
            _ => Vec::new(),
        },
        cache: cli.world.is_none().then(|| cache.clone()),
        // A plan that makes no key digests with the one there is.
        digest_key: master
            .digest
            .as_ref()
            .map(|k| k.derive("provider digest").to_hex()),
        master: master.clone(),
        recorded: saved
            .as_ref()
            .map(|(_, f)| f.externs.clone())
            .unwrap_or_default(),
        // What a run plans, its providers declare.
        check_types: matches!(cli.cmd, Cmd::Plan { .. } | Cmd::Apply { .. }) || hook.is_some(),
        discover_all: explains,
        whole_schema: match &cli.cmd {
            Cmd::Query { pattern, .. } | Cmd::Why { pattern, .. } => reads_schema(pattern),
            _ => false,
        },
        collisions: matches!(
            cli.cmd,
            Cmd::Plan { .. } | Cmd::Apply { .. } | Cmd::Query { .. } | Cmd::Why { .. }
        ),
        // `secrets set` gives what a violation may say is missing.
        blocking: !matches!(
            cli.cmd,
            Cmd::Plan { .. }
                | Cmd::Query { .. }
                | Cmd::Why { .. }
                | Cmd::Rekey { .. }
                | Cmd::SecretsSet { .. }
        ),
        policy: (explains
            && !matches!(
                cli.cmd,
                Cmd::SecretsList { .. }
                    | Cmd::SecretsRotate { .. }
                    | Cmd::SecretsCycle
                    | Cmd::SecretsSet { .. }
            ))
            || matches!(cli.cmd, Cmd::Plan { .. }),
        last_apply: last_derived
            .as_ref()
            .map(zset::Derived::facts)
            .unwrap_or_default(),
        destroy: destroying,
    };
    // A key never rotated is as old as its master (R-161, `secrets/4`).
    crate::functions::random::set_born(born(entries()));
    let mut ev = located.evaluate(
        read_outputs,
        &opts,
        &mut Watch::new(hook.as_deref_mut(), &cli.cmd),
    )?;
    let blocked = |violations: &[String], redact: &query::Redactor| -> Result<()> {
        if violations.is_empty() {
            return Ok(());
        }
        // A conflict as the plan's `conflicts` section says it (R-111),
        // not its raw context.
        let (conflicts, rest): (Vec<&String>, Vec<&String>) =
            violations.iter().partition(|v| report::is_conflict(v));
        if !conflicts.is_empty() {
            eprintln!("conflicts");
            for v in &conflicts {
                let shown =
                    report::violation_conflict(v, redact, report::Why::Line, report::Style::PLAIN);
                eprint!(
                    "{}",
                    shown.unwrap_or_else(|| format!("- {}\n", redact.text(v)))
                );
            }
        }
        if !rest.is_empty() {
            eprintln!("constraint violations:");
            for v in &rest {
                eprintln!("- {}", report::violation_line(v, redact));
            }
        }
        Err(match &cli.cmd {
            Cmd::Apply { .. } => Refused::apply(verb, conflicts.len(), rest.len(), None),
            _ => Refused::new("blocked by constraints", conflicts.len(), rest.len()),
        }
        .into())
    };
    // A destroy is refused by the denies over its plan, not by the
    // program's own: it wants none of the resources they are about.
    if opts.blocking && !destroying {
        blocked(&ev.violations, &ev.redact)?;
    }
    if explains {
        // When each change the plan holds runs (After R-156): `why` says
        // its tick, `later` only for what no tick of the plan makes.
        let schedule = match (&cli.cmd, &ev.policy) {
            (Cmd::Why { .. }, Some(Ok(p))) => {
                let mut r = report::report(&report::Input {
                    plan: &p.plan,
                    res: &p.res,
                    sections: &p.sections,
                    program: &ev.evaluator.program,
                    schema: ev.schema(),
                    stack: &ev.located.loaded.stack,
                    show_noop: false,
                    tick: 1,
                    moved: &[],
                    denies: &p.denies,
                    kept: &Default::default(),
                });
                // A group by the address its statement names, as the plan
                // prints it.
                r.explain(
                    report::Why::Line,
                    &p.res,
                    &query::Redactor::new(&p.res.facts, ev.schema()),
                );
                Some(r)
            }
            _ => None,
        };
        let x = ev.explained();
        match &cli.cmd {
            Cmd::Query { pattern, json } => print_query(
                pattern,
                &ev.located.loaded.program,
                &x.res.facts,
                &x.redact,
                *json,
                &cli.table,
            )?,
            Cmd::Why {
                pattern,
                tree,
                all,
                core,
                whole,
                json,
            } => {
                let waits = |t: &str| ev.evaluator.provider_wait(t);
                let when = |at: &str| schedule.as_ref().and_then(|r| r.when(at));
                let keys = ev
                    .located
                    .instance
                    .key
                    .iter()
                    .map(|(k, _)| k.clone())
                    .collect();
                let top = site_root(&cli.files);
                let cx = crate::why::Context {
                    res: &x.res,
                    schema: Some(ev.schema()),
                    redact: &x.redact,
                    signatures: ev.located.loaded.lowered.as_ref().map(|l| &l.signatures),
                    stack_keys: &keys,
                    top: top.as_deref(),
                    waits: &waits,
                    when: schedule
                        .is_some()
                        .then_some(&when as &dyn Fn(&str) -> Option<String>),
                };
                let how = crate::why::As {
                    tree: *tree || *all,
                    all: *all,
                    core: *core,
                    whole: *whole,
                };
                if *json {
                    let j = crate::why::why_json(pattern, how, &cx)?;
                    println!("{}", serde_json::to_string_pretty(&j)?);
                } else {
                    print!("{}", crate::why::why(pattern, how, &cx)?);
                }
            }
            Cmd::Explain { addresses } => {
                let addresses = addresses
                    .iter()
                    .map(|a| ir::parse_resource_address(a))
                    .collect::<Result<Vec<_>>>()?;
                let s = crate::diff::snapshot(&x.res, &x.redact, &addresses);
                println!("{}", serde_json::to_string(&s)?);
            }
            Cmd::Diff { since, json, why } => {
                let keys: Vec<String> = ev
                    .located
                    .loaded
                    .cfg
                    .keys
                    .iter()
                    .map(|(k, _)| k.clone())
                    .collect();
                let rerun = rerun_of(&cli, &ev.located.instance.key, keys)?;
                let d = crate::diff::diff(&audit.entries()?, since, &cli.files, &rerun, &|a| {
                    crate::diff::snapshot(&x.res, &x.redact, a)
                })?;
                if *json {
                    let mut j = d.json();
                    j["deployment"] = serde_json::json!(deployment);
                    println!("{}", serde_json::to_string_pretty(&j)?);
                } else {
                    print!("{}", d.text(*why));
                }
            }
            Cmd::SecretsList { .. }
            | Cmd::SecretsRotate { .. }
            | Cmd::SecretsCycle
            | Cmd::SecretsSet { .. } => {
                let born = born(entries());
                let mut list = crate::secrets::inventory::of(
                    &x.res.facts,
                    &x.redact,
                    &ev.st,
                    ev.evaluator.backend.schema(),
                    born.as_deref(),
                    master.epoch,
                );
                let dep = &ev.located.dep;
                // A given secret from its file (R-108): where it lives, its
                // generation and when it was set.
                let files = crate::custody::given::reads();
                given_rows(&mut list, &files, &mixing);
                match &cli.cmd {
                    Cmd::SecretsList { json } => {
                        secrets_list(&deployment, &list, master.epoch, *json, &cli.table)?;
                        if !*json {
                            for r in &files {
                                print_given_file(r, &mixing);
                            }
                            let log = entries().cloned().unwrap_or_default();
                            for h in crate::custody::holders(dep.store().as_ref(), &mixing, &log)? {
                                print_holders(&h);
                            }
                        }
                    }
                    Cmd::SecretsSet { name, remove } => secrets_set(
                        &deployment,
                        &secret_inputs_of(&ev.located.loaded.declared),
                        name,
                        *remove,
                        &files,
                        (&mixing, &master),
                        &audit,
                    )?,
                    Cmd::SecretsRotate { key } => {
                        let sealed = files.iter().find(|r| {
                            r.file
                                .as_ref()
                                .is_some_and(|f| f.leaves.iter().any(|l| l.name() == *key))
                        });
                        if let Some(r) = sealed {
                            bail!(
                                "secrets rotate {key}: {key} is given in {deployment}, sealed in \
                                 {}: give it its new value with `dform secrets set {} {key}`, \
                                 then plan",
                                r.shown,
                                written(&ev.located.instance)
                            );
                        }
                        secrets_rotate(dep, &list, &ev.st.memo, key, &audit)?
                    }
                    _ => secrets_cycle(dep, &list, &master, &mixing, &audit)?,
                }
            }
            _ => unreachable!("explains is query, why, diff, __explain or secrets"),
        }
        return Ok(Outcome::Done);
    }
    let deployment::Evaluation {
        located,
        mut st,
        moves,
        res,
        violations,
        redact,
        compiled,
        mut policy,
        evaluator,
        ..
    } = ev;
    let deployment::Compiled {
        resources,
        adopts,
        lifecycle,
    } = compiled?;
    let backend = &*evaluator.backend;
    let externs = &evaluator.externs;
    let program = &evaluator.program;
    let schema = backend.schema();
    let stack = &located.loaded.stack;
    let stack_cfg = &located.loaded.cfg;
    let instance = &located.instance;
    let location = &located.location;
    let times = located.times;
    let evaluate_with =
        |st: &state::State,
         withheld: &BTreeSet<ir::Address>,
         more: &[Atom],
         tick: Option<usize>| { evaluator.evaluate_with(st, withheld, more, tick) };
    let evaluate = |st: &state::State| evaluator.evaluate(st);
    let plan_for = |res: engine::EvalResult,
                    violations: &[String],
                    resources: Vec<ir::Resource>,
                    adopts: &[ir::Adopt],
                    lifecycle: &zset::Lifecycle,
                    st: &state::State| {
        evaluator.plan(res, violations, resources, adopts, lifecycle, st)
    };
    // The copies state remembers as the run starts: a removed copy's
    // deletes print under it (R-67).
    let kept = st.instances.clone();
    // An apply that resumes one interrupted: its first tick is what
    // remained (R-122).
    let resuming = std::cell::Cell::new(false);
    // Said once a run: what a run without the master cannot compare.
    let drift_said = std::cell::Cell::new(false);
    let report_of = |plan: &crate::provider::Plan,
                     res: &engine::EvalResult,
                     sections: &stuck::Sections,
                     tick: usize,
                     moved: &[(ir::Address, ir::Address)],
                     denies: &[String]| {
        let mut report = report::report(&report::Input {
            plan,
            res,
            sections,
            program,
            schema,
            stack,
            show_noop: cli.show_noop,
            tick,
            moved,
            denies,
            kept: &kept,
        });
        report.resumed = resuming.get() && tick == 1;
        // A destroy's deletes say no reason; a plan's say why they are
        // gone (`Report::explain`, After R-149 amendments 4 and 5).
        report.removing = destroying;
        custody_marks(&mut report, plan, backend);
        if !drift_said.replace(true) {
            drift_unknown(&deployment, plan, backend);
        }
        report
    };
    // How much each printed change says of why (R-79).
    let why = match &cli.cmd {
        Cmd::Plan { why, .. } | Cmd::Apply { why, .. } => *why,
        _ => report::Why::None,
    };
    // Each change's leaf that changed since the last apply: the last
    // apply's program evaluated again (`diff`'s reading), at the commit it
    // recorded or with the inputs it recorded. Only this executable can
    // evaluate it (not a test linking dform in).
    let because = |report: &mut report::Report,
                   plan: &crate::provider::Plan,
                   res: &engine::EvalResult| {
        let dform = std::env::current_exe()
            .ok()
            .is_some_and(|e| e.file_stem().is_some_and(|s| s == "dform"));
        if why == report::Why::None || !dform || cli.files.is_empty() {
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
        let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
        let (Some(entries), Ok(rerun)) = (entries(), rerun_of(&cli, &instance.key, keys)) else {
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
        let redact = query::Redactor::new(&res.facts, schema);
        report.because(&then, &crate::diff::snapshot(res, &redact, &addresses));
    };
    let top = site_root(&cli.files);
    // What the plan empties since the last apply (R-80), less what this
    // apply's `--allow-empty` and the stack's `allow_empty` name.
    let mut allow_empty = match &cli.cmd {
        Cmd::Apply { allow_empty, .. } => allow_empty.clone(),
        _ => Vec::new(),
    };
    if let Some(t) = located
        .loaded
        .manifest
        .as_ref()
        .and_then(|m| m.stacks.get(stack))
    {
        allow_empty.extend(t.allow_empty.iter().cloned());
    }
    let emptied = |plan: &crate::provider::Plan,
                   res: &engine::EvalResult,
                   because: &dyn Fn(&str) -> Option<String>|
     -> Vec<zset::Emptied> {
        // A destroy empties everything; its question says so.
        let Some(then) = last_derived.as_ref().filter(|_| !destroying) else {
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
            .filter(|e| !e.allowed(&allow_empty))
            .collect()
    };
    let explain = |report: &mut report::Report,
                   plan: &crate::provider::Plan,
                   res: &engine::EvalResult,
                   tick: usize| {
        report.keys = located
            .instance
            .key
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        report.explain(why, res, &query::Redactor::new(&res.facts, schema));
        if tick == 1 {
            because(report, plan, res);
            let leaf: std::collections::BTreeMap<String, String> = report
                .definite
                .iter()
                .filter_map(|d| Some((d.addr.to_string(), d.because.clone()?)))
                .collect();
            report.warnings = emptied(plan, res, &|a| leaf.get(a).cloned());
        }
        if let Some(top) = &top {
            report.relative_to(top);
        }
    };
    // `--why=none` is the bare diff, laid out as before R-79.
    let rendered = |report: &report::Report| match why {
        report::Why::None => report.render_bare(cli.style),
        _ => report.render(cli.style),
    };
    let show = |plan: &crate::provider::Plan,
                res: &engine::EvalResult,
                sections: &stuck::Sections,
                tick: usize,
                moved: &[(ir::Address, ir::Address)],
                denies: &[String]| {
        let mut report = report_of(plan, res, sections, tick, moved, denies);
        // Printed: the lines the default level folds away say no site.
        report.every_site = false;
        explain(&mut report, plan, res, tick);
        print!("{}", rendered(&report))
    };
    // `apply PLAN`: the delta re-evaluated at tick 1 must be the file's;
    // a later tick's is compared at its boundary, which stops before a
    // tick that differs (`zset::file::tick_differences`).
    let check_saved = |plan: &crate::provider::Plan,
                       res: &engine::EvalResult,
                       sections: &stuck::Sections|
     -> Result<()> {
        let Some((path, saved)) = &saved else {
            return Ok(());
        };
        let report = report_of(plan, res, sections, 1, &[], &[]);
        let redact = query::Redactor::new(&res.facts, schema);
        let key = file_key;
        let now = zset::file::delta(plan, sections, &report, schema, &redact, key);
        let mut diff = saved.stale(&now);
        // A secret answer the plan read (a location's), read again: by its
        // version when its source keeps versions (R-172), else by its
        // digest. One that is "not yet" now is waited on, not compared.
        let now: std::collections::BTreeMap<String, serde_json::Value> =
            answer_inputs(externs, key)
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
    };

    // The plan file of a plan: its delta at tick 1, the inputs, the pinned
    // commits, the extern answers and the `requires_approval` rows.
    let plan_file_of = |plan: &crate::provider::Plan,
                        res: &engine::EvalResult,
                        sections: &stuck::Sections,
                        resources: &[ir::Resource],
                        st: &state::State,
                        key: Option<&zset::file::Key>,
                        inputs: zset::file::Inputs|
     -> Result<zset::file::PlanFile> {
        let report = report_of(plan, res, sections, 1, &[], &[]);
        let redact = query::Redactor::new(&res.facts, schema);
        let mut deformations = zset::file::delta(plan, sections, &report, schema, &redact, key);
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
        let mut unresolved: std::collections::BTreeSet<String> = deformations
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
            stack: stack.clone(),
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
    };

    match cli.cmd.clone() {
        Cmd::Eval => {
            println!("facts: {}", res.facts.len());
            println!("resources: {}", resources.len());
            for r in &resources {
                println!("- {}", r.addr);
            }
        }
        Cmd::Query { .. } | Cmd::Why { .. } | Cmd::Diff { .. } | Cmd::Explain { .. } => {
            unreachable!("explained before")
        }
        Cmd::Show { addr } => {
            let addr = ir::parse_resource_address(&addr)?;
            let Some(r) = resources.iter().find(|r| r.addr == addr) else {
                bail!("no resource {} in this deployment", report::address(&addr));
            };
            let json = serde_json::to_string_pretty(&redact.json(&r.attrs))?;
            println!("{}", json);
        }
        Cmd::Strata | Cmd::Test | Cmd::Effects { .. } => unreachable!("handled before evaluation"),
        Cmd::Rekey { .. } => {
            let Some(r) = rekey else {
                unreachable!("rekey_args ran for rekey");
            };
            let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
            let named = crate::lint::key_named(&res, backend.schema(), &keys);
            if named.is_empty() {
                println!(
                    "no name-like attribute depends on the key ({})",
                    keys.join(", ")
                );
            } else {
                println!(
                    "these name-like attributes depend on the key ({}); the next plan of {} \
                     renames them, usually a replace:",
                    keys.join(", "),
                    r.to.name()
                );
                for n in &named {
                    println!("  {n}");
                }
            }
            let place = |i: &crate::stack::Instance| {
                let location = deployment::deployment_location(
                    &root,
                    stack,
                    stack_cfg.backend.as_ref(),
                    i.segment().as_deref(),
                );
                crate::stack::Place {
                    world: crate::stack::world_file(&location, &i.dir(&root.join(stack))),
                    location,
                }
            };
            let opener = open_s3(&root, true);
            let moved = crate::stack::rekey(
                &root,
                (&r.from, &place(&r.from)),
                (&r.to, &place(&r.to)),
                &opener,
                times,
            )?;
            crate::audit::Log::new(
                moved.open(&opener)?,
                cli.audit_sink.clone().or(stack_cfg.audit_sink.clone()),
            )
            .append(
                "rekey",
                serde_json::json!({
                    "from": r.from.name(),
                    "to": r.to.name(),
                    "who": crate::audit::who(),
                }),
            )?;
            println!(
                "stack {} rekeyed to {}: {moved}",
                r.from.name(),
                r.to.name(),
            );
        }
        Cmd::Fmt { .. }
        | Cmd::Doc
        | Cmd::Controller { .. }
        | Cmd::Log { .. }
        | Cmd::StackList
        | Cmd::Handover { .. }
        | Cmd::Unlock
        | Cmd::StateShow { .. }
        | Cmd::Output { .. }
        | Cmd::StateMv { .. }
        | Cmd::ProviderCheck { .. }
        | Cmd::ProviderSchema { .. }
        | Cmd::Init { .. }
        | Cmd::Completions { .. }
        | Cmd::Complete { .. }
        | Cmd::SecretsList { .. }
        | Cmd::SecretsRotate { .. }
        | Cmd::SecretsCycle
        | Cmd::SecretsSet { .. }
        | Cmd::ForgetHost { .. } => {
            unreachable!("handled before evaluation")
        }
        Cmd::Graph { what: None } => print!("{}", graph::resources(&resources)),
        Cmd::Graph { what: Some(spec) } => {
            let redact = query::Redactor::new(&res.facts, backend.schema());
            print!("{}", graph::relation(&spec, &res.facts, &redact)?);
        }
        Cmd::Plan { out, json, .. } => {
            let Planned {
                res,
                resources,
                plan,
                sections,
                denies,
                unreachable,
            } = policy
                .take()
                .ok_or_else(|| anyhow::anyhow!("internal: a plan without its policy pass"))??;
            // Of a destroy, only the denies over its plan refuse it.
            let violations = match destroying {
                true => Vec::new(),
                false => violations,
            };
            let mut report = report_of(&plan, &res, &sections, 1, &moves, &denies);
            report.every_site = json;
            explain(&mut report, &plan, &res, 1);
            // The plan file, when one is written or the plan needs an
            // approval: its digest is what an approver signs.
            let needs = crate::approval::needs(&res.facts);
            let file = if out.is_some() || !needs.is_empty() {
                let loaded;
                let key = match (writes, &key) {
                    (true, k) => k.as_ref(),
                    (false, _) => {
                        // The key may be made now: a bucket is checked
                        // first, as for any run that writes.
                        if let (None, store::Location::S3(spec)) = (&cli.world, &location) {
                            open_s3(&root, true)(spec)?;
                        }
                        loaded = dep
                            .master(
                                &mixing,
                                crate::custody::Want {
                                    make: true,
                                    new_master,
                                },
                            )?
                            .key;
                        loaded.as_ref()
                    }
                };
                let inputs = match &inputs {
                    Some(i) => i.clone(),
                    None => plan_inputs(key)?,
                };
                let mut f = plan_file_of(&plan, &res, &sections, &resources, &st, key, inputs)?;
                f.digest = Some(f.digest());
                Some(f)
            } else {
                None
            };
            if json {
                let mut j = report.json();
                j["deployment"] = serde_json::json!(deployment);
                // How the plan ends, as its exit status says (R-147).
                j["outcome"] = match violations.is_empty() && denies.is_empty() {
                    true => Outcome::Done.word(),
                    false => "refused",
                }
                .into();
                j["key_defaults"] = serde_json::json!(instance.defaulted);
                if !unreachable.is_empty() {
                    j["unreachable"] = unreachable
                        .iter()
                        .map(|(a, why)| serde_json::json!({ "address": a.to_string(), "reason": why }))
                        .collect();
                }
                if let Some(f) = &file {
                    j["needs_approval"] = serde_json::to_value(&f.needs_approval)?;
                    j["digest"] = serde_json::to_value(&f.digest)?;
                }
                // The names declared more than once (R-104), as the plan
                // file lists them.
                let guarded = zset::file::guarded(&res);
                if !guarded.is_empty() {
                    j["guarded"] = serde_json::to_value(&guarded)?;
                }
                println!("{}", serde_json::to_string_pretty(&j)?);
            } else {
                cli.held.summary(report.summary());
                cli.held.print(&rendered(&report));
                cli.held.print(&unreachable_text(&unreachable));
                // Who each secret output no provider holds is sealed to:
                // the grant (R-166).
                let unheld = crate::stack::unheld_secret_outputs(
                    &res.facts,
                    &crate::stack::secret_output_types(program),
                );
                if !unheld.is_empty() && cli.world.is_none() {
                    let readers = readers_of(&root, &deployment, &open_s3(&root, false))?;
                    cli.held.print(&grants_text(&unheld, &readers));
                }
                // The digest to approve, when a change is held for an
                // approval (the bare diff lists those changes after it);
                // a plan file's digest is on stderr beside its path.
                if let Some(f) = file.as_ref().filter(|f| !f.needs_approval.is_empty()) {
                    if why == report::Why::None {
                        cli.held.print(&needs_text(&f.needs_approval));
                    }
                    cli.held.print(&format!(
                        "plan digest: {}\n",
                        f.digest.as_deref().unwrap_or_default()
                    ));
                }
            }
            // The report listed the conflicts and the denies over the plan
            // (R-111): stderr names only what it did not, once. An up to
            // date plan lists none (a secret's age, R-161, denies one).
            let mut unshown: Vec<String> = violations
                .iter()
                .filter(|v| !report::is_conflict(v))
                .cloned()
                .collect();
            if report.undeformed && !json {
                unshown.extend(denies.iter().cloned());
            }
            blocked(&unshown, &redact)?;
            if !violations.is_empty() || !denies.is_empty() {
                let conflicts = violations.iter().filter(|v| report::is_conflict(v)).count();
                return Err(Refused::new(
                    "blocked by constraints",
                    conflicts,
                    violations.len() - conflicts + denies.len(),
                )
                .into());
            }
            if let (Some(out), Some(file)) = (out, &file) {
                file.save(&out)?;
                eprintln!(
                    "plan file: {} (plan digest: {})",
                    out.display(),
                    file.digest.as_deref().unwrap_or_default()
                );
                audit.append(
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
        }
        Cmd::Apply {
            max_ticks,
            parallel,
            approval,
            yes,
            ..
        } => {
            // How long a tick waits on an open null (R-122): the `wait` of
            // the provider that answers it, as dform.toml sets it (a
            // built-in extern's by its `[providers.NAME]` table), else
            // 10m; not its calls' `timeout`.
            let waits = located
                .loaded
                .manifest
                .as_ref()
                .map(|m| m.provider_waits())
                .unwrap_or_default();
            let wait_budget = |on: &[String]| {
                on.iter()
                    .map(|l| {
                        let set = || {
                            let (t, _) = crate::value::null_owner(l)?;
                            let serving = waits.iter().find(|(n, _)| backend.serves(n, &t));
                            match serving {
                                Some((_, d)) => Some(*d),
                                None => waits.get(t.split_once('.')?.0).copied(),
                            }
                        };
                        set().unwrap_or(crate::project::WAIT)
                    })
                    .max()
                    .unwrap_or_default()
            };
            for addr in chaos.addresses() {
                if !resources.iter().any(|r| &r.addr == addr) && st.get(addr).is_none() {
                    bail!(
                        "--chaos: {} is not a resource of this stack",
                        report::address(addr)
                    );
                }
            }
            // One apply at a time per deployment; the lock is held until the
            // apply's end is in the audit log.
            *session = Some(Session {
                log: audit.clone(),
                lock: dep.lock()?,
            });
            // Under the lease again: a memo the last run kept in memory.
            if let Some(h) = hook.as_deref_mut() {
                h.flush()?;
            }
            // Without the master (R-164) the apply's digests are its plan
            // file's form: derivation digests, labels.
            let key = file_key;
            let inputs = inputs
                .clone()
                .ok_or_else(|| anyhow::anyhow!("internal: no plan inputs"))?;
            // Approvals (docs/reference.md, "Approvals"): a token verifies against the
            // stack's trust root (loaded once), for this plan's digest and
            // this deployment, by an approver `approver_allowed` admits
            // when the program restricts them.
            let restricts = crate::approval::restricts_approvers(program);
            let roots: std::cell::OnceCell<Vec<_>> = std::cell::OnceCell::new();
            let verify_token = |token: &str,
                                needs: &[(String, String)],
                                digest: &str,
                                facts: &BTreeSet<Atom>|
             -> Result<crate::approval::Verified> {
                if stack_cfg.approvals.is_empty() {
                    bail!(
                        "stack {deployment} has no approvals trust root \
                         (dform.toml: `[stacks.{stack}] approvals = 'jwks(\"https://...\")'`)"
                    );
                }
                let roots = match roots.get() {
                    Some(r) => r,
                    None => {
                        let r = crate::approval::load_roots(&stack_cfg.approvals, &cache)?;
                        roots.get_or_init(|| r)
                    }
                };
                let expect = crate::approval::Expect {
                    stack: &instance.stack,
                    key: &instance.key,
                    digest,
                    now: crate::approval::now(),
                };
                let allowed = |w: &str, d: &str| crate::approval::approver_allowed(facts, w, d);
                let allowed: Option<executor::Allowed> =
                    if restricts { Some(&allowed) } else { None };
                executor::approve(token, needs, roots, &expect, allowed)
            };
            let mut approved: Option<crate::approval::Verified> = None;
            keep_memos(&mut st, externs, &master)?;
            evaluator.files.keep(&mut st);
            // A checkpoint of the state (`wal`): before a tick's first
            // call and after its last, and where the apply stops. Between,
            // each call's change of state goes to the log alone, before
            // the next call (`record`).
            let persist = |st: &state::State| dep.save_state(st);
            let record = |st: &state::State| dep.record(st);
            // Nothing is written, to state or the world, until the apply is
            // confirmed: the moves, the resolution of uncertain calls and the
            // in-flight record taken here are written with the tick's first.
            if !moves.is_empty() {
                print_moves(&moves);
            }
            let resumed = st.in_flight.take();
            resuming.set(resumed.is_some());
            if let Some(f) = &resumed {
                // The remaining deformations come back as facts with the
                // documents they were planned against, as the held ones do
                // at a boundary: the evaluator derives the deny when the
                // world moved under one (`zset::POLICY_RULES`).
                let remaining = executor::remaining(f);
                // As the record keeps it: a sensitive leaf by its digest.
                let observed = backend.stored_world(&backend.observe(&st)?);
                let changed = executor::changed_under(backend, &remaining, &observed);
                if !changed.is_empty() {
                    eprint!(
                        "the world changed under a remaining action:\n{}",
                        executor::format_changes(&changed)
                    );
                }
                let facts = zset::deformation_facts(
                    remaining.keys().map(|a| ("remaining", a)),
                    &remaining,
                    &observed,
                );
                let (after, denies) = evaluate_with(&st, &BTreeSet::new(), &facts, Some(f.tick))?;
                // The program's own violations did not refuse it (a
                // destroy's): only what the remaining actions add does.
                let denies: Vec<String> = denies
                    .into_iter()
                    .filter(|d| !violations.contains(d))
                    .collect();
                if !denies.is_empty() {
                    let redact = query::Redactor::new(&after.facts, backend.schema());
                    eprintln!("constraint violations:");
                    for d in &denies {
                        eprintln!("- {}", report::violation_line(d, &redact));
                    }
                    persist(&st)?;
                    return Err(Refused::apply(
                        verb,
                        0,
                        denies.len(),
                        Some(format!(
                            "on the remaining actions of the interrupted {verb}: review \
                             `dform plan`, then {verb} again"
                        )),
                    )
                    .into());
                }
            }
            // Ticks (E §2.7): each applies every definite deformation in
            // dependency order; what waits on a null is held. At the
            // boundary the results come back as world facts, round 0
            // resolves them, everything is re-derived and policy is checked
            // again before the next tick.
            let (mut res, mut violations, mut resources, mut adopts, mut lifecycle) =
                (res, violations, resources, adopts, lifecycle);
            let mut tick = 1;
            // Every address a tick's plan has listed so far, and the last
            // one's pending groups: what it could not name.
            let mut listed: BTreeSet<ir::Address> = BTreeSet::new();
            let mut unnamed: Vec<String> = Vec::new();
            // A plan file or an approval applies only the ticks whose
            // addresses the plan it approves named (R-30); `--yes` answers
            // every question (R-122).
            let shown = saved.is_some() || approval.is_some();
            // What the last tick's plan held under `later` waiting on a
            // provider's settings (R-110): listed, but planned only once
            // the provider is configured, so asked for again (R-45).
            let mut on_provider: BTreeSet<ir::Address> = BTreeSet::new();
            // What the first tick's plan scheduled in a later tick, its
            // attributes as written (R-156): shown, so not asked again.
            let mut scheduled: BTreeSet<String> = BTreeSet::new();
            // The delta of the plan this apply showed, or of the plan file
            // it applies: a later tick's re-plan is compared with it, and
            // what earlier ticks ran (After R-156).
            let mut shown_delta: Vec<zset::file::Entry> = Vec::new();
            let mut ran: BTreeSet<(String, String)> = BTreeSet::new();
            let delta_of = |plan: &crate::provider::Plan,
                            res: &engine::EvalResult,
                            sections: &stuck::Sections,
                            tick: usize| {
                let report = report_of(plan, res, sections, tick, &[], &[]);
                let redact = query::Redactor::new(&res.facts, schema);
                zset::file::delta(plan, sections, &report, schema, &redact, file_key)
            };
            // The providers the plan's own evaluation configured.
            evaluator.take_configured();
            loop {
                // Between ticks, a signal stops the apply here.
                crate::interrupt::check()?;
                let Planned {
                    res: r,
                    resources: docs,
                    mut plan,
                    sections,
                    denies,
                    unreachable,
                } = plan_for(res, &violations, resources, &adopts, &lifecycle, &st)?;
                (res, resources) = (r, docs);
                if tick == 1 {
                    check_saved(&plan, &res, &sections)?;
                }
                // A provider the last tick made the settings of known (a
                // kubeconfig read from the server it created) was
                // configured at the boundary: said, a secret setting as
                // `(sensitive)`, and logged by its keys, never a value.
                for name in evaluator.take_configured() {
                    let (keys, shown): (Vec<String>, Vec<String>) = evaluator
                        .settings_shown(&name, &res.facts, why >= report::Why::How)
                        .into_iter()
                        .unzip();
                    if why != report::Why::None {
                        println!(
                            "provider {name}: configured after tick {}: {}",
                            tick - 1,
                            shown.join(", ")
                        );
                    }
                    audit.append(
                        "configure",
                        serde_json::json!({ "tick": tick - 1, "provider": name, "settings": keys }),
                    )?;
                }
                // What the policy pass says needs an approval, and the
                // digest of this plan: the file's, else of the plan as a
                // file would record it.
                let mut needs = crate::approval::needs(&res.facts);
                if let (1, Some((_, f))) = (tick, &saved) {
                    needs.extend(
                        f.needs_approval
                            .iter()
                            .map(|n| (n.deformation.clone(), n.reason.clone())),
                    );
                    needs.sort();
                    needs.dedup();
                }
                let digest = match &saved {
                    _ if tick > 1 && (hook.is_none() || needs.is_empty()) => None,
                    Some((path, f)) => {
                        let d = f.digest();
                        if f.digest.as_ref().is_some_and(|x| *x != d) {
                            bail!(
                                "plan file {}: its digest {} is not its content's ({d}): \
                                 it was edited after the plan",
                                path.display(),
                                f.digest.as_deref().unwrap_or_default()
                            );
                        }
                        Some(d)
                    }
                    None => Some(
                        plan_file_of(&plan, &res, &sections, &resources, &st, key, inputs.clone())?
                            .digest(),
                    ),
                };
                if tick == 1 {
                    audit.append(
                        "plan",
                        serde_json::json!({
                            "digest": digest,
                            "documents": crate::diff::documents(&evaluator.tables.sources()),
                            "file": saved.as_ref().map(|(p, _)| p.display().to_string()),
                            "inputs": inputs,
                            "needs_approval": needs
                                .iter()
                                .map(|(d, r)| serde_json::json!({ "deformation": d, "reason": r }))
                                .collect::<Vec<_>>(),
                            "who": crate::audit::who(),
                        }),
                    )?;
                }
                // Controller mode: the report is one log line, and the
                // policy pass gates what this tick may apply; a deformation
                // that needs an approval is held until a token for the
                // plan's digest arrives.
                let mut undeformed = false;
                if let Some(h) = hook.as_deref_mut() {
                    let text = report_of(&plan, &res, &sections, tick, &[], &denies).text();
                    undeformed = text
                        .lines()
                        .next()
                        .is_some_and(|l| l.ends_with(" is up to date"));
                    h.gate(tick, &mut plan, &res.facts, &text);
                    if let (false, Some(digest)) = (needs.is_empty(), &digest) {
                        let mut tokens: Vec<String> = res
                            .facts
                            .iter()
                            .filter(|a| a.pred == "approval")
                            .filter_map(|a| match a.args.as_slice() {
                                [Term::Val(Value::Str(t))] => Some(t.clone()),
                                _ => None,
                            })
                            .collect();
                        tokens.extend(h.dropped_tokens());
                        let (mut ok, mut refused) = (None, Vec::new());
                        for t in &tokens {
                            match verify_token(t, &needs, digest, &res.facts) {
                                Ok(v) => {
                                    ok = Some(v);
                                    break;
                                }
                                Err(e) if e.is::<crate::approval::OtherDigest>() => {}
                                Err(e) => refused.push(e.to_string()),
                            }
                        }
                        h.approvals(tick, &mut plan, &needs, digest, ok.as_ref(), &refused)?;
                    }
                }
                let held: Vec<String> = plan
                    .actions
                    .iter()
                    .filter_map(|a| waits_on(a, &sections))
                    .flatten()
                    .collect();
                // A create_before_destroy replacement deposes an object that
                // is deleted at the next tick, once what depends on it has
                // moved to the replacement.
                let mut boundary = !held.is_empty()
                    || !sections.pending_groups.is_empty()
                    || !sections.undetermined.is_empty()
                    || plan
                        .actions
                        .iter()
                        .any(|a| matches!(a.kind, ActionKind::Replace { create_first: true }));
                // The controller applies ticks until a plan is undeformed:
                // every tick that changes something is followed by another.
                if hook.is_some() {
                    boundary |= plan.actions.iter().any(|a| {
                        !matches!(a.kind, ActionKind::Noop) && waits_on(a, &sections).is_none()
                    });
                } else {
                    if why == report::Why::None && (tick > 1 || boundary) {
                        println!("tick {tick}:");
                    } else if why != report::Why::None && tick > 1 {
                        // A later tick that only waits has no section of
                        // its own in the report: its header says which
                        // tick the report is of.
                        let report = report_of(&plan, &res, &sections, tick, &[], &denies);
                        if !report.undeformed && report.changes() == 0 {
                            println!("tick {tick}  0 changes");
                        }
                    }
                    show(&plan, &res, &sections, tick, &[], &denies);
                    if tick == 1 {
                        print!("{}", unreachable_text(&unreachable));
                    }
                }
                if !denies.is_empty() {
                    let redact = query::Redactor::new(&res.facts, backend.schema());
                    eprintln!("constraint violations:");
                    for d in &denies {
                        eprintln!("- {}", report::violation_line(d, &redact));
                    }
                    let at = (tick > 1).then(|| {
                        format!(
                            "stopped at tick {tick}; ticks 1 to {} were applied",
                            tick - 1
                        )
                    });
                    return Err(Refused::apply(verb, 0, denies.len(), at).into());
                }
                // A batch apply asks before it changes anything, unless
                // `--yes` or it applies a reviewed plan file.
                // What it carries over from an interrupted apply is marked.
                if tick == 1 && hook.is_none() {
                    print!("{}", executor::carried_over(resumed.as_ref(), &st, &plan));
                }
                if tick == 1 && hook.is_none() && !yes && saved.is_none() {
                    let report = report_of(&plan, &res, &sections, tick, &[], &denies);
                    if !report.undeformed {
                        let n = report.changes();
                        still_held(session)?;
                        if !confirm(n, false, destroying, &deployment, tick, cli.style)? {
                            return Ok(declined(&deployment, tick));
                        }
                    }
                }
                // What the plan empties since the last apply is asked for
                // on its own, also under `--yes` or of a plan file, unless
                // `--allow-empty` names it (R-80).
                if tick == 1 && hook.is_none() {
                    for e in emptied(&plan, &res, &|_| None) {
                        still_held(session)?;
                        if !confirm_emptied(&e, &deployment, cli.style)? {
                            return Ok(declined(&deployment, tick));
                        }
                    }
                }
                // A later tick whose plan holds an address no earlier one
                // listed (a pending group's member, named only now) asks
                // again, its plan printed above, counting the new ones; with
                // a plan file too, whose groups bound them (`check_saved`).
                let addresses: BTreeSet<&ir::Address> = plan
                    .actions
                    .iter()
                    .filter(|a| !matches!(a.kind, ActionKind::Noop))
                    .map(|a| &a.addr)
                    .collect();
                // Of a plan file or an approval, it stops before such a tick
                // instead, the state consistent: the next apply plans them as
                // its tick 1. `--yes` applies it.
                if tick > 1 && hook.is_none() {
                    let new = addresses.iter().filter(|a| !listed.contains(**a)).count();
                    if new > 0 && shown {
                        st.in_flight = None;
                        persist(&st)?;
                        return Ok(stopped(Stopped {
                            tick: tick - 1,
                            new,
                            unnamed,
                            on_provider: false,
                            differs: Vec::new(),
                        }));
                    }
                    // What `later` showed waiting on a provider, planned
                    // now against it: asked for as tick 1 was, unless
                    // `--yes`; a plan file or an approval did not see it.
                    let planned = addresses
                        .iter()
                        .filter(|a| {
                            on_provider.contains(**a) && !scheduled.contains(&a.to_string())
                        })
                        .count();
                    if planned > 0 && shown {
                        st.in_flight = None;
                        persist(&st)?;
                        return Ok(stopped(Stopped {
                            tick: tick - 1,
                            new: planned,
                            unnamed: Vec::new(),
                            on_provider: true,
                            differs: Vec::new(),
                        }));
                    }
                    // The tick as its boundary re-plans it, against the
                    // tick as the plan shown had it (After R-156): the
                    // same changes and values, a value the plan did not
                    // know whatever it became. One that differs is
                    // printed below the tick with what differs, and asked
                    // for again; a plan file or an approval stops before
                    // it, naming what differs.
                    let now = delta_of(&plan, &res, &sections, tick);
                    let differs =
                        zset::file::tick_differences(&shown_delta, &now, tick, &ran, file_key);
                    if !differs.is_empty() {
                        println!("tick {tick} differs from the plan shown:");
                        for d in &differs {
                            println!("{}", d.line());
                        }
                    }
                    // A change gone from the tick is said, not asked for:
                    // the tick does less than was shown.
                    let more = differs.iter().any(|d| d.mark != '-');
                    if more && shown {
                        st.in_flight = None;
                        persist(&st)?;
                        let mut names: Vec<String> = Vec::new();
                        for n in differs.iter().map(|d| d.name()) {
                            if !names.contains(&n) {
                                names.push(n);
                            }
                        }
                        return Ok(stopped(Stopped {
                            tick: tick - 1,
                            new: differs.len(),
                            unnamed: Vec::new(),
                            on_provider: false,
                            differs: names,
                        }));
                    }
                    let asked = new + planned > 0 || more;
                    if asked && !yes {
                        still_held(session)?;
                        if !confirm(
                            new + planned,
                            true,
                            destroying,
                            &deployment,
                            tick,
                            cli.style,
                        )? {
                            return Ok(declined(&deployment, tick));
                        }
                    }
                    // What was asked for (or `--yes` applied) is what the
                    // next boundary compares with.
                    if asked {
                        let key = |e: &zset::file::Entry| (e.typ.clone(), e.name.clone());
                        let mut by: std::collections::BTreeMap<_, _> =
                            shown_delta.drain(..).map(|e| (key(&e), e)).collect();
                        by.extend(now.into_iter().map(|e| (key(&e), e)));
                        shown_delta = by.into_values().collect();
                    }
                }
                if tick == 1 {
                    scheduled = report_of(&plan, &res, &sections, tick, &[], &denies)
                        .ticks
                        .into_iter()
                        .filter(|(t, _)| *t > 1)
                        .flat_map(|(_, xs)| xs)
                        .collect();
                    // What the later ticks are compared with at their
                    // boundaries: the plan file's delta, else this plan's.
                    shown_delta = match &saved {
                        Some((_, f)) => f.deformations.clone(),
                        None => delta_of(&plan, &res, &sections, tick),
                    };
                }
                listed.extend(addresses.into_iter().cloned());
                on_provider = resources
                    .iter()
                    .filter(|r| evaluator.provider_wait(&r.addr.typ).is_some())
                    .map(|r| r.addr.clone())
                    .collect();
                if hook.is_none() {
                    unnamed = report_of(&plan, &res, &sections, tick, &[], &denies)
                        .groups
                        .iter()
                        .map(|g| {
                            let on: Vec<String> =
                                g.on.iter().map(|n| report::attribute_label(n)).collect();
                            format!("{} on {}", report::address_text(&g.pattern), on.join(", "))
                        })
                        .collect();
                }
                if hook.is_none() {
                    approve_entry(
                        tick,
                        &needs,
                        digest.as_deref(),
                        approval.as_deref(),
                        match (saved.is_some(), destroying) {
                            (true, _) => Asked::File,
                            (false, true) => Asked::Destroy,
                            (false, false) => Asked::Apply,
                        },
                        &mut approved,
                        &audit,
                        &|t: &str, d: &str| verify_token(t, &needs, d, &res.facts),
                        &|w: &str, d: &str| {
                            !restricts || crate::approval::approver_allowed(&res.facts, w, d)
                        },
                    )?;
                }
                if tick == 1 {
                    let dir = files[0]
                        .parent()
                        .filter(|d| !d.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    let commit = crate::project::git_head(dir);
                    let mut e = serde_json::json!({
                        "who": crate::audit::who(),
                        "dform": env!("CARGO_PKG_VERSION"),
                        "commit": commit,
                        "providers": backend.names(),
                        "protocol": crate::plugin::backend::VERSION,
                    });
                    // A dirty tree: the commit is not the program; which
                    // tracked files it had modified.
                    let modified = match commit {
                        Some(_) => crate::project::git_modified(dir),
                        None => Vec::new(),
                    };
                    if !modified.is_empty() {
                        e["dirty"] = true.into();
                        e["modified"] = modified.into();
                    }
                    if destroying {
                        e["destroy"] = true.into();
                    }
                    audit.append("apply_start", e)?;
                    if !opened.is_empty() {
                        audit.append(
                            "opened",
                            serde_json::json!({
                                "outputs": opened,
                                "who": crate::audit::who(),
                            }),
                        )?;
                    }
                    // The master this apply derives with, as state records
                    // it (R-163): a new one is said in the log, and where
                    // it came from.
                    if let Some(id) = master
                        .id
                        .as_ref()
                        .filter(|id| st.master.as_ref() != Some(*id))
                    {
                        audit.append(
                            "master",
                            serde_json::json!({
                                "from": st.master,
                                "to": id,
                                "source": master.source,
                                "made": master.made,
                                "who": crate::audit::who(),
                            }),
                        )?;
                        st.master = Some(id.clone());
                    }
                    // A new master sealed to recipients: who could open it
                    // from the start (the offboarding list's first entry).
                    if master.made && !mixing.recipients.is_empty() {
                        audit.append(
                            "recipients",
                            serde_json::json!({
                                "added": recipients_json(&mixing.recipients),
                                "removed": [],
                                "id": master.id,
                                "who": crate::audit::who(),
                            }),
                        )?;
                    }
                    // A plain key file now sealed (R-164); the master sealed
                    // again to the recipients dform.toml names now.
                    if let Some(done) =
                        crate::custody::reseal(dep.store().as_ref(), &deployment, &master, &mixing)?
                    {
                        if done.key_file {
                            audit.append(
                                "custody",
                                serde_json::json!({
                                    "sealed": store::KEY,
                                    "into": store::MASTER,
                                    "id": master.id,
                                    "who": crate::audit::who(),
                                }),
                            )?;
                        }
                        if !done.added.is_empty()
                            || !done.removed.is_empty()
                            || done.passphrase.is_some()
                        {
                            audit.append(
                                "recipients",
                                serde_json::json!({
                                    "added": recipients_json(&done.added),
                                    "removed": recipients_json(&done.removed),
                                    "passphrase": done.passphrase.map(|p| match p {
                                        true => "added",
                                        false => "removed",
                                    }),
                                    "id": master.id,
                                    "epoch": master.epoch,
                                    "who": crate::audit::who(),
                                }),
                            )?;
                        }
                    }
                }
                // A run that does not hold the master (R-164) makes no
                // change that needs it, nor one that depends on one: they
                // wait for an apply with it, and this one stops after the
                // tick.
                let needing = needing_master(&plan, &resources, &st, backend);
                plan.actions.retain(|a| !needing.contains_key(&a.addr));
                ran.extend(
                    plan.actions
                        .iter()
                        .filter(|a| {
                            !matches!(a.kind, ActionKind::Noop) && waits_on(a, &sections).is_none()
                        })
                        .map(|a| (a.addr.typ.clone(), a.addr.name.clone())),
                );
                // Kept in state: a sensitive leaf by its digest.
                let observed = backend.stored_world(&backend.observe(&st)?);
                executor::begin(&mut st, tick, &plan, &observed);
                if let Some(f) = st.in_flight.as_mut() {
                    f.destroy = destroying;
                }
                executor::mark_creates(
                    &mut st,
                    &deployment,
                    plan.actions
                        .iter()
                        .filter(|a| waits_on(a, &sections).is_none()),
                );
                persist(&st)?;
                let pending: BTreeSet<ir::Address> = plan
                    .actions
                    .iter()
                    .filter(|a| waits_on(a, &sections).is_some())
                    .map(|a| a.addr.clone())
                    .collect();
                let mut seen: executor::Seen =
                    observed.into_iter().map(|(a, d)| (a, Some(d))).collect();
                plan.actions.retain(|a| waits_on(a, &sections).is_none());
                let changed = plan
                    .actions
                    .iter()
                    .any(|a| !matches!(a.kind, ActionKind::Noop));
                // Every apply is at least one tick of the fake world, also
                // when there is nothing to do. Each Apply call's change of
                // state is logged before the next call (`executor`, `wal`).
                if tick == 1 || changed {
                    // Each action goes to the audit log: its result, its
                    // remote id, and a digest of its redacted diff.
                    let redact = query::Redactor::new(&res.facts, schema);
                    let report = report_of(&plan, &res, &sections, tick, &[], &[]);
                    let diffs: std::collections::BTreeMap<ir::Address, String> =
                        zset::file::delta(&plan, &sections, &report, schema, &redact, key)
                            .into_iter()
                            .map(|e| {
                                let digest = crate::approval::digest_of(
                                    &serde_json::to_value(&e).unwrap_or_default(),
                                );
                                (
                                    ir::Address {
                                        typ: e.typ,
                                        name: e.name,
                                    },
                                    digest,
                                )
                            })
                            .collect();
                    let audit_failed = std::cell::RefCell::new(None);
                    // What a forget drops from state (R-154): its remote id,
                    // which the log keeps.
                    let forgotten: std::collections::BTreeMap<ir::Address, String> = plan
                        .actions
                        .iter()
                        .filter(|a| matches!(a.kind, ActionKind::Forget))
                        .filter_map(|a| Some((a.addr.clone(), st.get(&a.addr)?.remote.clone())))
                        .collect();
                    let on_action = |a: &crate::provider::Action,
                                     err: Option<&anyhow::Error>,
                                     st: &state::State| {
                        if let Some(remote) = forgotten.get(&a.addr) {
                            let e = serde_json::json!({
                                "tick": tick,
                                "address": a.addr.to_string(),
                                "remote": remote,
                                "why": "lifecycle retain",
                            });
                            if let Err(x) = audit.append("forgot", e) {
                                audit_failed.borrow_mut().get_or_insert(x);
                            }
                            return;
                        }
                        let mut e = serde_json::json!({
                            "tick": tick,
                            "action": zset::deformation_kind(&a.kind, false).unwrap_or("no-op"),
                            "address": a.addr.to_string(),
                            "result": if err.is_some() { "failed" } else { "ok" },
                            "remote": st.get(&a.addr).map(|e| e.remote.clone()),
                            "diff": diffs.get(&a.addr),
                        });
                        if let Some(err) = err {
                            crate::audit::error(&mut e, &redact.text(&crate::diag::shape(err)));
                        }
                        if let Err(x) = audit.append("action", e) {
                            audit_failed.borrow_mut().get_or_insert(x);
                        }
                    };
                    let fence = || dep.check_fence();
                    // The tick's block on stderr, filling in (R-127); a
                    // controller's log line is its report.
                    let mode = crate::progress::Mode::of_stderr(why == report::Why::None);
                    let progress = hook.is_none().then(|| {
                        let actions: Vec<&crate::provider::Action> = plan.actions.iter().collect();
                        let mut block = report::progress::Block::new(tick, &actions);
                        // A failure's site, where its change is derived (R-109).
                        block.sites =
                            report::sites(&res, actions.iter().map(|a| &a.addr), top.as_deref());
                        crate::progress::Progress::tick(
                            block,
                            mode,
                            match mode {
                                crate::progress::Mode::Terminal => cli.style,
                                _ => report::Style::default(),
                            },
                        )
                    });
                    let on_event = |e: executor::Event| {
                        if let Some(p) = &progress {
                            p.event(&e, &|t: &str| redact.text(t));
                        }
                    };
                    let opts = executor::Options {
                        parallel: parallel as usize,
                        persist: &record,
                        stop_after: stop_after.as_ref(),
                        on_action: Some(&on_action),
                        before_submit: Some(&fence),
                        on_event: Some(&on_event),
                    };
                    let applied = executor::run_tick(
                        backend, &resources, &adopts, &lifecycle, &mut st, &plan, &opts,
                    );
                    let failed = progress.map(|p| p.finish()).unwrap_or_default();
                    // A failure the block said in full below it is not
                    // said again: the run ends naming what failed (R-109).
                    let applied = match applied {
                        Err(e) if !failed.is_empty() && e.is::<report::Failure>() => {
                            Err(anyhow::anyhow!(
                                "apply {deployment}: tick {tick} failed: {}",
                                failed
                                    .iter()
                                    .map(report::address)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        }
                        applied => applied,
                    };
                    for note in backend.take_notes() {
                        println!("chaos: {note}");
                    }
                    if let Some(e) = audit_failed.into_inner() {
                        return Err(e);
                    }
                    // The tick's checkpoint, also of a tick that failed;
                    // not of one chaos `stop-after` stopped as if dform
                    // were killed: its calls' answers are in the log alone,
                    // and the next run replays them.
                    let killed = stop_after.as_ref().is_some_and(|left| left.get() == 0);
                    let checkpoint = match killed {
                        true => Ok(()),
                        false => persist(&st),
                    };
                    log_retries(&audit, &redact, backend, tick)?;
                    seen.extend(applied?);
                    checkpoint?;
                    // The world as the executor saw it, keyed like a
                    // secret: a document may hold one.
                    let world: serde_json::Map<String, serde_json::Value> = seen
                        .iter()
                        .map(|(a, d)| (state::key(a), d.clone().unwrap_or_default()))
                        .collect();
                    let canonical = crate::approval::canonical_json(&world.into());
                    // Without the master there is no digest of it (R-164).
                    audit.append(
                        "tick",
                        serde_json::json!({
                            "tick": tick,
                            "world": key.map(|k| format!("hmac-sha256:{}", k.digest(canonical.as_bytes()))),
                        }),
                    )?;
                }
                if !needing.is_empty() {
                    st.in_flight = None;
                    persist(&st)?;
                    return Ok(Outcome::Stopped {
                        tick,
                        why: needing_text(&deployment, &needing, master.without.as_deref()),
                    });
                }
                if !boundary && destroying {
                    st.in_flight = None;
                    // What no Delete could reach stays in state: the
                    // destroy stops there (exit 5); run again once their
                    // provider can be configured, or retain them.
                    let kept: BTreeSet<String> =
                        unreachable.iter().map(|(a, _)| state::key(a)).collect();
                    let left: Vec<&String> = st
                        .resources
                        .keys()
                        .chain(st.deposed.keys())
                        .filter(|k| !kept.contains(*k))
                        .collect();
                    if !left.is_empty() {
                        bail!(
                            "destroy {deployment}: state still holds {} after the last tick",
                            left.iter()
                                .map(|k| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !unreachable.is_empty() {
                        persist(&st)?;
                        let n = match unreachable.len() {
                            1 => "1 object".to_string(),
                            n => format!("{n} objects"),
                        };
                        return Ok(Outcome::Stopped {
                            tick,
                            why: format!(
                                "destroy {deployment}: stopped; {n} no Delete could reach \
                                 stay in state (listed under `unreachable`): destroy again \
                                 once their provider can be configured, or forget them with \
                                 `lifecycle(r, \"retain\")`"
                            ),
                        });
                    }
                    // The deployment is gone: its checkpoint is empty but
                    // for the count of idempotency keys given out, so a
                    // later apply never reuses one; the audit log keeps
                    // its history (R-146) and says it was destroyed,
                    // which `stack list` reads.
                    st = state::State {
                        version: st.version,
                        keys: st.keys,
                        ..Default::default()
                    };
                    persist(&st)?;
                    audit.append(
                        "destroyed",
                        serde_json::json!({ "deployment": deployment, "who": crate::audit::who() }),
                    )?;
                    // What other stacks read of it goes with it: a reader
                    // waits on it as on one never applied (R-121).
                    if cli.world.is_none() && dep.store().get(store::OUTPUTS)?.is_some() {
                        dep.store().delete(store::OUTPUTS)?;
                    }
                    // No closing line: the block showed every change
                    // finish, and the exit status says how it ended.
                    break;
                }
                if !boundary {
                    st.in_flight = None;
                    // The rotations this apply carried are made (R-161),
                    // and an earlier master epoch no secret derives from
                    // any more is retired (R-165).
                    for r in st.secrets.values_mut() {
                        r.pending = false;
                    }
                    let mut keep = st.epochs_in_use();
                    keep.insert(master.epoch.max(1));
                    for (epoch, id) in crate::custody::retire(dep.store().as_ref(), &keep)? {
                        audit.append(
                            "retired",
                            serde_json::json!({
                                "epoch": epoch,
                                "id": id,
                                "who": crate::audit::who(),
                            }),
                        )?;
                    }
                    // What this apply derived, for the next plan's
                    // guardrail and policy pass (R-80).
                    let relations: BTreeSet<String> = located
                        .loaded
                        .lowered
                        .as_ref()
                        .map(|l| l.signatures.keys().map(|(p, _)| p.clone()).collect())
                        .unwrap_or_default();
                    let redact = query::Redactor::new(&res.facts, schema);
                    let record = zset::Derived::of(&res, &redact, &relations, top.as_deref());
                    if record != zset::Derived::default() {
                        audit.append("derived", serde_json::json!({ "record": record }))?;
                    }
                    // The stack's outputs, as the world now is, for other
                    // stacks to read (evaluated again only when it has any:
                    // the evaluation refreshes). A --world fixture is not
                    // registered: everything stays beside the world file.
                    keep_memos(&mut st, externs, &master)?;
                    evaluator.files.keep(&mut st);
                    crate::tables::record(&mut st.externs, &externs.recorded());
                    // The copies state still holds resources of (R-67).
                    let held: Vec<ir::Address> = st
                        .resources
                        .keys()
                        .filter_map(|k| state::parse_key(k))
                        .collect();
                    st.instances = crate::zset::Instances::from_facts(&res.facts)
                        .with(&st.instances)
                        .kept(&held);
                    // A secret one by its label and digest, never its
                    // value (E DR-19); a ref resolved, as the world is.
                    let secret_types = crate::stack::secret_output_types(program);
                    // The project's deployments that read it: a secret
                    // output no provider holds is sealed to each (R-166).
                    let readers = match secret_types.is_empty() || cli.world.is_some() {
                        true => Vec::new(),
                        false => readers_of(&root, &deployment, &open_s3(&root, false))?,
                    };
                    let sealing = std::cell::RefCell::new(None);
                    // The same value to the same reader seals the same: the
                    // published outputs move only when a value or a reader
                    // does.
                    let seed = master
                        .key
                        .as_ref()
                        .map(|k| crate::secrets::derived(k, "dform sealed output seed"));
                    let seal = |path: &str, v: &serde_json::Value| {
                        let mut out = std::collections::BTreeMap::new();
                        let plain = crate::approval::canonical_json(v);
                        for (r, public) in &readers {
                            let Some(public) = public else { continue };
                            let label = sealed_label(&deployment, path, r);
                            match crate::custody::seal_to(
                                public,
                                &label,
                                plain.as_bytes(),
                                seed.as_ref(),
                            ) {
                                Ok(b) => {
                                    out.insert(r.clone(), b);
                                }
                                Err(e) => {
                                    sealing.borrow_mut().get_or_insert(e);
                                }
                            }
                        }
                        out
                    };
                    let outputs = if crate::stack::has_outputs(&res.facts) {
                        crate::stack::outputs(
                            &evaluate(&st)?.0.facts,
                            &secret_types,
                            &backend.observe(&st)?,
                            &st,
                            &deployment,
                            &|b| key.map(|k| k.digest(b)),
                            &seal,
                        )
                    } else {
                        Default::default()
                    };
                    if let Some(e) = sealing.into_inner() {
                        return Err(e);
                    }
                    // A grant that changed is said in the log: who may
                    // now open which output.
                    let grants = |o: &std::collections::BTreeMap<
                        String,
                        crate::stack::SecretOutput,
                    >| {
                        o.iter()
                            .filter(|(_, x)| !x.sealed.is_empty())
                            .map(|(k, x)| (k.clone(), x.sealed.keys().cloned().collect::<Vec<_>>()))
                            .collect::<std::collections::BTreeMap<_, _>>()
                    };
                    if grants(&outputs.secret) != grants(&st.secret_outputs) {
                        audit.append(
                            "sealed",
                            serde_json::json!({
                                "outputs": grants(&outputs.secret),
                                "who": crate::audit::who(),
                            }),
                        )?;
                    }
                    // A secret output this run cannot digest (R-164): not
                    // published; the apply stops, its changes made.
                    if !outputs.unproven.is_empty() {
                        persist(&st)?;
                        return Ok(Outcome::Stopped {
                            tick,
                            why: format!(
                                "apply {deployment}: stopped; the secret outputs {} changed, and \
                                 only the deployment's master ({}) can publish them: apply \
                                 again with it",
                                outputs.unproven.join(", "),
                                master.without.as_deref().unwrap_or("not held")
                            ),
                        });
                    }
                    st.outputs = outputs.known.clone();
                    st.secret_outputs = outputs.secret.clone();
                    persist(&st)?;
                    // Published beside the state, apart from it: what
                    // other stacks read.
                    if cli.world.is_none()
                        && (!outputs.is_empty()
                            || !secret_types.is_empty()
                            || dep.store().get(store::OUTPUTS)?.is_some())
                    {
                        let published = crate::stack::Published::new(&deployment, &outputs);
                        dep.publish(&published.bytes())?;
                    }
                    // Every deployment of a keyed stack is registered.
                    let keyed = instance.segment().is_some();
                    if (!outputs.is_empty()
                        || !secret_types.is_empty()
                        || stack_cfg.bootstrap
                        || keyed)
                        && cli.world.is_none()
                    {
                        crate::stack::register(&root, &deployment, location, stack_cfg.bootstrap)?;
                    }
                    // No closing line: the block showed every change
                    // finish, and the exit status says how it ended.
                    if let Some(h) = hook.as_deref_mut() {
                        h.finish(
                            &deployment,
                            undeformed,
                            &backend.stored_world(&backend.observe(&st)?),
                        )?;
                    }
                    break;
                }
                if !changed && let Some(h) = hook.as_deref_mut() {
                    // Everything definite is held: wait for the next event.
                    st.in_flight = None;
                    persist(&st)?;
                    h.finish(
                        &deployment,
                        false,
                        &backend.stored_world(&backend.observe(&st)?),
                    )?;
                    break;
                }
                if !changed {
                    let mut waits: Vec<String> = sections.blocking.iter().cloned().collect();
                    waits.extend(held);
                    waits.sort();
                    waits.dedup();
                    // What waiting can resolve (R-81): the world reaching a
                    // value (a Job's status), an extern's "not yet". The
                    // tick waits, up to its providers' timeout, for one to
                    // change (R-122).
                    let on = waiting_on(&sections, &st, externs);
                    if on.is_empty() {
                        // A null by its label; a provider's settings as
                        // `later` names them (R-110).
                        let waits: Vec<String> = waits
                            .iter()
                            .map(|n| match n.starts_with("provider ") {
                                true => n.clone(),
                                false => report::attribute_label(n),
                            })
                            .collect();
                        bail!(
                            "apply stopped at tick {tick}: nothing definite to apply, still waiting on {}",
                            waits.join(", ")
                        );
                    }
                    // Said as printed (R-111); the audit log keeps the labels.
                    let names: Vec<String> =
                        on.iter().map(|l| report::attribute_label(l)).collect();
                    let labels: Vec<String> = on.iter().map(|l| ir::label(l)).collect();
                    let budget = wait_budget(&on);
                    let mut w = crate::progress::Wait::new();
                    // On a terminal, the wait is one line counting up
                    // (R-127); elsewhere its lines say it every 10s.
                    let mode = crate::progress::Mode::of_stderr(why == report::Why::None);
                    let live = (mode == crate::progress::Mode::Terminal).then(|| {
                        crate::progress::Progress::wait(tick, names.clone(), mode, cli.style)
                    });
                    let resolved = loop {
                        if live.is_none() {
                            w.tick(&names);
                        }
                        if w.elapsed() >= budget {
                            break false;
                        }
                        if crate::interrupt::sleep(w.next_poll(budget)) {
                            break false;
                        }
                        backend.reread();
                        externs.forget_not_yet();
                        let (next, _) = evaluate_with(&st, &BTreeSet::new(), &[], Some(tick))?;
                        let docs =
                            ir::compile_resources(next.facts.iter().cloned(), backend.schema())?;
                        let now = deployment::sections(&next, &docs, backend.schema());
                        if waiting_on(&now, &st, externs) != on {
                            break true;
                        }
                    };
                    if let Some(l) = live {
                        l.done();
                    }
                    let redact = query::Redactor::new(&res.facts, schema);
                    log_retries(&audit, &redact, backend, tick)?;
                    let result = match (resolved, crate::interrupt::requested()) {
                        (true, _) => "resolved",
                        (false, Some(_)) => "interrupted",
                        (false, None) => "expired",
                    };
                    audit.append(
                        "wait",
                        crate::audit::wait(tick, &labels, w.since(), w.elapsed(), result),
                    )?;
                    if !resolved {
                        // Nothing of the tick was applied: the next apply
                        // plans it again, as an unattended stop does.
                        st.in_flight = None;
                        evaluator.files.keep(&mut st);
                        persist(&st)?;
                        crate::interrupt::check()?;
                        bail!(
                            "apply stopped at tick {tick}: waited {} on {}, still unknown \
                             (the provider's `wait` in dform.toml); state is consistent: \
                             run apply again to wait again",
                            crate::plugin::policy::show(budget),
                            names.join(", ")
                        );
                    }
                }
                if tick == max_ticks {
                    bail!(
                        "apply stopped after {max_ticks} ticks (--max-ticks): the stack still has changes"
                    );
                }
                // The boundary. The held deformations come back as facts
                // with the documents they were planned against: the
                // evaluator derives the deny when the world moved under one.
                let held = executor::check_boundary(backend, &seen, &pending, &st, tick)?;
                let (next, next_violations) =
                    evaluate_with(&st, &BTreeSet::new(), &held, Some(tick))?;
                violations = next_violations;
                let redact = query::Redactor::new(&next.facts, backend.schema());
                for w in &next.warnings {
                    eprintln!("warning: {}", redact.text(w));
                }
                // A destroy is refused by what its held changes derive, not
                // by the program's own violations.
                let refusing: Vec<&String> = match destroying {
                    false => violations.iter().collect(),
                    true => {
                        let (_, own) = evaluate(&st)?;
                        violations.iter().filter(|v| !own.contains(v)).collect()
                    }
                };
                if !refusing.is_empty() {
                    eprintln!("constraint violations after tick {tick}:");
                    for v in &refusing {
                        eprintln!("- {}", report::violation_line(v, &redact));
                    }
                    let conflicts = refusing.iter().filter(|v| report::is_conflict(v)).count();
                    return Err(Refused::apply(
                        verb,
                        conflicts,
                        refusing.len() - conflicts,
                        Some(format!(
                            "stopped after tick {tick}; ticks 1 to {tick} were applied"
                        )),
                    )
                    .into());
                }
                resources = ir::compile_resources(next.facts.iter().cloned(), backend.schema())?;
                adopts = ir::compile_adopts(next.facts.iter())?;
                lifecycle = zset::Lifecycle::from_facts(&next.facts, backend.schema())?;
                res = next;
                tick += 1;
            }
        }
    }

    Ok(Outcome::Done)
}

/// What the command line does at an evaluation's steps
/// (`deployment::Observer`): it prints what the evaluation says, and
/// controller mode's hook stamps the inputs and tables and adds the drift.
struct Watch<'a> {
    hook: Option<&'a mut controller::Hook>,
    cmd: &'a Cmd,
}

impl<'a> Watch<'a> {
    fn new(hook: Option<&'a mut controller::Hook>, cmd: &'a Cmd) -> Watch<'a> {
        Watch { hook, cmd }
    }

    fn plans(&self) -> bool {
        matches!(self.cmd, Cmd::Plan { .. } | Cmd::Apply { .. })
    }
}

impl deployment::Observer for Watch<'_> {
    fn note(&mut self, note: deployment::Note) {
        use deployment::Note;
        match note {
            Note::Warning(w) | Note::Policy(w) => eprintln!("warning: {w}"),
            Note::Collision(w) if self.plans() => eprintln!("warning: {w}"),
            Note::Collision(_) => {}
            Note::Resolved(line) => eprintln!("resolved: {line}"),
            Note::TableMoved(m) if self.plans() => match self.hook {
                Some(_) => controller::log(format_args!("{m}")),
                None => println!("{m}"),
            },
            Note::TableMoved(_) => {}
            Note::Computed(_, n) if self.plans() => eprintln!("note: {n}"),
            Note::Computed(..) => {}
        }
    }

    fn relations(&mut self, relations: &[watch::Relation]) {
        if let Some(h) = self.hook.as_deref_mut() {
            h.inputs(relations);
        }
    }

    fn tables(&mut self, read: &[(watch::Relation, String)]) {
        if let Some(h) = self.hook.as_deref_mut() {
            h.tables(read);
        }
    }

    fn facts(&mut self, backend: &Providers, st: &state::State) -> Result<Vec<Atom>> {
        Ok(match self.hook.as_deref_mut() {
            Some(h) => h.drift_facts(&backend.stored_world(&backend.observe(st)?)),
            None => Vec::new(),
        })
    }
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

/// Outside a project only what writes no state runs: a plan (without a
/// plan file), a query, a `dev` view, or a run whose state is a world
/// fixture's (`dev --world`, beside the world file).
fn needs_project(cli: &Cli) -> bool {
    match &cli.cmd {
        Cmd::Apply { .. } | Cmd::Controller { .. } | Cmd::Plan { out: Some(_), .. } => {
            cli.world.is_none()
        }
        Cmd::Log { .. }
        | Cmd::StackList
        | Cmd::Rekey { .. }
        | Cmd::Handover { .. }
        | Cmd::Unlock
        | Cmd::StateShow { .. }
        | Cmd::Output { .. }
        | Cmd::StateMv { .. }
        | Cmd::SecretsList { .. }
        | Cmd::SecretsRotate { .. }
        | Cmd::SecretsCycle
        | Cmd::SecretsSet { .. }
        | Cmd::ForgetHost { .. } => true,
        _ => false,
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
