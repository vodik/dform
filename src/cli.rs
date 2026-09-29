//! The command line (`dform`, README.md). `main` takes the backend the
//! run reaches its providers through.

use crate::ast::Atom;
use crate::ast::Term;
use crate::chaos::Chaos;
use crate::controller;
use crate::engine;
use crate::executor;
use crate::graph;
use crate::inputs;
use crate::ir;
use crate::loader;
use crate::partition;
use crate::plan_print::{self, waits_on};
use crate::plugin::{self, Providers};
use crate::provider::ActionKind;
use crate::query;
use crate::schema;
use crate::state;
use crate::store;
use crate::stuck;
use crate::value::Value;
use crate::watch;
use crate::why;
use crate::zset;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How this run reaches its providers (`main`'s `launch`).
static LAUNCH: std::sync::OnceLock<&'static (dyn plugin::Launch + Sync)> =
    std::sync::OnceLock::new();

fn launch() -> &'static dyn plugin::Launch {
    *LAUNCH
        .get()
        .expect("internal: cli::main sets the providers' backend")
}

/// The command line as typed (README "Commands"). `resolve` turns it into
/// one run, [`Cli`]: the target's files, its key, the project's state.
#[derive(Parser, Debug, Clone)]
#[command(name = "dform")]
#[command(about = "Facts + rules + constraints for infra", long_about = None)]
struct Args {
    /// Run as if dform started in DIR: the project, its dform.state/ and
    /// discovery are DIR's.
    #[arg(short = 'C', value_name = "DIR", global = true)]
    dir: Option<PathBuf>,

    #[command(flatten)]
    inputs: Inputs,

    #[command(subcommand)]
    cmd: Command,
}

/// What a run is given besides its target.
#[derive(clap::Args, Debug, Clone, Default)]
struct Inputs {
    /// A stack input: --set region=us-east1. A key input's value is the
    /// target's (`dform plan app env=prod`), never --set's.
    #[arg(long = "set", global = true, value_name = "K=V")]
    set: Vec<String>,

    /// Stack inputs from a .df file of facts, one `name(value).` per input
    /// (repeatable). Each is a normal contribution, like --set.
    #[arg(long = "input-file", global = true)]
    input_files: Vec<PathBuf>,

    /// Provide data facts: --data zone=us-test-1a
    #[arg(long = "data", global = true)]
    data: Vec<String>,

    /// Show no-op actions in plan
    #[arg(long, global = true)]
    show_noop: bool,

    /// Also pipe every audit log entry, a JSON line, to this command
    /// (`sh -c CMD`, once per entry). Overrides the stack's `audit_sink`.
    /// A sink that fails is a warning; the local log is authoritative.
    #[arg(long = "audit-sink", global = true)]
    audit_sink: Option<String>,
}

/// The mock's flags: `dform dev [FLAGS] COMMAND`.
#[derive(clap::Args, Debug, Clone, Default)]
struct Mock {
    /// A provider: a schema the mock provider plays (a name,
    /// providers/NAME/schema.df else a built-in, or a path to a schema .df
    /// file), or a plugin executable (a path to one, or to a directory
    /// holding a `dform-provider*`). Repeatable; overrides the program's
    /// `provider` statements.
    #[arg(long = "provider", global = true)]
    providers: Vec<String>,

    /// The fake provider's world file: what exists. Plan refreshes from it,
    /// apply writes it back. State sits beside it as <stem>.state.json.
    /// Default: dform.state/<stack>/remote.json.
    #[arg(long = "world", global = true)]
    world: Option<PathBuf>,

    /// Discovery inventory file (cloud_exists/cloud_attr/cloud_computed).
    /// Default: <world dir>/inventory.json if --world is given and that file
    /// exists, else dform.state/inventory.json.
    #[arg(long = "inventory", global = true)]
    inventory: Option<PathBuf>,

    /// Inject a failure into the fake provider at apply (repeatable):
    /// fail=T/N, timeout=T/N, crash=T/N, read-lag=T/N:READS,
    /// mutate=T/N:PATH=JSON, latency=T/N:MS, fresh-ids, stop-after=N.
    /// Deterministic; nothing sleeps.
    #[arg(long = "chaos", global = true)]
    chaos: Vec<String>,
}

/// What a command runs on: a stack by name (`infra`), a program file
/// (`stacks/infra.df`), or a deployment (`shop.app[env=prod]`, or
/// `shop.app env=prod`). None: the one stack under the current directory.
#[derive(clap::Args, Debug, Clone, Default)]
struct Target {
    /// A stack name, a .df file, or a deployment `NAME[K=V,...]`.
    #[arg(value_name = "TARGET")]
    target: Option<String>,
    /// The deployment's key values.
    #[arg(value_name = "K=V")]
    keys: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    #[command(flatten)]
    Run(Run),
    /// Format .df files in place: spacing, indentation and the commas a
    /// newline makes redundant (line breaks are kept). No PATH formats the
    /// project's .df files. `--check` changes nothing and fails if any
    /// file would.
    Fmt {
        paths: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// The project's stacks and their deployments.
    Stack {
        #[command(subcommand)]
        cmd: StackCommand,
    },
    /// A deployment's state.
    State {
        #[command(subcommand)]
        cmd: StateCommand,
    },
    /// Provider tools.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCommand,
    },
    /// Make the working directory a project: a minimal dform.toml, and
    /// dform.state/ in the nearest .gitignore.
    Init { name: Option<String> },
    /// Print a shell completion script: `dform completions zsh > _dform`.
    /// It completes stack names, key values and deployments by asking
    /// dform.
    Completions { shell: Shell },
    /// Development: the mock's flags (--world, --inventory, --provider,
    /// --chaos) before any command, and the evaluator's own views.
    Dev {
        #[command(flatten)]
        mock: Mock,
        #[command(subcommand)]
        cmd: DevCommand,
    },
    /// Serve the language server protocol on stdin and stdout: diagnostics
    /// of the selected environment, a contributors hover, schema
    /// completion (README "Language server").
    Lsp,
    /// The completion scripts' helper: candidates for the next word.
    #[command(name = "__complete", hide = true)]
    Complete { words: Vec<String> },
    /// Serve a provider built into dform (`fake`, the mock) over gRPC:
    /// how dform starts the mock, so it is always of the same build.
    #[command(name = "__provider", hide = true)]
    ServeProvider { name: String },
}

/// The commands that run on a target, at the top level and under `dev`.
#[derive(Subcommand, Debug, Clone)]
enum Run {
    /// Plan a deployment: what apply would do, and what it waits on.
    Plan {
        #[command(flatten)]
        target: Target,
        /// Write the plan file: inputs, a digest of the world, and the
        /// deformation delta with its nulls and tick schedule.
        /// `apply PLAN.json` applies exactly this delta or refuses.
        #[arg(long = "out")]
        out: Option<PathBuf>,
        /// Print the plan as one JSON document instead of text.
        #[arg(long)]
        json: bool,
        /// A what-if plan: the program with this scenario's facts and
        /// policy, against the stack's world.
        #[arg(long = "scenario")]
        scenario: Option<String>,
    },
    /// Apply a deployment, every key value named (`apply app env=prod`),
    /// or a plan file from `plan --out` (`apply PLAN.json`): refresh,
    /// re-evaluate, and refuse unless the delta is the file's.
    Apply {
        #[command(flatten)]
        target: Target,
        /// Stop after this many ticks (phase boundaries) if the stack is
        /// still deformed.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
        /// At most this many provider Apply calls in flight: a tick's
        /// independent actions overlap.
        #[arg(long = "parallel", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        parallel: u64,
        /// A signed approval of the plan file's digest (a JWT, or a DSSE
        /// envelope): verified against the stack's `approvals` trust root
        /// before any Apply call. Required when the policy says
        /// `requires_approval` of a deformation.
        #[arg(long = "approval")]
        approval: Option<PathBuf>,
        /// Apply without asking. Without it, apply prints the plan and asks
        /// before changing anything, and refuses when there is no terminal
        /// to ask on. `apply PLAN.json` never asks.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Print how a fact was derived: rule, bindings, the facts it read,
    /// recursively. Variables are allowed; every match is printed.
    Why {
        pattern: String,
        #[command(flatten)]
        target: Target,
        /// Show every alternative derivation, not only the first.
        #[arg(long)]
        all: bool,
    },
    /// Query the final fact store: a predicate name (every fact of it) or
    /// body literals with variables, printed as a table with one column per
    /// variable: `dform query 'attr(net.vpc, n, .cidr, c)'`.
    Query {
        pattern: String,
        #[command(flatten)]
        target: Target,
        /// Print the answer as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Run every scenario against an empty mock world: each passes when
    /// nothing is denied. Fails if any scenario is denied.
    Test {
        #[command(flatten)]
        target: Target,
    },
    /// The deployment's audit log (`state.audit.jsonl` beside its state),
    /// one line per entry; `log verify` checks its hash chain and names the
    /// first broken link.
    Log {
        #[command(subcommand)]
        cmd: Option<LogCommand>,
        #[command(flatten)]
        target: Target,
        /// Entries from this one on: a sequence number, or a time (RFC
        /// 3339, or a prefix of one: `2026-09-28`).
        #[arg(long)]
        since: Option<String>,
        /// Print the entries as one JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Controller mode.
    Controller {
        #[command(subcommand)]
        cmd: ControllerCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ControllerCommand {
    /// Wait for an input relation's source or the world to change, then
    /// refresh, evaluate, plan, gate on policy and apply, one log line per
    /// event and per tick. Refuses a `role = bootstrap` stack.
    Run {
        #[command(flatten)]
        target: Target,
        /// How often to look at the sources and the world file, in
        /// milliseconds (polling: no file notification).
        #[arg(long = "poll", default_value_t = 500)]
        poll: u64,
        /// Handle what changed since the last run (or a resync), then exit.
        #[arg(long)]
        once: bool,
        /// Exit after this many events (the start counts).
        #[arg(long = "max-events")]
        max_events: Option<usize>,
        /// Per event, stop after this many ticks if still deformed.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum LogCommand {
    /// Check the chain: every entry's hash is its content's and names the
    /// entry before. Fails naming the first broken link.
    Verify {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum StackCommand {
    /// Every stack of the project: its key, the deployments with state,
    /// and per deployment the last apply (commit, time, actor, from the
    /// audit log) and whether a saved plan is pending.
    List,
    /// Move one deployment of a keyed stack to another key value: `rekey
    /// app env=staging env=stg` moves the state of `app[env=staging]` to
    /// `app[env=stg]` (a directory, or a prefix of the stack's bucket), and
    /// its registry entry. Nothing in the cloud
    /// changes. First it lists the resources whose name-like attributes
    /// depend on the key (from provenance): the next plan renames them,
    /// usually a replace. With one side only (`rekey app env=staging`), it
    /// moves the state the stack had before it was keyed.
    Rekey {
        stack: String,
        /// Each key input as `k=v`: the old values, then the new ones.
        #[arg(value_name = "K=V", required = true)]
        pairs: Vec<String>,
    },
    /// Move a deployment's state to another backend and record it in the
    /// registry: `local("DIR")`, `s3("BUCKET", "PREFIX", {endpoint: "URL",
    /// region: "R"})` (the deployment's own prefix), or `k8s("ns/name")`,
    /// the in-cluster backend (for now a directory in the bootstrap stack's
    /// state). The controller runs the stack from there; a batch `apply`
    /// refuses it.
    Handover {
        stack: String,
        #[arg(long = "to")]
        to: String,
    },
    /// Remove a deployment's apply lock left by an apply that is gone.
    /// Refuses while the holder runs.
    Unlock {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum StateCommand {
    /// The deployment's state: each address, its provider and remote id.
    Show {
        #[command(flatten)]
        target: Target,
    },
    /// Forget one persisted extern answer (`extern ... persist`) of STACK:
    /// the answer for EXTERN with the input values ARGS, each written as
    /// `--set` takes a value. The next plan asks the provider again.
    Taint {
        stack: String,
        #[arg(value_name = "EXTERN")]
        pred: String,
        args: Vec<String>,
    },
    /// Give the object at FROM the address TO (each TYPE/NAME): nothing in
    /// the cloud changes, and the next plan sees the object under TO.
    Mv {
        from: String,
        to: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ProviderCommand {
    /// The conformance suite: run every protocol method against the
    /// provider at PATH (an executable, a directory holding one, or a mock
    /// schema, which the mock provider plays) with a synthetic schema, and
    /// report what deviates. Fails if anything does.
    Check { path: String },
    /// Print a provider's schema facts: a built-in mock schema's name, a
    /// schema .df, or a plugin executable.
    Schema { provider: String },
}

#[derive(Subcommand, Debug, Clone)]
enum DevCommand {
    #[command(flatten)]
    Run(Run),
    /// Print the stratification of the program (partition graph strata)
    Strata {
        #[command(flatten)]
        target: Target,
    },
    /// Graphviz DOT: the resource dependency DAG, the partition graph
    /// (`--strata`), or a binary relation (`--relation PRED` or `PRED/2`).
    Graph {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "relation")]
        strata: bool,
        #[arg(long)]
        relation: Option<String>,
    },
    /// The evaluation's size and resources.
    Eval {
        #[command(flatten)]
        target: Target,
    },
    /// A resource's desired document.
    Show {
        #[arg(value_name = "TYPE")]
        typ: String,
        name: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum Shell {
    Zsh,
    Bash,
    Fish,
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
}

/// What a run does.
#[derive(Debug, Clone)]
enum Cmd {
    Eval,
    Plan {
        out: Option<PathBuf>,
        json: bool,
        scenario: Option<String>,
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
    },
    Query {
        pattern: String,
        json: bool,
    },
    Why {
        pattern: String,
        all: bool,
    },
    Show {
        typ: String,
        name: String,
    },
    Strata,
    Fmt {
        paths: Vec<PathBuf>,
        check: bool,
    },
    Graph {
        what: Option<String>,
    },
    Controller {
        poll: u64,
        once: bool,
        max_events: Option<usize>,
        max_ticks: usize,
    },
    Taint {
        stack: String,
        pred: String,
        args: Vec<String>,
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
    StateShow,
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
    let args = Args::parse_from(args);
    let result = match &args.cmd {
        Command::ServeProvider { name } => serve_provider(name),
        Command::Lsp => dform_lsp::serve_stdio(dform_lsp::Options {
            version: env!("CARGO_PKG_VERSION"),
            real: launch,
        }),
        _ => resolve(args).and_then(|cli| run(cli, None)),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            use std::io::IsTerminal;
            eprint!(
                "{}",
                crate::diag::report(&e, std::io::stderr().is_terminal())
            );
            std::process::ExitCode::FAILURE
        }
    }
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
/// not set another. For tests that drive many runs (`tests/model.rs`).
pub fn run_in_process(
    launch: &'static (dyn plugin::Launch + Sync),
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<()> {
    let set = *LAUNCH.get_or_init(|| launch);
    if !std::ptr::addr_eq(set, launch) {
        bail!("internal: this process reaches its providers through another backend");
    }
    run(resolve(Args::try_parse_from(args)?)?, None)
}

/// The run the command line asks for: `-C` taken, the working project
/// found, the target resolved to its program file and key.
fn resolve(args: Args) -> Result<Cli> {
    if let Some(dir) = &args.dir {
        std::env::set_current_dir(dir).map_err(|e| anyhow::anyhow!("-C {}: {e}", dir.display()))?;
    }
    let version = env!("CARGO_PKG_VERSION");
    let project = crate::project::Project::find(Path::new("."), version)?;
    let mut mock = Mock::default();
    let (cmd, target): (Cmd, Option<Target>) = match args.cmd {
        Command::Run(r) => run_cmd(r),
        Command::Dev { mock: m, cmd } => {
            mock = m;
            match cmd {
                DevCommand::Run(r) => run_cmd(r),
                DevCommand::Strata { target } => (Cmd::Strata, Some(target)),
                DevCommand::Graph {
                    target,
                    strata,
                    relation,
                } => (
                    Cmd::Graph {
                        what: if strata {
                            Some("strata".into())
                        } else {
                            relation
                        },
                    },
                    Some(target),
                ),
                DevCommand::Eval { target } => (Cmd::Eval, Some(target)),
                DevCommand::Show { typ, name, target } => (Cmd::Show { typ, name }, Some(target)),
            }
        }
        Command::Fmt { paths, check } => (Cmd::Fmt { paths, check }, None),
        Command::Stack { cmd } => match cmd {
            StackCommand::List => (Cmd::StackList, None),
            StackCommand::Rekey { stack, pairs } => {
                let target = Target {
                    target: Some(stack.clone()),
                    keys: vec![],
                };
                (Cmd::Rekey { stack, pairs }, Some(target))
            }
            StackCommand::Handover { stack, to } => (Cmd::Handover { stack, to }, None),
            StackCommand::Unlock { target } => (Cmd::Unlock, Some(target)),
        },
        Command::State { cmd } => match cmd {
            StateCommand::Show { target } => (Cmd::StateShow, Some(target)),
            StateCommand::Taint { stack, pred, args } => (Cmd::Taint { stack, pred, args }, None),
            StateCommand::Mv { from, to, target } => (Cmd::StateMv { from, to }, Some(target)),
        },
        Command::Provider { cmd } => match cmd {
            ProviderCommand::Check { path } => (Cmd::ProviderCheck { path }, None),
            ProviderCommand::Schema { provider } => (Cmd::ProviderSchema { provider }, None),
        },
        Command::Init { name } => (Cmd::Init { name }, None),
        Command::Completions { shell } => (Cmd::Completions { shell }, None),
        Command::Complete { words } => (Cmd::Complete { words }, None),
        Command::ServeProvider { .. } => bail!("internal: `__provider` serves before a project"),
        Command::Lsp => bail!("internal: `lsp` serves before a project"),
    };
    let inputs = args.inputs;
    let mut cli = Cli {
        cmd,
        files: Vec::new(),
        keys: Vec::new(),
        user_set: inputs.set.clone(),
        set: inputs.set,
        input_files: inputs.input_files,
        data: inputs.data,
        show_noop: inputs.show_noop,
        providers: mock.providers,
        world: mock.world,
        in_project: project.is_some(),
        // Outside a project nothing writes the state root (`needs_project`).
        root: project.as_ref().map_or_else(
            || PathBuf::from(crate::project::STATE_DIR),
            |p| p.state_root(),
        ),
        manifest: None,
        inventory: mock.inventory,
        audit_sink: inputs.audit_sink,
    };
    if let Cmd::Apply { chaos, .. } = &mut cli.cmd {
        *chaos = mock.chaos;
    } else if !mock.chaos.is_empty() {
        bail!("--chaos is for apply");
    }
    // A plan file is `apply`'s target: its inputs name the program.
    if let (Cmd::Apply { plan_file, .. }, Some(t)) = (&mut cli.cmd, &target)
        && let Some(f) = t.target.as_deref().filter(|f| f.ends_with(".json"))
    {
        if !t.keys.is_empty() {
            bail!("apply {f}: a plan file names its deployment; give no key values");
        }
        *plan_file = Some(PathBuf::from(f));
        return Ok(cli);
    }
    let Some(target) = target else {
        return Ok(cli);
    };
    let (file, keys) = target_of(project.as_ref(), &target)?;
    cli.set.extend(keys.iter().map(|(k, v)| format!("{k}={v}")));
    cli.keys = keys;
    cli.files = vec![file];
    Ok(cli)
}

/// A command that runs on a target, and the target.
fn run_cmd(r: Run) -> (Cmd, Option<Target>) {
    match r {
        Run::Plan {
            target,
            out,
            json,
            scenario,
        } => (
            Cmd::Plan {
                out,
                json,
                scenario,
            },
            Some(target),
        ),
        Run::Apply {
            target,
            max_ticks,
            parallel,
            approval,
            yes,
        } => (
            Cmd::Apply {
                plan_file: None,
                chaos: Vec::new(),
                max_ticks,
                parallel,
                approval,
                yes,
            },
            Some(target),
        ),
        Run::Why {
            pattern,
            target,
            all,
        } => (Cmd::Why { pattern, all }, Some(target)),
        Run::Query {
            pattern,
            target,
            json,
        } => (Cmd::Query { pattern, json }, Some(target)),
        Run::Test { target } => (Cmd::Test, Some(target)),
        Run::Log {
            cmd,
            target,
            since,
            json,
        } => {
            let (verify, target) = match cmd {
                Some(LogCommand::Verify { target }) => (true, target),
                None => (false, target),
            };
            (
                Cmd::Log {
                    verify,
                    since,
                    json,
                },
                Some(target),
            )
        }
        Run::Controller {
            cmd:
                ControllerCommand::Run {
                    target,
                    poll,
                    once,
                    max_events,
                    max_ticks,
                },
        } => (
            Cmd::Controller {
                poll,
                once,
                max_events,
                max_ticks,
            },
            Some(target),
        ),
    }
}

/// The program file and key values a target names. A path (`.df`) is the
/// program; a name is the project's stack of that name; `NAME[K=V,...]`
/// and trailing `K=V`s are the key. No target: the one stack under the
/// working directory, else the stacks there are listed.
fn target_of(
    project: Option<&crate::project::Project>,
    t: &Target,
) -> Result<(PathBuf, Vec<(String, String)>)> {
    let mut keys = Vec::new();
    let pair = |kv: &str| -> Result<(String, String)> {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected a key value K=V, got '{kv}'"))?;
        Ok((k.trim().to_string(), v.trim().to_string()))
    };
    let mut name = t.target.clone();
    if let Some(n) = &name
        && n.contains('=')
        && !n.contains('[')
    {
        keys.push(pair(n)?);
        name = None;
    }
    if let Some(n) = name.clone()
        && let Some((stack, rest)) = n.strip_suffix(']').and_then(|n| n.split_once('['))
    {
        for kv in rest.split(',').filter(|kv| !kv.trim().is_empty()) {
            keys.push(pair(kv)?);
        }
        name = Some(stack.to_string());
    }
    for kv in &t.keys {
        keys.push(pair(kv)?);
    }
    // A name, or no target, is the project's discovery's.
    let project_or = |what: &str| {
        project.ok_or_else(|| {
            anyhow::anyhow!(
                "{what}: {}; outside a project, name a program file (`dform plan path/to/file.df`)",
                crate::project::not_in_a_project(Path::new("."))
            )
        })
    };
    let found = |project: &crate::project::Project| {
        let d = crate::project::discover(project);
        for w in &d.warnings {
            eprintln!("warning: {w}");
        }
        d
    };
    let file = match name {
        Some(n) if n.ends_with(".df") || Path::new(&n).is_file() => {
            if let Some(p) = project {
                found(p).check()?;
            }
            PathBuf::from(n)
        }
        Some(n) => {
            let project = project_or(&format!("stack {n}"))?;
            let d = found(project);
            d.check()?;
            match d.named(&n).as_slice() {
                [one] => one.file.clone(),
                _ => bail!(
                    "no stack {n} in the project at {}{}",
                    project.root.display(),
                    listing(&d.stacks)
                ),
            }
        }
        None => {
            let project = project_or("no target")?;
            let d = found(project);
            d.check()?;
            let here: Vec<&crate::project::Found> = d
                .stacks
                .iter()
                .filter(|s| s.file.is_relative() && !s.file.starts_with(".."))
                .collect();
            match here.as_slice() {
                [one] => one.file.clone(),
                [] => bail!(
                    "no stack under {}: a stack is a .df file with a `stack` statement; name \
                     a program file (`dform plan path/to/file.df`){}",
                    std::env::current_dir()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                    listing(&d.stacks)
                ),
                many => bail!(
                    "{} stacks under the current directory; name one:{}",
                    many.len(),
                    listing(&many.iter().map(|f| (*f).clone()).collect::<Vec<_>>())
                ),
            }
        }
    };
    Ok((file, keys))
}

/// `\n  NAME[KEY]  FILE` per stack, for an error that lists them.
fn listing(stacks: &[crate::project::Found]) -> String {
    if stacks.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nthe project's stacks:");
    for s in stacks {
        let key = if s.keys.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.keys.join(", "))
        };
        out.push_str(&format!("\n  {}{key}  {}", s.name, s.file.display()));
    }
    out
}

/// An apply's audit session: its log, and the stack's lock, held until the
/// apply's end is logged.
struct Session {
    log: crate::audit::Log,
    _lock: crate::store::Guard,
}

/// One run of the command line. `hook`: controller mode's part of an apply
/// (`controller::Hook`). An apply's end, whatever it is, goes to the audit
/// log.
fn run(cli: Cli, hook: Option<&mut controller::Hook>) -> Result<()> {
    let mut session = None;
    let r = run_with(cli, hook, &mut session);
    if let Some(s) = session {
        let end = match &r {
            Ok(()) => serde_json::json!({ "result": "ok" }),
            Err(e) if e.is::<Declined>() => serde_json::json!({ "result": "declined" }),
            Err(e) => serde_json::json!({ "result": "failed", "error": e.to_string() }),
        };
        let logged = s.log.append("apply_end", end);
        r?;
        logged?;
        return Ok(());
    }
    r
}

fn run_with(
    mut cli: Cli,
    mut hook: Option<&mut controller::Hook>,
    session: &mut Option<Session>,
) -> Result<()> {
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
    if let Cmd::Controller { .. } = cli.cmd {
        return run_controller(cli);
    }
    match &cli.cmd {
        Cmd::Handover { stack, to } => {
            let (from, home, times) = place_of(&cli, stack)?;
            let opener = open_s3(&cli.root, true);
            let moved = crate::stack::handover(&cli.root, stack, &from, &home, to, &opener, times)?;
            crate::audit::Log::new(moved.open(&opener)?, cli.audit_sink.clone()).append(
                "handover",
                serde_json::json!({ "stack": stack, "to": to, "who": crate::audit::who() }),
            )?;
            println!("stack {stack} handed over to {to}: {moved}");
            return Ok(());
        }
        Cmd::Taint { stack, pred, args } => return taint(&cli, stack, pred, args),
        Cmd::StackList => return stack_list(&cli),
        Cmd::Init { name } => {
            for line in crate::project::init(Path::new("."), name.as_deref())? {
                println!("{line}");
            }
            return Ok(());
        }
        Cmd::Completions { shell } => {
            print!("{}", completion_script(*shell));
            return Ok(());
        }
        Cmd::Complete { words } => return complete(words),
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
            return Ok(());
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
            return Ok(());
        }
        Cmd::Fmt { paths, check } => {
            let paths = if paths.is_empty() {
                let project =
                    crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
                crate::project::df_files(&project)
            } else {
                paths.clone()
            };
            return fmt_files(&paths, *check);
        }
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
    // The program's project's manifest (a plan file's program's, too).
    if let Some(root) = crate::project::manifest_root(&files[0]) {
        cli.manifest = Some(crate::project::Manifest::load(
            &root.join(crate::project::MANIFEST),
            env!("CARGO_PKG_VERSION"),
        )?);
    }
    let mut program = loader::load_program(&files)?;
    // Input relations: declared, and stated as their sources hold them now.
    let relations = watch::take(&mut program)?;
    if let Some(h) = hook.as_deref_mut() {
        h.inputs(&relations);
    }
    program.statements.extend(watch::read(&relations)?);
    // The commit each git input relation's ref names: a plan file pins them.
    let pinned = pinned_commits(&relations);
    if let Cmd::Plan {
        scenario: Some(name),
        ..
    } = &cli.cmd
    {
        program = crate::scenario::select(&program, name)?;
    }
    // `stack` and `provider` statements, over the manifest's defaults;
    // `--provider` overrides the latter.
    if let Some(m) = &cli.manifest {
        with_default_unknowns(&mut program, m);
    }
    let mut stack_cfg = crate::stack::config(&program)?;
    if let Some(m) = &cli.manifest {
        with_manifest(&mut stack_cfg, m, &files[0]);
    }
    // A key's value is the target's, else its input's default.
    let own = stack_cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    check_keys(&cli, &stack_cfg, &own)?;
    // `stack rekey`: the run is of the old deployment (its state, its
    // world), the provenance of its names is listed, and its state moves.
    let rekey = match cli.cmd.clone() {
        Cmd::Rekey { stack, pairs } => {
            Some(rekey_args(&mut cli, &stack_cfg, &files, &stack, &pairs)?)
        }
        _ => None,
    };
    let providers = if cli.providers.is_empty() {
        stack_cfg.providers.clone()
    } else {
        cli.providers.clone()
    };
    // The stack's and the instances' typed inputs, when the program lowers
    // (when it does not, evaluation reports why).
    let lowered = crate::transform::lower(&program).ok();
    let declared = lowered
        .as_ref()
        .map(|l| l.inputs.clone())
        .unwrap_or_default();
    // An input a fact of the program gives (a scenario's `with k = v`).
    let mut given: BTreeSet<String> = input_fact_keys(&program);
    for f in &cli.input_files {
        let src = std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("read --input-file {}: {e}", f.display()))?;
        let facts = crate::parser::parse_file(&f.display().to_string(), &src)?;
        let stmts = inputs::file_stmts(&facts, &declared)?;
        given.extend(facts.statements.iter().filter_map(|s| match s {
            crate::ast::Stmt::Fact(a) => Some(a.pred.clone()),
            _ => None,
        }));
        program.statements.extend(stmts);
    }
    if let Cmd::Test = cli.cmd {
        return run_tests(&program, &providers, &cli, &files);
    }
    if let Cmd::Strata = cli.cmd {
        return print_strata(&files, &program, &load_schema(&providers)?);
    }
    if let Cmd::Graph { what: Some(w) } = &cli.cmd
        && w == "strata"
    {
        let graph = partition::build(&program, &load_schema(&providers)?)?;
        return match partition::stratify(&graph) {
            partition::Verdict::Stratified { strata } => {
                print!("{}", graph::strata(&graph, Some(&strata)));
                Ok(())
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

    let set_keys: Vec<String> = cli
        .set
        .iter()
        .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_string()))
        .collect();
    for w in crate::lint::lint(&program, &set_keys) {
        eprintln!("warning: {w}");
    }
    // Every evaluation carries the rules that derive the lifecycle denies
    // from the deformation the planner hands back (`zset::POLICY_RULES`).
    let program = zset::with_policy_rules(program)?;

    let root = cli.root.clone();
    let stack = stack_cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    let mut set = Vec::new();
    for kv in &cli.set {
        let (k, v) = split_kv(kv)?;
        given.insert(k.to_string());
        set.push((k.to_string(), v));
    }
    let set_facts = inputs::set_facts(&declared, &set)?;
    // The deployment this run is of: the stack, or one value of its key.
    let instance = match &rekey {
        Some(r) => r.from.clone(),
        None => crate::stack::instance(&stack_cfg, &stack, &program, &set_facts)?,
    };
    let deployment = instance.name();
    inputs::check_required(&declared, &given)?;
    // A keyed stack's plan and apply say first which deployment they are
    // of, and which of its key values are defaults.
    let text_plan = matches!(cli.cmd, Cmd::Plan { json: false, .. });
    if !instance.key.is_empty()
        && hook.is_none()
        && (text_plan || matches!(cli.cmd, Cmd::Apply { .. }))
    {
        println!("deployment: {}", instance.describe());
    }
    // Where the stack's deployments are (its backend's, else the state
    // root's), and this one's objects: there, or where it was handed over
    // to. A `--world` fixture's are beside it.
    let base = stack_location(&root, &stack, stack_cfg.backend.as_ref());
    // The deployment's own directory under the state root: its world's
    // when its state is in a bucket (the world is the provider's).
    let home = instance.dir(&root.join(&stack));
    let handed = match &cli.world {
        None => crate::stack::handed_over(&root, &deployment)?,
        Some(_) => None,
    };
    let location = match &handed {
        Some((_, loc)) => loc.clone(),
        None => base.child(instance.segment().as_deref()),
    };
    let mut paths = match &cli.world {
        Some(w) => state::world_paths(&root, w),
        None => state::StackPaths {
            state: match &location {
                store::Location::Local(dir) => state::state_path(dir),
                store::Location::S3(_) => state::state_path(&home),
            },
            world: crate::stack::world_file(&location, &home),
            inventory: root.join("inventory.json"),
        },
    };
    paths.inventory = resolve_inventory(&cli.inventory, &cli.world, &paths.inventory);
    let times = cli
        .manifest
        .as_ref()
        .map(|m| m.lease_times())
        .unwrap_or_default();
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
    let dep = match &cli.world {
        Some(_) => store::Deployment::local(&paths.state, &deployment),
        None => store::Deployment::new(location.open(&open_s3(&root, writes))?, &deployment, times),
    };
    // The deployment's audit log, beside its state.
    let audit = dep.audit(cli.audit_sink.clone().or(stack_cfg.audit_sink.clone()));
    match &cli.cmd {
        Cmd::Log {
            verify,
            since,
            json,
        } => {
            return print_log(&audit, &deployment, *verify, since.as_deref(), *json);
        }
        Cmd::Unlock => {
            println!("{}", dep.unlock()?);
            return Ok(());
        }
        Cmd::StateShow => return state_show(&dep),
        Cmd::StateMv { from, to } => {
            return state_mv(&dep, from, to, &audit);
        }
        _ => {}
    }
    // The stack's plan-file key: a plan that writes a file, and an apply,
    // digest secrets with it (the plan file's, the audit log's).
    let key = match (&saved, &cli.cmd) {
        (Some(_), _) | (None, Cmd::Plan { out: Some(_), .. }) | (None, Cmd::Apply { .. }) => {
            Some(dep.plan_key()?)
        }
        _ => None,
    };
    let secret_inputs: BTreeSet<String> = declared
        .iter()
        .filter(|d| d.scope.is_empty())
        .filter(|d| matches!(&d.decl.ty, crate::ast::TypeExpr::Apply(n, _) if n == "secret"))
        .map(|d| d.decl.name.clone())
        .collect();
    // Other stacks' published outputs: each deployment the program names,
    // of the project (every registered one, when it names one by a
    // variable) or of a remote, through its backend. Read once, as facts;
    // a plan file records their digests, and an apply of it refuses when
    // one moved.
    let (named, any_name) = lowered
        .as_ref()
        .map(|l| crate::stack::named_outputs(&l.program))
        .unwrap_or_default();
    let remotes = cli
        .manifest
        .as_ref()
        .map(|m| m.remotes())
        .unwrap_or_default();
    let read_outputs = crate::stack::stack_outputs(
        &root,
        &deployment,
        (!any_name).then_some(&named),
        &remotes,
        &open_s3(&root, false),
    )?;
    let secret_outputs = crate::stack::secret_outputs(&read_outputs);
    let outputs_read: Vec<zset::file::OutputsDigest> = read_outputs
        .iter()
        .map(|r| zset::file::OutputsDigest {
            deployment: r.name.clone(),
            digest: r.digest.clone(),
        })
        .collect();
    let plan_inputs = |key: &zset::file::Key| -> Result<zset::file::Inputs> {
        Ok(zset::file::Inputs {
            stack_outputs: outputs_read.clone(),
            ..plan_inputs(&cli, &files, &secret_inputs, key)?
        })
    };
    // What the plan file records, when one is written or read.
    let inputs = match &key {
        Some(k) => Some(plan_inputs(k)?),
        None => None,
    };
    if let (Some((path, saved)), Some(inputs), Some(k)) = (&saved, &inputs, &key) {
        // The environment variables the plan read, as they are now.
        let labels = saved
            .inputs
            .env
            .iter()
            .filter_map(|e| e.get("sensitive")?.as_str().map(str::to_string));
        let now = zset::file::Inputs {
            env: env_inputs(labels, k),
            ..inputs.clone()
        };
        let mut diff = saved.input_differences(&now);
        diff.extend(saved.commit_differences(&pinned));
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
        if stack_cfg.bootstrap {
            bail!(
                "stack {deployment} is role = bootstrap: it stays batch, and the controller never runs it"
            );
        }
        h.audit = Some(audit.clone());
        h.open(
            dep.store().clone(),
            &paths.world,
            root.parent().unwrap_or(Path::new("")),
        )?;
    } else if let (Cmd::Apply { .. }, Some((to, _))) = (&cli.cmd, &handed) {
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
    let chaos_specs = match &cli.cmd {
        Cmd::Apply { chaos, .. } => chaos.clone(),
        _ => Vec::new(),
    };
    // What providers and trust roots fetch is cached in the state root's
    // cache/, a world fixture's beside it.
    let cache = match &cli.world {
        Some(w) => w.parent().unwrap_or(Path::new("")).to_path_buf(),
        None => root.join("cache"),
    };
    // The schema is asked for once the run knows the types it names.
    let backend = Providers::start_deferred(
        launch(),
        &providers,
        &plugin::Config {
            world: paths.world.clone(),
            inventory: paths.inventory.clone(),
            chaos: chaos_specs,
            cache: cli.world.is_none().then(|| cache.clone()),
            configured: provider_configs(&program),
            stack: deployment.clone(),
            blocks: stack_cfg.provider_blocks.clone(),
        },
    )?;
    // Externs are asked on demand: a table's of its file, else of the file
    // provider, else of the mock.
    let (no_program, no_fns) = (crate::ast::Program { statements: vec![] }, vec![]);
    let program_dir = crate::project::base_of(&files[0]);
    let tables = crate::tables::Tables::default();
    let externs = crate::externs::Externs::new(
        lowered.as_ref().map_or(&no_program, |l| &l.program),
        lowered.as_ref().map_or(&no_fns, |l| &l.extern_fns),
        |f, inputs| {
            if let Some(r) = tables.answer(f, inputs) {
                return r;
            }
            if let Some(r) = crate::externs::file(f, inputs, &program_dir) {
                return r;
            }
            if let Some(r) = crate::externs::env_var(f, inputs) {
                return r;
            }
            let plus: Vec<bool> = f.args.iter().map(|b| b.input).collect();
            backend.query(&f.name, &plus, inputs)
        },
    );

    let mut st = dep.load_state()?;
    // What the plan file read, then what state persisted, before asking.
    if let Some((_, saved)) = &saved {
        externs.preload(saved.externs.clone());
    }
    externs.preload_persisted(st.externs.clone());

    let mut base_extra = set_facts;
    base_extra.extend(build_extra_facts(&cli.data)?);
    base_extra.extend(crate::stack::output_facts(&read_outputs));
    // The manifest, as facts policy may read.
    if let Some(m) = &cli.manifest {
        base_extra.extend(m.facts());
    }
    let discovered = backend.discover(world_types(&cli.cmd, lowered.as_ref()).as_ref())?;
    let scope = catalog_scope(&cli.cmd, &program, &base_extra, &discovered, &st);
    backend.load_schema(scope.as_ref())?;
    // What a run plans, its providers declare: a type none does would be
    // handed to one that knows nothing of it.
    if matches!(cli.cmd, Cmd::Plan { .. } | Cmd::Apply { .. }) || hook.is_some() {
        backend.check_types(&program, &providers)?;
    }
    // A provider configured from what it serves itself is a cycle.
    if let Some(l) = &lowered {
        backend.check_configuration(&l.program, &l.extern_fns, |p| {
            p == crate::syntax::resolve::ENV_VAR
                || p.starts_with("file.")
                || crate::tables::describe(p).is_some()
        })?;
    }
    // Apply calls whose answer was lost, resolved before anything is
    // planned: a plan sees what they did (`apply` writes it down).
    for line in executor::resolve_uncertain(&backend, &mut st)? {
        eprintln!("resolved: {line}");
    }
    // The static secret pass and the refinement checks (a literal that
    // violates one, E0306), against the provider's schema.
    if let Some(l) = &lowered {
        crate::secrets::check(l, backend.schema(), &secret_outputs)?;
        crate::refine::check(&l.program, backend.schema())?;
    }
    // An `expect_account` a secret reaches is named by its label.
    let secret_accounts = lowered
        .as_ref()
        .map(|l| crate::secrets::secret_expected_accounts(l, backend.schema(), &secret_outputs))
        .unwrap_or_default();
    base_extra.extend(backend.catalog(scope.as_ref())?);
    base_extra.extend(discovered);
    if let Some(h) = hook.as_deref_mut() {
        base_extra.extend(h.drift_facts(&backend.observe(&st)?));
    }
    // Refresh as facts: round 0 resolves every null the world can answer,
    // except those of `withheld` addresses (being replaced). `more`: the
    // deformation facts of a policy pass, which continues the last
    // evaluation over the same facts from the first stratum that reads
    // them (`engine::Resumable`); `why` labels them as the plan's, of apply
    // tick `tick` (`None`: of `plan`).
    let last: std::cell::RefCell<Option<(Vec<Atom>, engine::Resumable)>> = Default::default();
    let evaluate_with = |st: &state::State,
                         withheld: &BTreeSet<ir::Address>,
                         more: &[Atom],
                         tick: Option<usize>|
     -> Result<(engine::EvalResult, Vec<String>)> {
        let mut extra = base_extra.clone();
        extra.extend(executor::withhold(backend.world_facts(st)?, withheld));
        let (res, mut violations) = if more.is_empty() {
            let (mut res, mut violations, mut resumable) =
                externs.eval_resumable(&program, &extra, zset::POLICY_INPUTS)?;
            // A provider the program configures, its settings now known,
            // is configured, and what it serves read again.
            if backend.configure_from(&res.facts)? {
                extra = base_extra.clone();
                extra.extend(executor::withhold(backend.world_facts(st)?, withheld));
                (res, violations, resumable) =
                    externs.eval_resumable(&program, &extra, zset::POLICY_INPUTS)?;
            }
            // Each provider reaches the account the program expects of it
            // (`expect_account`), or nothing is planned.
            backend
                .check_accounts(&res.facts, &secret_accounts)
                .with_context(|| format!("deployment {deployment}"))?;
            *last.borrow_mut() = Some((extra, resumable));
            (res, violations)
        } else {
            let resumed = match &*last.borrow() {
                Some((seen, resumable)) if *seen == extra => Some(resumable.with_at(more, tick)?),
                _ => None,
            };
            match resumed {
                Some((res, violations)) if externs.settle(&res.facts)? => (res, violations),
                _ => {
                    // A new extern call: the answers the resumable was
                    // taken with are not all of them any more.
                    *last.borrow_mut() = None;
                    extra.extend(more.iter().cloned());
                    externs.eval_at(&program, &extra, tick)?
                }
            }
        };
        violations.extend(inputs::violations(&res.facts, &declared));
        Ok((res, violations))
    };
    let evaluate = |st: &state::State| evaluate_with(st, &BTreeSet::new(), &[], None);
    let (mut res, mut violations) = evaluate(&st)?;
    // moved/3 rewrites state's identity before the diff (E §3.4); round 0
    // must see the new addresses, so the program is evaluated again.
    let moves = st.apply_moves(&zset::Lifecycle::from_facts(&res.facts, backend.schema())?.moved);
    if !moves.is_empty() {
        (res, violations) = evaluate(&st)?;
    }
    // The tables' sources, for the controller; a git table whose ref has
    // moved since the deployment was last applied says so.
    if let Some(h) = hook.as_deref_mut() {
        h.tables(&tables.sources());
    }
    if matches!(cli.cmd, Cmd::Plan { .. } | Cmd::Apply { .. }) {
        for m in crate::tables::moved(&st.externs, &externs.recorded()) {
            match hook {
                Some(_) => controller::log(format_args!("{m}")),
                None => println!("{m}"),
            }
        }
    }
    // Policy messages quote values and rule text: printed redacted.
    // The collision lint of a keyed stack: a name every deployment writes
    // the same. Under strict mode a `deny` fact, which `query` and `why`
    // see too.
    let strict = stack_cfg.unknowns == crate::stack::Unknowns::Strict;
    let collisions = if !stack_cfg.keys.is_empty()
        && !stack_cfg.isolated
        && matches!(
            cli.cmd,
            Cmd::Plan { .. } | Cmd::Apply { .. } | Cmd::Query { .. } | Cmd::Why { .. }
        ) {
        let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
        crate::lint::key_collisions(&res, backend.schema(), &keys, &deployment)
    } else {
        Vec::new()
    };
    if strict {
        crate::lint::deny_collisions(&mut res, &collisions);
        violations.extend(collisions.iter().map(|c| c.text.clone()));
    } else if matches!(cli.cmd, Cmd::Plan { .. } | Cmd::Apply { .. }) {
        for c in &collisions {
            eprintln!("warning: {}", c.text);
        }
    }
    let redact = query::Redactor::new(&res.facts, backend.schema());
    for w in &res.warnings {
        eprintln!("warning: {}", redact.text(w));
    }
    let blocked = |violations: &[String]| -> Result<()> {
        if violations.is_empty() {
            return Ok(());
        }
        eprintln!("constraint violations:");
        for v in violations {
            eprintln!("- {}", redact.text(v));
        }
        bail!("blocked by constraints");
    };
    // A plan prints what it would do, conflicts included (E §2.8: a
    // conflict is a fact, not an abort), and then refuses; query and why
    // explain what blocks it.
    if !matches!(
        cli.cmd,
        Cmd::Plan { .. } | Cmd::Query { .. } | Cmd::Why { .. } | Cmd::Rekey { .. }
    ) {
        blocked(&violations)?;
    }

    let resources = ir::compile_resources(res.facts.iter().cloned(), backend.schema())?;
    let adopts = ir::compile_adopts(res.facts.iter())?;
    let lifecycle = zset::Lifecycle::from_facts(&res.facts, backend.schema())?;
    let schema = backend.schema();
    // The provider's plan for this evaluation (whose violations are
    // `violations`), and the policy over it. A replace makes a new object,
    // so the nulls that named the old one are retracted (`executor`): the
    // program is evaluated again without the replaced identities, and what
    // reads them is held until the replacement exists. Then the policy pass
    // (E §2.8): the plan's deformations go back to the evaluator as facts
    // and the program is evaluated once more; the denies it derives beyond
    // the plan's own evaluation are the denies over the plan. Returns that
    // evaluation, the documents the plan was taken from, the plan, its
    // sections and the denies.
    let plan_for = |res: engine::EvalResult,
                    violations: &[String],
                    resources: Vec<ir::Resource>,
                    adopts: &[ir::Adopt],
                    lifecycle: &zset::Lifecycle,
                    st: &state::State|
     -> Result<Planned> {
        let mut plan = backend.plan(&resources, adopts, lifecycle, st)?;
        let replaced = executor::replaced(&plan);
        let (res, violations, resources) = if replaced.is_empty() {
            (res, violations.to_vec(), resources)
        } else {
            let (again, violations) = evaluate_with(st, &replaced, &[], None)?;
            let docs = ir::compile_resources(again.facts.iter().cloned(), schema)?;
            plan = backend.plan_retracting(&docs, adopts, lifecycle, st, &replaced)?;
            executor::hold_dependents(&mut plan, &docs, &replaced);
            (again, violations, docs)
        };
        let sections = plan_sections(&res, &resources, schema);
        executor::hold_deposed(&mut plan, &resources, &sections);
        // The resource rules that may derive after a boundary (pending
        // groups), for strict mode.
        let may_derive: Vec<Atom> = res
            .may_derive
            .iter()
            .filter(|m| m.head.pred == "want")
            .map(|m| m.fact())
            .collect();
        drop(res);
        let observed = backend.observe(st)?;
        let before = observed
            .iter()
            .map(|(a, d)| (a.clone(), Some(d.clone())))
            .collect();
        let mut facts = zset::deformation_facts(
            plan.actions.iter().filter_map(|a| {
                let held = waits_on(a, &sections).is_some();
                Some((zset::deformation_kind(&a.kind, held)?, &a.addr))
            }),
            &before,
            &observed,
        );
        facts.extend(may_derive);
        let (res, all) = evaluate_with(st, &replaced, &facts, None)?;
        let again = ir::compile_resources(res.facts.iter().cloned(), schema)?;
        if again.len() != resources.len()
            || again
                .iter()
                .zip(&resources)
                .any(|(a, b)| a.addr != b.addr || a.attrs != b.attrs)
        {
            bail!(
                "a resource rule reads deformation/4 or world_digest/3: the plan would \
                 depend on itself (only policy may read the deformation)"
            );
        }
        let denies = all
            .into_iter()
            .filter(|v| !violations.contains(v))
            .collect();
        Ok(Planned {
            res,
            resources,
            plan,
            sections,
            denies,
        })
    };
    // query and why read the policy pass, so a deny over the plan can be
    // asked for and explained; when there is no plan (planning fails), the
    // program's own evaluation.
    let explained = |res: engine::EvalResult| -> engine::EvalResult {
        match plan_for(
            res.clone(),
            &violations,
            resources.clone(),
            &adopts,
            &lifecycle,
            &st,
        ) {
            Ok(p) => p.res,
            Err(_) => res,
        }
    };
    let report_of = |plan: &crate::provider::Plan,
                     res: &engine::EvalResult,
                     sections: &stuck::Sections,
                     tick: usize,
                     moved: &[(ir::Address, ir::Address)],
                     denies: &[String]| {
        plan_print::report(&plan_print::Input {
            plan,
            res,
            sections,
            program: &program,
            schema,
            stack: &stack,
            show_noop: cli.show_noop,
            tick,
            moved,
            denies,
        })
    };
    let show = |plan: &crate::provider::Plan,
                res: &engine::EvalResult,
                sections: &stuck::Sections,
                tick: usize,
                moved: &[(ir::Address, ir::Address)],
                denies: &[String]| {
        print!(
            "{}",
            report_of(plan, res, sections, tick, moved, denies).text()
        )
    };
    // `apply PLAN`: the delta re-evaluated at each tick must be the file's.
    let check_saved = |plan: &crate::provider::Plan,
                       res: &engine::EvalResult,
                       sections: &stuck::Sections,
                       tick: usize|
     -> Result<()> {
        let Some((path, saved)) = &saved else {
            return Ok(());
        };
        let report = report_of(plan, res, sections, tick, &[], &[]);
        let redact = query::Redactor::new(&res.facts, schema);
        let key = key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("internal: no plan key"))?;
        let now = zset::file::delta(plan, sections, &report, schema, &redact, key);
        let diff = saved.stale(&now, tick);
        if diff.is_empty() {
            return Ok(());
        }
        eprintln!(
            "plan file {} is stale: re-evaluation after refresh at tick {tick} does not reproduce its delta:",
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
                        key: &zset::file::Key,
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
                .map(|r| format!("{}.{}", r.addr.typ, r.addr.name))
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
                ..inputs
            },
            world_digest: zset::file::world_digest(&backend.world_facts(st)?),
            deformations,
            pending_groups: report
                .groups
                .iter()
                .map(|g| zset::file::Group {
                    pattern: g.pattern.clone(),
                    on: g.on.clone(),
                })
                .collect(),
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
            git_commits: pinned.clone(),
            needs_approval: crate::approval::needs(&res.facts)
                .into_iter()
                .map(|(deformation, reason)| zset::file::NeedsApproval {
                    deformation,
                    reason,
                })
                .collect(),
            digest: None,
        })
    };

    match cli.cmd.clone() {
        Cmd::Eval => {
            println!("facts: {}", res.facts.len());
            println!("resources: {}", resources.len());
            for r in &resources {
                println!("- {}.{}", r.addr.typ, r.addr.name);
            }
        }
        Cmd::Query { pattern, json } => {
            let mut res = explained(res);
            if strict {
                crate::lint::deny_collisions(&mut res, &collisions);
            }
            let redact = query::Redactor::new(&res.facts, backend.schema());
            print_query(&pattern, &res.facts, &redact, json)?;
        }
        Cmd::Why { pattern, all } => {
            let mut res = explained(res);
            if strict {
                crate::lint::deny_collisions(&mut res, &collisions);
            }
            let redact = query::Redactor::new(&res.facts, backend.schema());
            let query::Query::Body { body, .. } = query::parse(&pattern)? else {
                bail!("why: expected a fact pattern such as 'want(net.vpc, N)', got '{pattern}'");
            };
            let [crate::ast::Lit::Pos(pat)] = body.as_slice() else {
                bail!("why: expected one fact pattern, got '{pattern}'");
            };
            let matched = why::find(pat, &res.facts)?;
            if matched.is_empty() {
                bail!("why: no fact matches {pattern}");
            }
            let printer = why::Printer {
                circuit: &res.circuit,
                redact: &redact,
                all,
            };
            for (i, (a, focus)) in matched.iter().enumerate() {
                let Some(id) = res.circuit.fact_id(&engine::circuit_fact(a)) else {
                    bail!("internal: no provenance for {}", partition::fmt_atom(a));
                };
                if i > 0 {
                    println!();
                }
                print!("{}", printer.tree(id, focus.as_ref()));
            }
        }
        Cmd::Show { typ, name } => {
            let addr = ir::Address { typ, name };
            let Some(r) = resources.iter().find(|r| r.addr == addr) else {
                bail!("resource not found");
            };
            let json = serde_json::to_string_pretty(&redact.json(&r.attrs))?;
            println!("{}", json);
        }
        Cmd::Strata | Cmd::Test => unreachable!("handled before evaluation"),
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
                let location = base.child(i.segment().as_deref());
                crate::stack::Place {
                    world: crate::stack::world_file(&location, &i.dir(&root.join(&stack))),
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
        | Cmd::Controller { .. }
        | Cmd::Log { .. }
        | Cmd::StackList
        | Cmd::Handover { .. }
        | Cmd::Unlock
        | Cmd::StateShow
        | Cmd::StateMv { .. }
        | Cmd::ProviderCheck { .. }
        | Cmd::ProviderSchema { .. }
        | Cmd::Init { .. }
        | Cmd::Completions { .. }
        | Cmd::Complete { .. }
        | Cmd::Taint { .. } => {
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
            } = plan_for(res, &violations, resources, &adopts, &lifecycle, &st)?;
            let report = report_of(&plan, &res, &sections, 1, &moves, &denies);
            // The plan file, when one is written or the plan needs an
            // approval: its digest is what an approver signs.
            let needs = crate::approval::needs(&res.facts);
            let file = if out.is_some() || !needs.is_empty() {
                let loaded;
                let key = match &key {
                    Some(k) => k,
                    None => {
                        loaded = dep.plan_key()?;
                        &loaded
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
                j["key_defaults"] = serde_json::json!(instance.defaulted);
                if let Some(f) = &file {
                    j["needs_approval"] = serde_json::to_value(&f.needs_approval)?;
                    j["digest"] = serde_json::to_value(&f.digest)?;
                }
                println!("{}", serde_json::to_string_pretty(&j)?);
            } else {
                print!("{}", report.text());
                // What needs an approval, and the digest to approve; a
                // plan file's digest is on stderr beside its path.
                if let Some(f) = file.as_ref().filter(|f| !f.needs_approval.is_empty()) {
                    print!("{}", needs_text(&f.needs_approval));
                    println!("plan digest: {}", f.digest.as_deref().unwrap_or_default());
                }
            }
            blocked(&[violations, denies].concat())?;
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
                        "file": out.display().to_string(),
                        "inputs": file.inputs,
                        "git_commits": file.git_commits,
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
            for addr in chaos.addresses() {
                if !resources.iter().any(|r| &r.addr == addr) && st.get(addr).is_none() {
                    bail!(
                        "--chaos: {}/{} is not a resource of this stack",
                        addr.typ,
                        addr.name
                    );
                }
            }
            // One apply at a time per deployment; the lock is held until the
            // apply's end is in the audit log.
            *session = Some(Session {
                log: audit.clone(),
                _lock: dep.lock()?,
            });
            let key = key
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("internal: no plan key"))?;
            let inputs = inputs
                .clone()
                .ok_or_else(|| anyhow::anyhow!("internal: no plan inputs"))?;
            // Approvals (README "Approvals"): a token verifies against the
            // stack's trust root (loaded once), for this plan's digest and
            // this deployment, by an approver `approver_allowed` admits
            // when the program restricts them.
            let restricts = crate::approval::restricts_approvers(&program);
            let roots: std::cell::OnceCell<Vec<_>> = std::cell::OnceCell::new();
            let verify_token = |token: &str,
                                needs: &[(String, String)],
                                digest: &str,
                                facts: &BTreeSet<Atom>|
             -> Result<crate::approval::Verified> {
                if stack_cfg.approvals.is_empty() {
                    bail!(
                        "stack {deployment} has no approvals trust root \
                         (`stack ... {{ approvals = jwks(\"https://...\") }}`)"
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
            persist_externs(&mut st, &externs);
            let persist = |st: &state::State| dep.save_state(st);
            // Nothing is written, to state or the world, until the apply is
            // confirmed: the moves, the resolution of uncertain calls and the
            // in-flight record taken here are written with the tick's first.
            if !moves.is_empty() {
                print_moves(&moves);
            }
            let resumed = st.in_flight.take();
            if let Some(f) = &resumed {
                let names: Vec<String> = f
                    .remaining
                    .keys()
                    .filter_map(|k| state::parse_key(k))
                    .map(|a| format!("{}.{}", a.typ, a.name))
                    .collect();
                println!(
                    "resuming the apply interrupted at tick {}; remaining: {}",
                    f.tick,
                    names.join(", ")
                );
                // The remaining deformations come back as facts with the
                // documents they were planned against, as the held ones do
                // at a boundary: the evaluator derives the deny when the
                // world moved under one (`zset::POLICY_RULES`).
                let remaining = executor::remaining(f);
                let observed = backend.observe(&st)?;
                let changed = executor::changed_under(&backend, &remaining, &observed);
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
                if !denies.is_empty() {
                    let redact = query::Redactor::new(&after.facts, backend.schema());
                    eprintln!("constraint violations:");
                    for d in &denies {
                        eprintln!("- {}", redact.text(d));
                    }
                    persist(&st)?;
                    bail!(
                        "apply stopped: blocked by constraints on the remaining actions of the \
                         interrupted apply; review `dform plan`, then apply again"
                    );
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
            loop {
                let Planned {
                    res: r,
                    resources: docs,
                    mut plan,
                    sections,
                    denies,
                } = plan_for(res, &violations, resources, &adopts, &lifecycle, &st)?;
                (res, resources) = (r, docs);
                check_saved(&plan, &res, &sections, tick)?;
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
                            "file": saved.as_ref().map(|(p, _)| p.display().to_string()),
                            "inputs": inputs,
                            "git_commits": pinned,
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
                        .is_some_and(|l| l.ends_with(" is undeformed"));
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
                    if tick > 1 || boundary {
                        println!("tick {tick}:");
                    }
                    show(&plan, &res, &sections, tick, &[], &denies);
                }
                if !denies.is_empty() {
                    let redact = query::Redactor::new(&res.facts, backend.schema());
                    eprintln!("constraint violations:");
                    for d in denies {
                        eprintln!("- {}", redact.text(&d));
                    }
                    bail!("apply stopped at tick {tick}: blocked by constraints");
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
                        let n = report.deformations() + report.pending_count();
                        confirm(n, &deployment)?;
                    }
                }
                if hook.is_none() {
                    approve_entry(
                        tick,
                        &needs,
                        digest.as_deref(),
                        approval.as_deref(),
                        saved.is_some(),
                        &mut approved,
                        &audit,
                        &|t: &str, d: &str| verify_token(t, &needs, d, &res.facts),
                        &|w: &str, d: &str| {
                            !restricts || crate::approval::approver_allowed(&res.facts, w, d)
                        },
                    )?;
                }
                if tick == 1 {
                    audit.append(
                        "apply_start",
                        serde_json::json!({
                            "who": crate::audit::who(),
                            "dform": env!("CARGO_PKG_VERSION"),
                            "commit": crate::project::git_head(
                                files[0].parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")),
                            ),
                            "providers": if providers.is_empty() {
                                vec!["fake".to_string()]
                            } else {
                                providers.clone()
                            },
                            "protocol": crate::plugin::backend::VERSION,
                        }),
                    )?;
                }
                let observed = backend.observe(&st)?;
                executor::begin(&mut st, tick, &plan, &observed);
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
                // when there is nothing to do. State is written after every
                // Apply call (`executor`).
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
                    let on_action = |a: &crate::provider::Action,
                                     err: Option<&anyhow::Error>,
                                     st: &state::State| {
                        let mut e = serde_json::json!({
                            "tick": tick,
                            "action": zset::deformation_kind(&a.kind, false).unwrap_or("no-op"),
                            "address": format!("{}.{}", a.addr.typ, a.addr.name),
                            "result": if err.is_some() { "failed" } else { "ok" },
                            "remote": st.get(&a.addr).map(|e| e.remote.clone()),
                            "diff": diffs.get(&a.addr),
                        });
                        if let Some(err) = err {
                            e["error"] = redact.text(&format!("{err:#}")).into();
                        }
                        if let Err(x) = audit.append("action", e) {
                            audit_failed.borrow_mut().get_or_insert(x);
                        }
                    };
                    let fence = || dep.check_fence();
                    let opts = executor::Options {
                        parallel: parallel as usize,
                        persist: &persist,
                        stop_after: stop_after.as_ref(),
                        on_action: Some(&on_action),
                        before_submit: Some(&fence),
                    };
                    let applied = executor::run_tick(
                        &backend, &resources, &adopts, &lifecycle, &mut st, &plan, &opts,
                    );
                    for note in backend.take_notes() {
                        println!("chaos: {note}");
                    }
                    if let Some(e) = audit_failed.into_inner() {
                        return Err(e);
                    }
                    seen.extend(applied?);
                    // The world as the executor saw it, keyed like a
                    // secret: a document may hold one.
                    let world: serde_json::Map<String, serde_json::Value> = seen
                        .iter()
                        .map(|(a, d)| (state::key(a), d.clone().unwrap_or_default()))
                        .collect();
                    let canonical = crate::approval::canonical_json(&world.into());
                    audit.append(
                        "tick",
                        serde_json::json!({
                            "tick": tick,
                            "world": format!("hmac-sha256:{}", key.digest(canonical.as_bytes())),
                        }),
                    )?;
                }
                if !boundary {
                    st.in_flight = None;
                    // The stack's outputs, as the world now is, for other
                    // stacks to read (evaluated again only when it has any:
                    // the evaluation refreshes). A --world fixture is not
                    // registered: everything stays beside the world file.
                    persist_externs(&mut st, &externs);
                    crate::tables::record(&mut st.externs, &externs.recorded());
                    st.outputs = if crate::stack::has_outputs(&res.facts) {
                        crate::stack::outputs(&evaluate(&st)?.0.facts)
                    } else {
                        Default::default()
                    };
                    persist(&st)?;
                    // Published beside the state, apart from it: what
                    // other stacks read (a secret output by its label).
                    let secret_types = crate::stack::secret_output_types(&program);
                    if cli.world.is_none()
                        && (!st.outputs.is_empty()
                            || !secret_types.is_empty()
                            || dep.store().get(store::OUTPUTS)?.is_some())
                    {
                        let published =
                            crate::stack::Published::new(&deployment, &st.outputs, &secret_types);
                        dep.publish(&published.bytes())?;
                    }
                    // Every deployment of a keyed stack is registered.
                    let keyed = instance.segment().is_some();
                    if (!st.outputs.is_empty()
                        || !secret_types.is_empty()
                        || stack_cfg.bootstrap
                        || keyed)
                        && cli.world.is_none()
                    {
                        crate::stack::register(&root, &deployment, &location, stack_cfg.bootstrap)?;
                    }
                    if let Some(h) = hook.as_deref_mut() {
                        h.finish(&deployment, undeformed, &backend.observe(&st)?)?;
                    } else if changed || tick > 1 {
                        println!("apply: complete");
                    } else {
                        println!("apply: nothing to do");
                    }
                    break;
                }
                if !changed && let Some(h) = hook.as_deref_mut() {
                    // Everything definite is held: wait for the next event.
                    st.in_flight = None;
                    persist(&st)?;
                    h.finish(&deployment, false, &backend.observe(&st)?)?;
                    break;
                }
                if !changed {
                    let mut waits: Vec<String> = sections.blocking.iter().cloned().collect();
                    waits.extend(held);
                    waits.sort();
                    waits.dedup();
                    let waits: Vec<String> = waits.iter().map(|n| format!("?{n}")).collect();
                    bail!(
                        "apply stopped at tick {tick}: nothing definite to apply, still waiting on {}",
                        waits.join(" ")
                    );
                }
                if tick == max_ticks {
                    bail!(
                        "apply stopped after {max_ticks} ticks (--max-ticks): the stack is still deformed"
                    );
                }
                // The boundary. The held deformations come back as facts
                // with the documents they were planned against: the
                // evaluator derives the deny when the world moved under one.
                let held = executor::check_boundary(&backend, &seen, &pending, &st, tick)?;
                let (next, next_violations) =
                    evaluate_with(&st, &BTreeSet::new(), &held, Some(tick))?;
                violations = next_violations;
                let redact = query::Redactor::new(&next.facts, backend.schema());
                for w in &next.warnings {
                    eprintln!("warning: {}", redact.text(w));
                }
                if !violations.is_empty() {
                    eprintln!("constraint violations after tick {tick}:");
                    for v in &violations {
                        eprintln!("- {}", redact.text(v));
                    }
                    bail!("apply stopped after tick {tick}: blocked by constraints");
                }
                resources = ir::compile_resources(next.facts.iter().cloned(), backend.schema())?;
                adopts = ir::compile_adopts(next.facts.iter())?;
                lifecycle = zset::Lifecycle::from_facts(&next.facts, backend.schema())?;
                res = next;
                tick += 1;
            }
        }
    }

    Ok(())
}

/// `dform controller run`: a run per event (`controller::Hook`), until
/// `--once` or `--max-events` says stop. A run that fails is logged and the
/// controller goes on watching; the first one failing ends it.
fn run_controller(cli: Cli) -> Result<()> {
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
        if once || max_events.is_some_and(|n| events >= n) {
            return Ok(());
        }
        while !hook.changed() {
            std::thread::sleep(std::time::Duration::from_millis(poll));
        }
    }
}

/// Ask on the terminal whether to apply `n` deformations to `deployment`:
/// only `y` or `yes` proceeds. With no terminal to ask on, a refusal naming
/// `--yes`, never a wait.
fn confirm(n: usize, deployment: &str) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!(
            "apply {deployment}: nothing to ask on (stdin is not a terminal); \
             pass --yes to apply without asking"
        );
    }
    let s = if n == 1 { "" } else { "s" };
    print!("Apply these {n} deformation{s} to {deployment}? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(Declined(deployment.to_string()).into()),
    }
}

/// An apply its confirmation declined: the audit log's `apply_end` says
/// `declined`.
#[derive(Debug)]
struct Declined(String);

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "apply {}: not confirmed; nothing was applied", self.0)
    }
}

impl std::error::Error for Declined {}

/// `stack rekey`: the deployment whose state moves, and where to.
struct Rekey {
    from: crate::stack::Instance,
    to: crate::stack::Instance,
}

/// `stack rekey STACK K=V...`: STACK is the program's stack, and keyed; the
/// pairs are the old key, then the new one, each naming every key input
/// once (the old one left out: the state from before the stack was keyed).
/// The run is set to the old values (the new ones for the unkeyed state),
/// so it evaluates the deployment whose state moves.
fn rekey_args(
    cli: &mut Cli,
    cfg: &crate::stack::Stack,
    files: &[PathBuf],
    stack: &str,
    pairs: &[String],
) -> Result<Rekey> {
    let own = cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    if stack != own {
        bail!(
            "stack rekey {stack}: the program ({}) owns stack {own}",
            files[0].display()
        );
    }
    if cli.world.is_some() {
        bail!(
            "stack rekey {stack}: with --world the state sits beside the world file; \
             there is nothing to move"
        );
    }
    let keys: Vec<&str> = cfg.keys.iter().map(|(k, _)| k.as_str()).collect();
    if keys.is_empty() {
        bail!(
            "stack rekey {stack}: the stack has no key; key it first (`stack {stack}[env] {{ .. }}`)"
        );
    }
    let side = |pairs: &[String]| -> Result<Vec<(String, String)>> {
        for p in pairs {
            let Some((k, _)) = p.split_once('=') else {
                bail!("stack rekey {stack}: expected K=V, got '{p}'");
            };
            if !keys.contains(&k) {
                bail!(
                    "stack rekey {stack}: {k} is not a key of the stack (its key: {})",
                    keys.join(", ")
                );
            }
        }
        keys.iter()
            .map(|k| {
                let vs: Vec<&str> = pairs
                    .iter()
                    .filter_map(|p| p.split_once('='))
                    .filter(|(x, _)| x == k)
                    .map(|(_, v)| v)
                    .collect();
                match vs.as_slice() {
                    [v] => Ok((k.to_string(), v.to_string())),
                    [] => bail!("stack rekey {stack}: no value for the key input {k}"),
                    _ => bail!("stack rekey {stack}: {k} is given twice on one side"),
                }
            })
            .collect()
    };
    let n = keys.len();
    let (from, to) = if pairs.len() == n {
        (Vec::new(), side(pairs)?)
    } else if pairs.len() == 2 * n {
        (side(&pairs[..n])?, side(&pairs[n..])?)
    } else {
        bail!(
            "stack rekey {stack} K=V...: the old key, then the new one, each naming {}",
            keys.join(", ")
        );
    };
    if from == to {
        bail!("stack rekey {stack}: the old key and the new one are the same");
    }
    let run_as = if from.is_empty() { &to } else { &from };
    cli.set
        .retain(|kv| !kv.split_once('=').is_some_and(|(x, _)| keys.contains(&x)));
    cli.set
        .extend(run_as.iter().map(|(k, v)| format!("{k}={v}")));
    let instance = |key| crate::stack::Instance {
        stack: own.clone(),
        key,
        defaulted: Vec::new(),
    };
    Ok(Rekey {
        from: instance(from),
        to: instance(to),
    })
}

/// The keys of the program's own `input("k", v)` facts.
fn input_fact_keys(program: &crate::ast::Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            crate::ast::Stmt::Fact(a) if a.pred == "input" => match a.args.first() {
                Some(Term::Val(Value::Str(k))) => Some(k.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `dform test`: every scenario evaluated against an empty mock world (the
/// provider's schema, no world, no state); a scenario fails when anything
/// is denied or it does not compile.
fn run_tests(
    program: &crate::ast::Program,
    providers: &[String],
    cli: &Cli,
    files: &[PathBuf],
) -> Result<()> {
    let (set, data) = (&cli.set, &cli.data);
    use std::io::IsTerminal;
    let names = crate::scenario::names(program)?;
    if names.is_empty() {
        bail!("no scenarios: write `scenario NAME {{ with k = v; deny rules }}`");
    }
    let backend = Providers::start(launch(), providers, &plugin::Config::default())?;
    let program_dir = crate::project::base_of(&files[0]);
    let mut failed = 0;
    for name in &names {
        let run = || -> Result<Vec<String>> {
            let p = crate::scenario::select(program, name)?;
            let lowered = crate::transform::lower(&p)?;
            crate::secrets::check(&lowered, backend.schema(), &Default::default())?;
            crate::refine::check(&lowered.program, backend.schema())?;
            let mut given = input_fact_keys(&p);
            let mut pairs = Vec::new();
            for kv in set {
                let (k, v) = split_kv(kv)?;
                given.insert(k.to_string());
                pairs.push((k.to_string(), v));
            }
            inputs::check_required(&lowered.inputs, &given)?;
            let mut extra = inputs::set_facts(&lowered.inputs, &pairs)?;
            extra.extend(cli.manifest.iter().flat_map(|m| m.facts()));
            extra.extend(build_extra_facts(data)?);
            extra.extend(backend.catalog(schema::named_types(&lowered.program, &extra).as_ref())?);
            let tables = crate::tables::Tables::default();
            let externs =
                crate::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
                    if let Some(r) = tables.answer(f, ins) {
                        return r;
                    }
                    if let Some(r) = crate::externs::file(f, ins, &program_dir) {
                        return r;
                    }
                    let plus: Vec<bool> = f.args.iter().map(|b| b.input).collect();
                    backend.query(&f.name, &plus, ins)
                });
            let p = zset::with_policy_rules(p)?;
            let (res, mut violations) = externs.eval(&p, &extra)?;
            violations.extend(inputs::violations(&res.facts, &lowered.inputs));
            let redact = query::Redactor::new(&res.facts, backend.schema());
            Ok(violations.iter().map(|v| redact.text(v)).collect())
        };
        match run() {
            Ok(denied) if denied.is_empty() => println!("scenario {name}: ok"),
            Ok(denied) => {
                failed += 1;
                println!("scenario {name}: denied");
                for d in denied {
                    println!("  - {d}");
                }
            }
            Err(e) => {
                failed += 1;
                println!("scenario {name}: error");
                let text = crate::diag::report(&e, std::io::stdout().is_terminal());
                for line in text.lines() {
                    println!("  {line}");
                }
            }
        }
    }
    println!("test: {} scenarios, {failed} failed", names.len());
    if failed > 0 {
        bail!("{failed} of {} scenarios failed", names.len());
    }
    Ok(())
}

/// Keep the answers of `persist` externs in state: never asked again.
/// `dform state taint STACK EXTERN ARGS...`: remove the answer from the stack's
/// state (beside `--world`, else where the registry has it, else where its
/// program's backend says), under the stack's lock.
fn taint(cli: &Cli, stack: &str, pred: &str, args: &[String]) -> Result<()> {
    let dep = match &cli.world {
        Some(w) => store::Deployment::local(&state::world_paths(&cli.root, w).state, stack),
        None => {
            let (place, _, times) = place_of(cli, stack)?;
            store::Deployment::new(
                place.location.open(&open_s3(&cli.root, true))?,
                stack,
                times,
            )
        }
    };
    let call = format!("{pred}({})", args.join(", "));
    if !dep.has_state()? {
        bail!(
            "taint {call}: stack {stack} has no state at {}",
            dep.locate(store::STATE)
        );
    }
    let _lock = dep.lock()?;
    let mut st = dep.load_state()?;
    if st.taint(pred, args).is_none() {
        bail!("taint {call}: stack {stack} has no persisted answer for it");
    }
    dep.save_state(&st)?;
    println!("tainted {call} of stack {stack}: the next plan asks again");
    Ok(())
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

/// Where a stack's deployments are: its backend's location, else its
/// directory under the state root.
fn stack_location(
    root: &Path,
    stack: &str,
    backend: Option<&crate::stack::Backend>,
) -> store::Location {
    match backend {
        Some(crate::stack::Backend::Local(dir)) => {
            store::Location::Local(state::local_dir(root, dir))
        }
        Some(crate::stack::Backend::S3(spec)) => store::Location::S3(spec.clone()),
        None => store::Location::Local(root.join(stack)),
    }
}

/// The backend of the project's stack `stack`, as its program (over the
/// manifest's default) says; `None` when it says none, or there is no
/// such stack.
fn stack_backend(
    project: &crate::project::Project,
    found: &crate::project::Found,
) -> Option<crate::stack::Backend> {
    let program = loader::load_program(std::slice::from_ref(&found.file)).ok()?;
    let mut cfg = crate::stack::config(&program).ok()?;
    with_manifest(&mut cfg, &project.manifest, &found.file);
    cfg.backend
}

/// Where the deployment `name` (`app`, `app[env=prod]`) is, for a command
/// that runs no program: where the registry has it, else where its stack's
/// program's backend says, else under the state root. With its default
/// directory (its world's when its state is in a bucket) and the lease
/// times.
fn place_of(cli: &Cli, name: &str) -> Result<(crate::stack::Place, PathBuf, store::LeaseTimes)> {
    let root = &cli.root;
    let home = crate::stack::instance_dir(root, name);
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let times = project
        .as_ref()
        .map(|p| p.manifest.lease_times())
        .unwrap_or_default();
    let location = match crate::stack::registry(root)?.remove(name) {
        Some(e) => e.state,
        None => {
            let (stack, seg) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((stack, seg)) => (stack, Some(seg)),
                None => (name, None),
            };
            let backend = project.as_ref().and_then(|p| {
                let d = crate::project::discover(p);
                match d.named(stack).as_slice() {
                    [one] => stack_backend(p, one),
                    _ => None,
                }
            });
            stack_location(root, stack, backend.as_ref()).child(seg)
        }
    };
    let place = crate::stack::Place {
        world: crate::stack::world_file(&location, &home),
        location,
    };
    Ok((place, home, times))
}

fn persist_externs(st: &mut state::State, externs: &crate::externs::Externs) {
    for a in externs.persisted() {
        if !st
            .externs
            .iter()
            .any(|b| b.pred == a.pred && b.inputs == a.inputs)
        {
            st.externs.push(a);
        }
    }
}

/// A plan and the evaluation that sees it (`plan_for` in `main`).
struct Planned {
    /// The policy pass: the program with the plan's deformations as facts.
    res: engine::EvalResult,
    /// The documents the plan was taken from.
    resources: Vec<ir::Resource>,
    plan: crate::provider::Plan,
    sections: stuck::Sections,
    /// Denies over the plan: what the policy pass derives beyond the plan's
    /// own evaluation (`lifecycle prevent_destroy`, a policy on
    /// `deformation/4`).
    denies: Vec<String>,
}

/// A batch apply's approval, before its Apply calls: at tick 1 the token
/// given (`--approval FILE`), verified (`verify`), or, with none, a
/// refusal if anything needs one; at a later tick, a new deformation that
/// needs one must be one the approver may approve (`allowed`). Each
/// verdict at tick 1 goes to the audit log.
#[allow(clippy::too_many_arguments)]
fn approve_entry(
    tick: usize,
    needs: &[(String, String)],
    digest: Option<&str>,
    token: Option<&Path>,
    from_file: bool,
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
            return audit.append("approval", serde_json::json!({ "result": "not required" }));
        }
        let error = format!("{} an approval, and no --approval was given", list(needs));
        audit.append(
            "approval",
            serde_json::json!({ "result": "refused", "digest": digest, "error": error }),
        )?;
        let how = if from_file {
            "apply it with --approval FILE, a signed approval of that digest"
        } else {
            "write the plan with `plan --out PLAN`, have its digest approved, and \
             `apply PLAN --approval FILE`"
        };
        bail!("apply refused: {error}; the plan's digest is {digest}: {how}");
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
            audit.append(
                "approval",
                serde_json::json!({ "result": "refused", "digest": digest, "error": e.to_string() }),
            )?;
            bail!("apply refused: {e}")
        }
    }
}

/// The `needs approval:` section of a plan: each deformation and why.
fn needs_text(needs: &[zset::file::NeedsApproval]) -> String {
    if needs.is_empty() {
        return String::new();
    }
    let mut out = String::from("needs approval:\n");
    for n in needs {
        out.push_str(&format!("  {}  ({})\n", n.deformation, n.reason));
    }
    out
}

/// The commit each `git` input relation's ref names now, but the
/// `approval` relation's: its tokens approve a plan, they are not part of
/// it.
fn pinned_commits(relations: &[watch::Relation]) -> Vec<zset::file::Pinned> {
    let mut out: Vec<zset::file::Pinned> = relations
        .iter()
        .filter(|r| r.pred != "approval" && matches!(r.source, watch::Source::Git { .. }))
        .map(|r| zset::file::Pinned {
            source: r.source.to_string(),
            commit: watch::stamp(&r.source),
        })
        .collect();
    out.sort_by(|a, b| a.source.cmp(&b.source));
    out.dedup();
    out
}

/// `dform log [verify]`: the deployment's audit log.
fn print_log(
    log: &crate::audit::Log,
    deployment: &str,
    verify: bool,
    since: Option<&str>,
    json: bool,
) -> Result<()> {
    let at = log.locate();
    if verify {
        let Some(text) = log.text()? else {
            bail!("stack {deployment} has no audit log at {at}");
        };
        return match crate::audit::verify(&text) {
            (n, None) => {
                println!("audit log {at}: {n} entries, the chain holds");
                Ok(())
            }
            (_, Some(b)) => bail!("audit log {at}: {}", b.why),
        };
    }
    let mut entries = log.entries()?;
    if let Some(s) = since {
        entries = crate::audit::since(entries, s);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for e in &entries {
            println!("{}", crate::audit::line(e));
        }
    }
    Ok(())
}

/// `moved/3` rewrites applied to state before the plan.
fn print_moves(moves: &[(ir::Address, ir::Address)]) {
    print!("{}", plan_print::moved_text(moves));
}

/// `dform dev strata`: the partition graph's strata, or the negative cycle.
fn print_strata(
    files: &[PathBuf],
    program: &crate::ast::Program,
    schema: &schema::Schema,
) -> Result<()> {
    let graph = partition::build(program, schema)?;
    let verdict = partition::stratify(&graph);
    let name = files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    print!("{}", format_strata(&name, &graph, &verdict));
    if let partition::Verdict::Rejected {
        scc,
        negative_edges,
    } = &verdict
    {
        bail!("{}", partition::cycle_error(&graph, scc, negative_edges));
    }
    Ok(())
}

/// A stable, multi-line rendering of the stratified case (one node per
/// line, sorted) so `dform dev strata` can be pinned as a golden snapshot.
/// Rejected (negative cycle) keeps `partition::report`'s own format.
fn format_strata(name: &str, g: &partition::Graph, v: &partition::Verdict) -> String {
    match v {
        partition::Verdict::Stratified { strata } => {
            let mut out = String::new();
            out.push_str(&format!(
                "== {name}: {} nodes, {} edges ({} negative)\n",
                g.nodes.len(),
                g.edges.len(),
                g.edges.iter().filter(|e| e.negative).count()
            ));
            let max = strata.values().copied().max().unwrap_or(0);
            out.push_str(&format!("   STRATIFIED, {} strata\n", max + 1));
            let mut by: std::collections::BTreeMap<usize, Vec<String>> =
                std::collections::BTreeMap::new();
            for (n, s) in strata {
                by.entry(*s).or_default().push(n.to_string());
            }
            for (s, mut ns) in by {
                ns.sort();
                out.push_str(&format!("   stratum {s}:\n"));
                for n in ns {
                    out.push_str(&format!("     {n}\n"));
                }
            }
            out
        }
        partition::Verdict::Rejected { .. } => partition::report(name, g, v),
    }
}

/// The types whose schema facts a run injects (`Providers::catalog`):
/// those the program, the facts given to it and state name; `None` (all of
/// them) for a `query` or `why` of a schema predicate, or a program that
/// reads the schema of a type it does not name.
fn catalog_scope(
    cmd: &Cmd,
    program: &crate::ast::Program,
    given: &[Atom],
    discovered: &[Atom],
    st: &state::State,
) -> Option<BTreeSet<String>> {
    if let Cmd::Query { pattern, .. } | Cmd::Why { pattern, .. } = cmd {
        let reads_schema = match query::parse(pattern).ok()? {
            query::Query::Pred(p) => schema::is_schema_pred(&p),
            query::Query::Body { body, .. } => body.iter().any(|l| {
                matches!(l, crate::ast::Lit::Pos(a) | crate::ast::Lit::Not(a)
                    if schema::is_schema_pred(&a.pred))
            }),
        };
        if reads_schema {
            return None;
        }
    }
    let lowered = crate::transform::lower(program).ok()?;
    let facts: Vec<Atom> = given.iter().chain(discovered).cloned().collect();
    let mut named = schema::named_types(&lowered.program, &facts)?;
    named.extend(
        st.resources
            .keys()
            .chain(st.deposed.keys())
            .chain(st.uncertain.keys())
            .filter_map(|k| state::parse_key(k).map(|a| a.typ)),
    );
    Some(named)
}

/// E §2.7's sections for this evaluation: what waits on a boundary.
fn plan_sections(
    res: &engine::EvalResult,
    resources: &[ir::Resource],
    schema: &schema::Schema,
) -> stuck::Sections {
    let docs = resources
        .iter()
        .map(|r| ((r.addr.typ.clone(), r.addr.name.clone()), r.attrs.clone()))
        .collect();
    stuck::sections(&res.stuck, &res.may_derive, &res.facts, &docs, schema)
}

/// `query`'s output, redacted: every fact of a predicate, or a table with
/// one column per variable; `--json` prints one document either way.
fn print_query(
    pattern: &str,
    facts: &std::collections::BTreeSet<Atom>,
    redact: &query::Redactor,
    json: bool,
) -> Result<()> {
    match query::parse(pattern)? {
        query::Query::Pred(pred) => {
            let matches: Vec<&Atom> = facts.iter().filter(|a| a.pred == pred).collect();
            if json {
                let doc = serde_json::json!({
                    "query": pattern,
                    "count": matches.len(),
                    "facts": matches.iter().map(|a| serde_json::json!({
                        "pred": a.pred,
                        "args": a.args.iter().map(|t| match t {
                            Term::Val(v) => redact.json(v),
                            t => serde_json::Value::String(partition::fmt_term(t)),
                        }).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&doc)?);
                return Ok(());
            }
            for a in &matches {
                println!("{}", redact.fmt_atom(a));
            }
            println!("matches: {}", matches.len());
        }
        query::Query::Body { body, vars } => {
            let table = query::table(&body, &vars, facts)?;
            if json {
                let doc = serde_json::json!({
                    "query": pattern,
                    "columns": table.vars,
                    "count": table.rows.len(),
                    "rows": table.json(redact),
                });
                println!("{}", serde_json::to_string_pretty(&doc)?);
                return Ok(());
            }
            print!("{}", table.render(redact));
        }
    }
    Ok(())
}

/// The types the program reads the inventory of (`world.T[e].p`,
/// `x in world.T`: `cloud_exists`, `cloud_attr`, `cloud_computed` with a
/// constant type), for discovery to ask about only those. `None` (every
/// type) for a query or why, which may ask about any, and for a program
/// whose type there is not a constant or that does not lower.
fn world_types(cmd: &Cmd, lowered: Option<&crate::transform::Lowered>) -> Option<BTreeSet<String>> {
    use crate::ast::{Lit, Stmt, Term};
    if matches!(cmd, Cmd::Query { .. } | Cmd::Why { .. }) {
        return None;
    }
    let mut out = BTreeSet::new();
    for st in &lowered?.program.statements {
        let body = match st {
            Stmt::Rule(r) => &r.body,
            Stmt::Constraint(c) => &c.body,
            _ => continue,
        };
        for l in body {
            let (Lit::Pos(a) | Lit::Not(a)) = l else {
                continue;
            };
            if !plugin::providers::INVENTORY
                .iter()
                .any(|(p, _)| *p == a.pred)
            {
                continue;
            }
            match a.args.first() {
                Some(Term::Val(crate::value::Value::Str(t))) => {
                    out.insert(t.clone());
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

/// The providers the program configures itself: the constant names of
/// its `provider_config(Name, Settings)` facts and rules.
fn provider_configs(program: &crate::ast::Program) -> BTreeSet<String> {
    use crate::ast::{Stmt, Term};
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Fact(a) => Some(a),
            Stmt::Rule(r) => Some(&r.head),
            _ => None,
        })
        .filter(|a| a.pred == "provider_config")
        .filter_map(|a| match a.args.first() {
            Some(Term::Val(crate::value::Value::Str(n))) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// The environment variables `env_var` reads, by label (`env_var/NAME`),
/// as a plan file records them: the label and the value's digest keyed
/// with the stack's plan key, as a secret input's. One not set now is
/// left out.
fn env_inputs(
    labels: impl Iterator<Item = String>,
    key: &zset::file::Key,
) -> Vec<serde_json::Value> {
    labels
        .filter_map(|label| {
            let name = label.strip_prefix("env_var/")?;
            let v = std::env::var(name).ok()?;
            Some(serde_json::json!({ "sensitive": label, "digest": key.digest(v.as_bytes()) }))
        })
        .collect()
}

/// This run's inputs as a plan file records them: each `--input-file` and
/// a `--set` of a secret input by their digest with the stack's key (the
/// latter with its label).
fn plan_inputs(
    cli: &Cli,
    files: &[PathBuf],
    secret: &BTreeSet<String>,
    key: &zset::file::Key,
) -> Result<zset::file::Inputs> {
    let read =
        |f: &PathBuf| std::fs::read(f).map_err(|e| anyhow::anyhow!("read {}: {e}", f.display()));
    let show = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
    Ok(zset::file::Inputs {
        files: files
            .iter()
            .map(|f| {
                Ok(zset::file::FileDigest {
                    path: f.display().to_string(),
                    fnv64: zset::file::fnv64(&read(f)?),
                })
            })
            .collect::<Result<_>>()?,
        input_files: cli
            .input_files
            .iter()
            .map(|f| {
                Ok(zset::file::KeyedDigest {
                    path: f.display().to_string(),
                    digest: key.digest(&read(f)?),
                })
            })
            .collect::<Result<_>>()?,
        set: cli
            .set
            .iter()
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) if secret.contains(k) => {
                    let label = crate::value::null_label(crate::modules::INPUT, "", k);
                    serde_json::json!({ "sensitive": label, "digest": key.digest(v.as_bytes()) })
                }
                _ => serde_json::Value::String(kv.clone()),
            })
            .collect(),
        data: cli.data.clone(),
        providers: cli.providers.clone(),
        world: show(&cli.world),
        inventory: show(&cli.inventory),
        env: Vec::new(),
        stack_outputs: Vec::new(),
    })
}

/// Load a plan file for `apply PLAN`; its inputs fill every input flag the
/// command line leaves out.
fn with_plan_inputs(cli: &mut Cli, path: &Path) -> Result<zset::file::PlanFile> {
    let saved = zset::file::PlanFile::load(path)?;
    let i = &saved.inputs;
    if cli.files.is_empty() {
        cli.files = i.files.iter().map(|f| PathBuf::from(&f.path)).collect();
    }
    if cli.set.is_empty() {
        for s in &i.set {
            match s {
                serde_json::Value::String(kv) => cli.set.push(kv.clone()),
                secret => {
                    let label = secret["sensitive"].as_str().unwrap_or_default();
                    let k = label.rsplit_once('#').map_or(label, |(_, k)| k);
                    bail!(
                        "plan file {}: input {k} is secret and the file holds only its digest; \
                         give every --set again (--set {k}=...)",
                        path.display()
                    );
                }
            }
        }
    }
    if cli.input_files.is_empty() {
        cli.input_files = i
            .input_files
            .iter()
            .map(|f| PathBuf::from(&f.path))
            .collect();
    }
    if cli.data.is_empty() {
        cli.data = i.data.clone();
    }
    if cli.providers.is_empty() {
        cli.providers = i.providers.clone();
    }
    if cli.world.is_none() {
        cli.world = i.world.as_ref().map(PathBuf::from);
    }
    if cli.inventory.is_none() {
        cli.inventory = i.inventory.as_ref().map(PathBuf::from);
    }
    Ok(saved)
}

/// The providers' schema: what `strata` and `graph strata` read, with no
/// world.
fn load_schema(providers: &[String]) -> Result<schema::Schema> {
    Ok(
        Providers::start(launch(), providers, &plugin::Config::default())?
            .schema()
            .clone(),
    )
}

/// `--inventory PATH`, else `<world dir>/inventory.json` when `--world` is
/// given and that file exists, else the stack's default (`dform.state/inventory.json`).
fn resolve_inventory(
    explicit: &Option<PathBuf>,
    world: &Option<PathBuf>,
    default: &Path,
) -> PathBuf {
    if let Some(p) = explicit {
        return p.clone();
    }
    if let Some(w) = world {
        let dir = w.parent().unwrap_or_else(|| Path::new("."));
        let candidate = dir.join("inventory.json");
        if candidate.exists() {
            return candidate;
        }
    }
    default.to_path_buf()
}

/// `dform fmt`: rewrite each file in its formatted form, or with `check`
/// list the files that are not and fail.
fn fmt_files(paths: &[PathBuf], check: bool) -> Result<()> {
    let mut unformatted = Vec::new();
    for p in paths {
        let src =
            std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("read {}: {e}", p.display()))?;
        let out = crate::fmt::format_source(&p.display().to_string(), &src)?;
        if out == src {
            continue;
        }
        if check {
            println!("{}", p.display());
            unformatted.push(p);
        } else {
            std::fs::write(p, out).map_err(|e| anyhow::anyhow!("write {}: {e}", p.display()))?;
        }
    }
    if !unformatted.is_empty() {
        bail!("{} file(s) not formatted", unformatted.len());
    }
    Ok(())
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
        | Cmd::StateShow
        | Cmd::StateMv { .. }
        | Cmd::Taint { .. } => true,
        _ => false,
    }
}

/// The manifest's `unknowns` default, said in the program's stack
/// statement when it does not say its own: strict mode is the program's
/// (`transform::STRICT_RULES`).
fn with_default_unknowns(program: &mut crate::ast::Program, m: &crate::project::Manifest) {
    let Some(u) = &m.defaults.unknowns else {
        return;
    };
    for s in &mut program.statements {
        if let crate::ast::Stmt::Stack(c) = s
            && !c.config.iter().any(|(k, _, _)| k == "unknowns")
        {
            c.config
                .push(("unknowns".into(), Term::Val(Value::Str(u.clone())), c.span));
        }
    }
}

/// The manifest under the program's own statements: a provider named
/// without a `source` takes the manifest's entry of that name, and a stack
/// statement that does not say its backend takes the manifest's default.
fn with_manifest(cfg: &mut crate::stack::Stack, m: &crate::project::Manifest, file: &Path) {
    for p in &mut cfg.providers {
        if !p.contains('/')
            && let Some(src) = m.provider_source(p)
        {
            *p = src;
        }
    }
    let name = cfg.name.clone().unwrap_or_else(|| state::stack_name(file));
    if cfg.backend.is_none() {
        cfg.backend = m.backend(&name);
    }
}

/// A key input's value is the target's: `--set` of one is an error, as is
/// a target key the stack does not have. A key the target does not name is
/// its input's default, for `plan` and `apply` alike (the controller names
/// every one: `run_controller`).
fn check_keys(cli: &Cli, cfg: &crate::stack::Stack, stack: &str) -> Result<()> {
    let keys: Vec<&str> = cfg.keys.iter().map(|(k, _)| k.as_str()).collect();
    for kv in &cli.user_set {
        if let Some((k, _)) = kv.split_once('=')
            && keys.contains(&k)
        {
            bail!(
                "--set {kv}: {k} is stack {stack}'s key; name the deployment in the target: \
                 `dform plan {stack} {kv}`"
            );
        }
    }
    for (k, _) in &cli.keys {
        if !keys.contains(&k.as_str()) {
            if keys.is_empty() {
                bail!("{k}=...: stack {stack} has no key; give an input with `--set {k}=...`");
            }
            bail!(
                "{k} is not a key of stack {stack} (its key: {}); give an input with `--set {k}=...`",
                keys.join(", ")
            );
        }
    }
    Ok(())
}

/// `dform stack list`: every stack discovery finds, its key and file, and
/// per deployment with state its last apply and a pending saved plan.
fn stack_list(cli: &Cli) -> Result<()> {
    let project = crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let d = crate::project::discover(&project);
    for w in &d.warnings {
        eprintln!("warning: {w}");
    }
    d.check()?;
    if d.stacks.is_empty() {
        println!("no stacks in {}", project.root.display());
        return Ok(());
    }
    let registry = crate::stack::registry(&cli.root)?;
    for s in &d.stacks {
        let key = if s.keys.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.keys.join(", "))
        };
        println!("{}{key}  {}", s.name, s.file.display());
        // Where the stack's deployments are: its backend's, else the state
        // root's.
        let base = stack_location(&cli.root, &s.name, stack_backend(&project, s).as_ref());
        if let store::Location::S3(spec) = &base {
            println!("  state in {spec}");
        }
        let opener = open_s3(&cli.root, false);
        let keys = match base.open(&opener).and_then(|st| st.list("")) {
            Ok(k) => k,
            Err(e) => {
                println!("  {base}: {e:#}");
                continue;
            }
        };
        let mut deployments: Vec<(String, store::Location)> = Vec::new();
        if s.keys.is_empty() {
            deployments.push((s.name.clone(), base.clone()));
        } else {
            let mut segs: Vec<&str> = keys
                .iter()
                .filter_map(|k| k.split_once('/').map(|(seg, _)| seg))
                .filter(|seg| seg.contains('='))
                .collect();
            segs.dedup();
            for seg in segs {
                deployments.push((format!("{}[{seg}]", s.name), base.child(Some(seg))));
            }
        }
        for (name, e) in &registry {
            let ours = name == &s.name || name.starts_with(&format!("{}[", s.name));
            if ours && !deployments.iter().any(|(n, _)| n == name) {
                deployments.push((name.clone(), e.state.clone()));
            }
        }
        let mut any = false;
        for (name, location) in deployments {
            let store = match location.open(&opener) {
                Ok(st) => st,
                Err(e) => {
                    println!("  {name}: {e:#}");
                    continue;
                }
            };
            let entries = crate::audit::Log::new(store.clone(), None)
                .entries()
                .unwrap_or_default();
            if entries.is_empty() && store.get(store::STATE)?.is_none() {
                continue;
            }
            any = true;
            let handed = registry
                .get(&name)
                .and_then(|e| e.backend.clone())
                .map(|b| format!(" (handed over to {b})"))
                .unwrap_or_default();
            println!("  {name}{handed}: {}", last_apply(&entries));
        }
        if !any {
            println!("  no deployment has state");
        }
    }
    Ok(())
}

/// A deployment's last apply and pending saved plan, from its audit log.
fn last_apply(entries: &[serde_json::Value]) -> String {
    let field = |e: &serde_json::Value, k: &str| e[k].as_str().unwrap_or("").to_string();
    let start = entries.iter().rposition(|e| e["kind"] == "apply_start");
    let mut out = match start {
        None => "never applied".to_string(),
        Some(i) => {
            let e = &entries[i];
            let end = entries[i..]
                .iter()
                .find(|e| e["kind"] == "apply_end")
                .map(|e| field(e, "result"))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "running or interrupted".into());
            let commit = e["commit"]
                .as_str()
                .map(|c| format!(" at {}", &c[..c.len().min(12)]))
                .unwrap_or_default();
            format!(
                "last apply {} by {}{commit}: {end}",
                field(e, "time"),
                field(e, "who")
            )
        }
    };
    let plan = entries
        .iter()
        .rposition(|e| e["kind"] == "plan" && e["file"].is_string() && e["digest"].is_string());
    if let Some(p) = plan
        && start.is_none_or(|s| p > s)
    {
        out.push_str(&format!(
            "; plan pending: {} ({})",
            field(&entries[p], "file"),
            field(&entries[p], "digest")
        ));
    }
    out
}

/// `dform state show`: the deployment's objects, by address.
fn state_show(dep: &crate::store::Deployment) -> Result<()> {
    let (deployment, at) = (dep.name(), dep.locate(crate::store::STATE));
    if !dep.has_state()? {
        bail!("stack {deployment} has no state at {at}: it was never applied");
    }
    let st = dep.load_state()?;
    println!("{deployment}: {at}");
    for (k, e) in &st.resources {
        let addr = state::parse_key(k).map_or(k.clone(), |a| format!("{}/{}", a.typ, a.name));
        println!("  {addr}  {} {}", e.provider, e.remote);
    }
    for (k, e) in &st.deposed {
        let addr = state::parse_key(k).map_or(k.clone(), |a| format!("{}/{}", a.typ, a.name));
        println!("  {addr} (deposed)  {} {}", e.provider, e.remote);
    }
    for (k, v) in &st.outputs {
        println!("  output {k} = {}", partition::fmt_value(v));
    }
    if st.in_flight.is_some() {
        println!("  an apply was interrupted: the next apply resumes it");
    }
    Ok(())
}

/// `TYPE/NAME`.
fn parse_address(s: &str) -> Result<ir::Address> {
    let (typ, name) = s
        .split_once('/')
        .filter(|(t, n)| !t.is_empty() && !n.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected an address TYPE/NAME, got '{s}'"))?;
    Ok(ir::Address {
        typ: typ.to_string(),
        name: name.to_string(),
    })
}

/// `dform state mv FROM TO`: the object state maps at FROM, at TO; under
/// the deployment's lock, logged.
fn state_mv(
    dep: &crate::store::Deployment,
    from: &str,
    to: &str,
    audit: &crate::audit::Log,
) -> Result<()> {
    let deployment = dep.name();
    let (old, new) = (parse_address(from)?, parse_address(to)?);
    let _lock = dep.lock()?;
    let mut st = dep.load_state()?;
    if st.get(&old).is_none() {
        bail!("state mv: stack {deployment} has no object at {from}");
    }
    if st.get(&new).is_some() {
        bail!("state mv: stack {deployment} already has an object at {to}");
    }
    st.apply_moves(&[(old, new)]);
    dep.save_state(&st)?;
    audit.append(
        "state_mv",
        serde_json::json!({ "from": from, "to": to, "who": crate::audit::who() }),
    )?;
    println!("moved {from} to {to} in stack {deployment}");
    Ok(())
}

/// The top-level commands, and each noun's subcommands.
const COMMANDS: &[&str] = &[
    "plan",
    "apply",
    "why",
    "query",
    "test",
    "fmt",
    "log",
    "stack",
    "state",
    "provider",
    "controller",
    "completions",
    "init",
    "lsp",
    "dev",
];

fn subcommands(noun: &str) -> &'static [&'static str] {
    match noun {
        "stack" => &["list", "rekey", "handover", "unlock"],
        "state" => &["show", "taint", "mv"],
        "provider" => &["check", "schema"],
        "controller" => &["run"],
        "log" => &["verify"],
        "completions" => &["zsh", "bash", "fish"],
        "dev" => &[
            "plan",
            "apply",
            "why",
            "query",
            "test",
            "log",
            "controller",
            "strata",
            "graph",
            "eval",
            "show",
        ],
        _ => &[],
    }
}

/// `dform completions SHELL`: a script that completes commands, and asks
/// `dform __complete` for targets.
fn completion_script(shell: Shell) -> String {
    let commands = COMMANDS.join(" ");
    match shell {
        Shell::Zsh => format!(
            "#compdef dform\n\
             # dform completions zsh > \"${{fpath[1]}}/_dform\"\n\
             _dform() {{\n\
             \x20 if (( CURRENT == 2 )); then\n\
             \x20   compadd -- {commands}\n\
             \x20 else\n\
             \x20   compadd -- ${{(f)\"$(dform __complete ${{words[2,CURRENT-1]}} 2>/dev/null)\"}}\n\
             \x20 fi\n\
             }}\n\
             compdef _dform dform\n"
        ),
        Shell::Bash => format!(
            "# dform completions bash > /etc/bash_completion.d/dform\n\
             _dform() {{\n\
             \x20 local cur=${{COMP_WORDS[COMP_CWORD]}}\n\
             \x20 if [ \"$COMP_CWORD\" -eq 1 ]; then\n\
             \x20   COMPREPLY=($(compgen -W \"{commands}\" -- \"$cur\"))\n\
             \x20 else\n\
             \x20   COMPREPLY=($(compgen -W \"$(dform __complete \"${{COMP_WORDS[@]:1:COMP_CWORD-1}}\" 2>/dev/null)\" -- \"$cur\"))\n\
             \x20 fi\n\
             }}\n\
             complete -F _dform dform\n"
        ),
        Shell::Fish => format!(
            "# dform completions fish > ~/.config/fish/completions/dform.fish\n\
             complete -c dform -f -n '__fish_use_subcommand' -a '{commands}'\n\
             complete -c dform -f -n 'not __fish_use_subcommand' -a '(dform __complete (commandline -opc)[2..-1])'\n"
        ),
    }
}

/// `dform __complete WORDS...`: the candidates for the word after WORDS
/// (the command line without `dform`): a noun's subcommands, else stack
/// names from discovery, the deployments with state, and after a stack's
/// name the values of its key inputs' enum types.
fn complete(words: &[String]) -> Result<()> {
    let words: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !w.starts_with('-'))
        .collect();
    let out: Vec<String> = match words.as_slice() {
        [noun] if !subcommands(noun).is_empty() => {
            subcommands(noun).iter().map(|s| s.to_string()).collect()
        }
        _ => {
            let Some(project) =
                crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?
            else {
                return Ok(());
            };
            let d = crate::project::discover(&project);
            let mut out: Vec<String> = Vec::new();
            match words.last().and_then(|w| d.named(w).first().copied()) {
                Some(s) => out.extend(key_values(s)),
                None => {
                    out.extend(d.stacks.iter().map(|s| s.name.clone()));
                    let root = project.state_root();
                    for s in d.stacks.iter().filter(|s| !s.keys.is_empty()) {
                        let Ok(entries) = std::fs::read_dir(root.join(&s.name)) else {
                            continue;
                        };
                        for e in entries.flatten() {
                            let seg = e.file_name().to_string_lossy().into_owned();
                            if seg.contains('=') && e.path().join("state.json").exists() {
                                out.push(format!("{}[{seg}]", s.name));
                            }
                        }
                    }
                }
            }
            out
        }
    };
    let mut out = out;
    out.sort();
    out.dedup();
    for c in out {
        println!("{c}");
    }
    Ok(())
}

/// `K=V` for each value of each key input with an enum type.
fn key_values(s: &crate::project::Found) -> Vec<String> {
    let Ok(program) = loader::load_program(std::slice::from_ref(&s.file)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for st in &program.statements {
        let crate::ast::Stmt::Input(i) = st else {
            continue;
        };
        if !s.keys.contains(&i.name) {
            continue;
        }
        if let crate::ast::TypeExpr::Apply(n, args) = &i.ty
            && n == "enum"
        {
            for a in args {
                if let crate::ast::TypeExpr::Str(v) | crate::ast::TypeExpr::Name(v) = a {
                    out.push(format!("{}={v}", i.name));
                }
            }
        }
    }
    out
}

fn build_extra_facts(data: &[String]) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    for kv in data {
        let (k, v) = split_kv(kv)?;
        out.push(atom_kv("data", k, v));
    }
    Ok(out)
}

fn split_kv(s: &str) -> Result<(&str, Value)> {
    let (k, raw) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected key=value, got '{s}'"))?;
    let v = if raw == "true" {
        Value::Bool(true)
    } else if raw == "false" {
        Value::Bool(false)
    } else if let Ok(i) = raw.parse::<i64>() {
        Value::Int(i)
    } else {
        Value::Str(raw.to_string())
    };
    Ok((k, v))
}

fn atom_kv(pred: &str, k: &str, v: Value) -> Atom {
    Atom {
        pred: pred.to_string(),
        args: vec![Term::Val(Value::Str(k.to_string())), Term::Val(v)],
        record: None,
        span: Default::default(),
    }
}
