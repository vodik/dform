//! The command line as typed (clap's definitions) and what `resolve` makes
//! of it: one run, [`Cli`].

use super::apply::Apply;
use super::complete::{Complete, Completions};
use super::controller_cmd::Controller;
use super::dev::{Effects, Eval, Graph, Show, Strata};
use super::explain::{Diff, Explain, Query, Why};
use super::plan::Plan;
use super::provider_cmd::{ProviderCheck, ProviderSchema};
use super::render::Render;
use super::secrets::Secrets;
use super::source::{Doc, Fmt, Init};
use super::stack::{Handover, Rekey, StackList};
use super::state_cmd::{ForgetHost, Log, Output, StateMv, StateShow, Unlock};
use super::status::Status;
use super::test::Test;
use super::{Cli, Cmd, Held, matrix};
use crate::project::Project;
use crate::report;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// The line an experimental command prints on every run.
pub(super) const EXPERIMENTAL: &str = "warning: controller mode is experimental: its process model (where it \
                            runs, how it is supervised, what an operator sees) is not decided; \
                            see docs/experimental/controller.md";

/// The command line as typed (docs/reference.md). `resolve` turns it into
/// one run, [`Cli`]: the target's files, its key, the project's state.
#[derive(Parser, Debug, Clone)]
#[command(name = "dform")]
#[command(about = "Facts + rules + constraints for infra", long_about = None)]
#[command(after_help = "See dform(1) for the full documentation.")]
pub(super) struct Args {
    /// Run as if dform started in DIR: the project, its dform.state/ and
    /// discovery are DIR's.
    #[arg(short = 'C', value_name = "DIR", global = true)]
    pub(super) dir: Option<PathBuf>,

    #[command(flatten)]
    pub(super) inputs: Inputs,

    #[command(subcommand)]
    pub(super) cmd: Command,
}

/// What a run is given besides its target.
#[derive(clap::Args, Debug, Clone, Default)]
pub(super) struct Inputs {
    /// A stack input: --set region=us-east1, an `@override` that wins over
    /// the input's default and every settings block. A key input's value is
    /// the target's (`dform plan app env=prod`), never --set's.
    #[arg(long = "set", global = true, value_name = "K=V")]
    pub(super) set: Vec<String>,

    /// Stack inputs from a .df file of facts, one `name(value).` per input
    /// (repeatable). Each is a normal contribution, like a settings block's.
    #[arg(long = "input-file", global = true)]
    pub(super) input_files: Vec<PathBuf>,

    /// Provide data facts: --data zone=us-test-1a
    #[arg(long = "data", global = true)]
    pub(super) data: Vec<String>,

    /// Show no-op actions in plan
    #[arg(long, global = true)]
    pub(super) show_noop: bool,

    /// Also pipe every audit log entry, a JSON line, to this command
    /// (`sh -c CMD`, once per entry). Overrides the stack's `audit_sink`.
    /// A sink that fails is a warning; the local log is authoritative.
    #[arg(long = "audit-sink", global = true)]
    pub(super) audit_sink: Option<String>,

    /// Colour the plan and errors: auto (when the output is a terminal and
    /// NO_COLOR is unset), always, never. `--json` and the plan file are
    /// never coloured.
    #[arg(long = "color", global = true, value_enum, default_value_t = ColorWhen::Auto)]
    pub(super) color: ColorWhen,
}

/// `--color`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ColorWhen {
    #[default]
    Auto,
    Always,
    Never,
}

/// The mock's flags: `dform dev [FLAGS] COMMAND`.
#[derive(clap::Args, Debug, Clone, Default)]
pub(super) struct Mock {
    /// A provider: a schema the mock provider plays (a name,
    /// providers/NAME/schema.df else a built-in, or a path to a schema .df
    /// file), or a plugin executable (a path to one, or to a directory
    /// holding a `dform-provider*`). Repeatable; overrides the program's
    /// providers' `use`s.
    #[arg(long = "provider", global = true)]
    pub(super) providers: Vec<String>,

    /// The fake provider's world file: what exists. Plan refreshes from it,
    /// apply writes it back. State sits beside it as <stem>.state.json.
    /// Default: dform.state/<stack>/remote.json.
    #[arg(long = "world", global = true)]
    pub(super) world: Option<PathBuf>,

    /// Discovery inventory file (cloud_exists/cloud_attr/cloud_computed).
    /// Default: <world dir>/inventory.json if --world is given and that file
    /// exists, else dform.state/inventory.json.
    #[arg(long = "inventory", global = true)]
    pub(super) inventory: Option<PathBuf>,

    /// Inject a failure into the fake provider at apply (repeatable):
    /// fail=T/N, timeout=T/N, crash=T/N, read-lag=T/N:READS,
    /// mutate=T/N:PATH=JSON, latency=T/N:MS, fresh-ids, stop-after=N.
    /// Deterministic; nothing sleeps.
    #[arg(long = "chaos", global = true)]
    pub(super) chaos: Vec<String>,
}

/// What a command runs on: a stack by name (`infra`), a program file
/// (`stacks/infra.df`), or a deployment (`shop.app[env=prod]`, or
/// `shop.app env=prod`). None: the one stack under the current directory;
/// for plan, apply and test in a project with a project.df, the
/// deployments it lists.
#[derive(clap::Args, Debug, Clone, Default)]
pub(super) struct Target {
    /// A stack name, a .df file, or a deployment `NAME[K=V,...]`.
    #[arg(value_name = "TARGET")]
    pub(super) target: Option<String>,
    /// The deployment's key values.
    #[arg(value_name = "K=V")]
    pub(super) keys: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum Command {
    #[command(flatten)]
    Run(Run),
    /// Format .df files in place.
    Fmt {
        paths: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// Print the project's doc comments as Markdown.
    Doc {
        #[command(flatten)]
        target: Target,
    },
    /// List, rekey and unlock the project's stacks' deployments.
    Stack {
        #[command(subcommand)]
        cmd: StackCommand,
    },
    /// Print a deployment's outputs as of its last apply.
    Output {
        /// The stack (or deployment) and its key values, then the output's
        /// NAME: `dform output app env=prod url`.
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Show each object's health, as its provider judges it now.
    Status {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Show and edit a deployment's state.
    State {
        #[command(subcommand)]
        cmd: StateCommand,
    },
    /// List, rotate, cycle and give a deployment's secrets.
    Secrets {
        #[command(subcommand)]
        cmd: SecretsCommand,
    },
    /// Check a provider, or print its schema.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCommand,
    },
    /// Make the working directory a project.
    Init { name: Option<String> },
    /// Print dform's version and its time zone database's release.
    Version,
    /// Print a shell completion script.
    Completions { shell: Shell },
    /// Run a command against the mock, or show the evaluator's views.
    Dev {
        #[command(flatten)]
        mock: Mock,
        #[command(subcommand)]
        cmd: DevCommand,
    },
    /// Serve the language server protocol on stdin and stdout.
    Lsp,
    /// The completion scripts' helper: candidates for the next word.
    #[command(name = "__complete", hide = true)]
    Complete { words: Vec<String> },
    /// Serve a provider built into dform (`fake`, the mock) over gRPC:
    /// how dform starts the mock, so it is always of the same build.
    #[command(name = "__provider", hide = true)]
    ServeProvider { name: String },
}

/// How much each change of a report says of why it is planned:
/// `-q`, the default, `-v`, `-vv`; `--why=LEVEL` by name.
#[derive(clap::Args, Debug, Clone)]
pub(super) struct Ladder {
    /// Quiet: the bare diff in apply order, each change by its full
    /// address with its values, no site or binding (`--why=none`).
    #[arg(short = 'q', long = "quiet", conflicts_with_all = ["verbose", "why"])]
    pub(super) quiet: bool,
    /// Say how: `-v` adds the deriving statement's bindings, the
    /// expression behind each value and the writes that lost, with their
    /// ranks (`--why=how`); `-vv` also each change's derivation,
    /// compressed to the facts, table rows, inputs and extern answers it
    /// rests on, one line each (`--why=full`).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count,
          conflicts_with = "why")]
    pub(super) verbose: u8,
    /// The level by name: `none` (`-q`); `line` (the default): where each
    /// change is derived, where a value written outside its block was
    /// written, the leaf that changed since the last apply; `how` (`-v`);
    /// `full` (`-vv`, `--why` alone).
    #[arg(long, value_name = "LEVEL", default_value = "line",
          num_args = 0..=1, require_equals = true, default_missing_value = "full")]
    pub(super) why: report::Why,
}

/// The commands that run on a target, at the top level and under `dev`.
#[derive(Subcommand, Debug, Clone)]
pub(super) enum Run {
    /// Plan a deployment: what apply would do, and what it waits on.
    Plan {
        #[command(flatten)]
        target: Target,
        /// Write the plan file: inputs, a digest of the world, and the
        /// changes with their nulls and tick schedule.
        /// `apply PLAN.json` applies exactly this delta or refuses.
        #[arg(long = "out")]
        out: Option<PathBuf>,
        /// Print the plan as one JSON document instead of text.
        #[arg(long)]
        json: bool,
        /// The plan `destroy` would apply: every object of the
        /// deployment's state deleted.
        #[arg(long, conflicts_with = "out")]
        destroy: bool,
        /// Take the master this run has (`RANDOM_MASTER`, a restored or a
        /// new key file) though state was applied with another, or make
        /// one where the key file is missing: every `random.*` value and
        /// every secret digest changes, on purpose.
        #[arg(long = "new-master")]
        new_master: bool,
        #[command(flatten)]
        why: Ladder,
    },
    /// Apply a deployment, or a plan file from `plan --out`.
    Apply {
        #[command(flatten)]
        target: Target,
        /// A safety valve: stop after this many ticks (phase boundaries)
        /// if the stack still has changes, a loop that never settles.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
        /// At most this many provider Apply calls in flight: a tick's
        /// independent actions overlap.
        #[arg(long = "parallel", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        parallel: u64,
        /// A signed approval of the plan file's digest (a JWT, or a DSSE
        /// envelope): verified against the stack's `approvals` trust root
        /// before any Apply call. Required when the policy says
        /// `requires_approval` of a change.
        #[arg(long = "approval")]
        approval: Option<PathBuf>,
        /// Apply without asking: every tick, each later one planned when
        /// the one before reports. Without it, apply prints the plan and
        /// asks before changing anything and again before a tick that
        /// adds what the plan could not show, and refuses when there is
        /// no terminal to ask on. `apply PLAN.json` never asks, and stops
        /// before such a tick.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// A rule or relation this apply may empty: the plan's
        /// `warning` names it and apply asks for it on its own, also
        /// under `--yes`, unless named here: the rule's `FILE:LINE`, a
        /// resource type it derives, or the relation. Repeatable; dform.toml
        /// `[stacks.NAME] allow_empty` names them for every apply.
        #[arg(long = "allow-empty", value_name = "RULE")]
        allow_empty: Vec<String>,
        /// How long a tick waits on a value the world has not reached yet
        /// (a rollout's `status.availableReplicas`, a Job's, an extern's
        /// "not yet") before the apply stops, exit 1: `90s`, `10m`.
        /// Over dform.toml's `[stacks.NAME] wait`, each provider's and
        /// `[apply] wait` (10m by default).
        #[arg(long = "wait-timeout", value_name = "DURATION", value_parser = wait_timeout)]
        wait_timeout: Option<std::time::Duration>,
        /// Take the master this run has (`RANDOM_MASTER`, a restored or a
        /// new key file) though state was applied with another, or make
        /// one where the key file is missing: every `random.*` value and
        /// every secret digest changes, on purpose.
        #[arg(long = "new-master")]
        new_master: bool,
        #[command(flatten)]
        why: Ladder,
    },
    /// Remove a deployment: delete every object its state holds.
    Destroy {
        #[command(flatten)]
        target: Target,
        /// A safety valve: stop after this many ticks.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
        /// At most this many provider Apply calls in flight.
        #[arg(long = "parallel", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        parallel: u64,
        /// A signed approval of the digest `plan --destroy` prints, when
        /// the policy says `requires_approval` of a delete.
        #[arg(long = "approval")]
        approval: Option<PathBuf>,
        /// Destroy without asking.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// How long a tick waits on a value not reached yet before the
        /// destroy stops, as `apply --wait-timeout`.
        #[arg(long = "wait-timeout", value_name = "DURATION", value_parser = wait_timeout)]
        wait_timeout: Option<std::time::Duration>,
        #[command(flatten)]
        why: Ladder,
    },
    /// Show how a value was derived, or why it was not.
    Why {
        pattern: String,
        #[command(flatten)]
        target: Target,
        /// A value's or a resource's whole derivation tree, not its
        /// chain: each fact with the rule and bindings that derived it.
        #[arg(long)]
        tree: bool,
        /// Show every alternative derivation, not only the first (a
        /// tree).
        #[arg(long)]
        all: bool,
        /// Print the tree in the core's spelling: lowered rules (`r12:
        /// head :- body`), core variables, facts as relations.
        #[arg(long)]
        core: bool,
        /// `-vv`: a long value whole, not elided (a secret stays
        /// `(sensitive)`); `-v` elides it as the default does.
        #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
        verbose: u8,
        /// Print one JSON document: each fact `why` names with its value
        /// whole and what `why -vv` prints of it.
        #[arg(long)]
        json: bool,
    },
    /// Query the final fact store.
    Query {
        pattern: String,
        #[command(flatten)]
        target: Target,
        /// Print the answer as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Show what the applies since REF did, and why.
    Diff {
        #[command(flatten)]
        target: Target,
        /// The first apply: a sequence number of the audit log, a time
        /// (RFC 3339, or a prefix of one: `2026-09-28`), or a git commit an
        /// apply recorded.
        #[arg(long, value_name = "REF")]
        since: String,
        /// Print the diff as one JSON document.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        why: Ladder,
    },
    /// `diff`'s helper: what the program says about ADDRESSes, as JSON.
    #[command(name = "__explain", hide = true)]
    Explain {
        #[command(flatten)]
        target: Target,
        #[arg(long = "address")]
        addresses: Vec<String>,
    },
    /// Run the program's denies over its input space.
    Test {
        #[command(flatten)]
        target: Target,
    },
    /// Print the planned documents as a YAML stream for another tool.
    Render {
        #[command(flatten)]
        target: Target,
        /// Print the objects as one JSON array.
        #[arg(long)]
        json: bool,
        /// Print the documents that hold no value only an apply makes,
        /// and say the rest on stderr, instead of refusing.
        #[arg(long)]
        partial: bool,
    },
    /// Print a deployment's audit log.
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
    /// Run a stack in controller mode (experimental).
    #[command(hide = !experimental())]
    Controller {
        #[command(subcommand)]
        cmd: ControllerCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum ControllerCommand {
    /// Apply a stack whenever a source it reads or the world changes.
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
        /// Per event, stop after this many ticks if changes remain.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum LogCommand {
    /// Check the audit log's hash chain.
    Verify {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum StackCommand {
    /// List the project's stacks and their deployments.
    List,
    /// Move a deployment's state to another key value.
    Rekey {
        stack: String,
        /// Each key input as `k=v`: the old values, then the new ones.
        #[arg(value_name = "K=V", required = true)]
        pairs: Vec<String>,
    },
    /// Move a deployment's state to another backend (experimental).
    #[command(hide = !experimental())]
    Handover {
        stack: String,
        #[arg(long = "to")]
        to: String,
    },
    /// Remove an apply lock left by an apply that is gone.
    Unlock {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum StateCommand {
    /// Print a deployment's state.
    Show {
        #[arg(long = "address", value_name = "ADDR")]
        addr: Option<String>,
        /// The state the audit log alone makes (its `state` entries from
        /// the last whole one), not the state file's checkpoint.
        #[arg(long = "from-log")]
        from_log: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Forget the host key recorded for HOST.
    ForgetHost {
        host: String,
        #[command(flatten)]
        target: Target,
    },
    /// Give the object at FROM the address TO.
    Mv {
        from: String,
        to: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum SecretsCommand {
    /// List the deployment's secrets, never a value.
    List {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Rotate one secret: the next plan changes its value.
    Rotate {
        /// [TARGET] [K=V..] KEY
        #[arg(value_name = "TARGET K=V.. KEY", required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// Make a new master epoch beside the current one.
    Cycle {
        #[command(flatten)]
        target: Target,
    },
    /// Give a secret, read from stdin and sealed into the file of given secrets.
    Set {
        /// [TARGET] [K=V..] NAME
        #[arg(value_name = "TARGET K=V.. NAME", required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// Remove a given secret from its file.
    Unset {
        /// [TARGET] [K=V..] NAME
        #[arg(value_name = "TARGET K=V.. NAME", required = true, num_args = 1..)]
        words: Vec<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum ProviderCommand {
    /// Run the conformance suite against a provider.
    Check { path: String },
    /// Print a provider's schema facts.
    Schema { provider: String },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum DevCommand {
    #[command(flatten)]
    Run(Run),
    /// Print the program's stratification.
    Strata {
        #[command(flatten)]
        target: Target,
    },
    /// Print what each scope reads, writes and offers.
    Effects {
        #[command(flatten)]
        target: Target,
        /// Print as one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print a graph as Graphviz DOT: dependencies, strata or a relation.
    Graph {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "relation")]
        strata: bool,
        #[arg(long)]
        relation: Option<String>,
    },
    /// Print the evaluation's size and resources.
    Eval {
        #[command(flatten)]
        target: Target,
    },
    /// Print a resource's desired document.
    Show {
        #[arg(value_name = "ADDR")]
        addr: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub(super) enum Shell {
    Zsh,
    Bash,
    Fish,
}

/// Whether the experimental commands (R-41: `controller`, `stack
/// handover`) are listed in `--help` and completions: `DFORM_EXPERIMENTAL=1`.
/// They run either way, each run with [`EXPERIMENTAL`] on stderr.
pub(super) fn experimental() -> bool {
    std::env::var_os("DFORM_EXPERIMENTAL").is_some_and(|v| v == "1")
}

/// The width of the terminal stdout is, if it is one.
fn terminal_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let (cols, _) = crossterm::terminal::size().ok()?;
    (cols > 0).then_some(cols as usize)
}

impl ColorWhen {
    /// Colour output to a stream that is a terminal (`terminal`) or not:
    /// `auto` colours a terminal unless NO_COLOR is set (non-empty).
    pub(super) fn style(self, terminal: bool) -> report::Style {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        report::Style {
            color: match self {
                ColorWhen::Always => true,
                ColorWhen::Never => false,
                ColorWhen::Auto => terminal && !no_color,
            },
        }
    }
}

impl Ladder {
    pub(super) fn level(&self) -> report::Why {
        report::Why::of(self.quiet, self.verbose, self.why)
    }
}

impl Args {
    /// The run the command line asks for: `-C` taken, the working project
    /// found, the target resolved to its program file and key.
    pub(super) fn resolve(self) -> Result<Cli> {
        if let Some(dir) = &self.dir {
            std::env::set_current_dir(dir)
                .map_err(|e| anyhow::anyhow!("-C {}: {e}", dir.display()))?;
        }
        let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
        let Typed { cmd, target, mock } = self.cmd.try_into()?;
        let mut cli = Cli::new(cmd, self.inputs, mock, project.is_some())?;
        if let Some(root) = project.as_ref().map(|p| p.state_root()) {
            cli.root = root;
        }
        cli.target(target, project.as_ref())?;
        Ok(cli)
    }
}

/// A command as typed: the command, its target, and the mock's flags
/// `dev` gave.
struct Typed {
    cmd: Cmd,
    target: Option<Target>,
    mock: Mock,
}

impl Typed {
    fn of(cmd: Cmd, target: Option<Target>) -> Typed {
        Typed {
            cmd,
            target,
            mock: Mock::default(),
        }
    }
}

impl TryFrom<Command> for Typed {
    type Error = anyhow::Error;

    fn try_from(c: Command) -> Result<Typed> {
        Ok(match c {
            Command::Run(r) => {
                let (cmd, target) = r.into();
                Typed::of(cmd, Some(target))
            }
            Command::Dev { mock, cmd } => {
                let (cmd, target) = cmd.into();
                Typed {
                    cmd,
                    target: Some(target),
                    mock,
                }
            }
            Command::Fmt { paths, check } => Typed::of(Cmd::Fmt(Fmt { paths, check }), None),
            Command::Doc { target } => {
                Typed::of(Cmd::Doc(Doc), target.target.is_some().then_some(target))
            }
            Command::Stack { cmd } => match cmd {
                StackCommand::List => Typed::of(Cmd::StackList(StackList), None),
                StackCommand::Rekey { stack, pairs } => {
                    let target = Target {
                        target: Some(stack.clone()),
                        keys: vec![],
                    };
                    Typed::of(Cmd::Rekey(Rekey { stack, pairs }), Some(target))
                }
                StackCommand::Handover { stack, to } => {
                    Typed::of(Cmd::Handover(Handover { stack, to }), None)
                }
                StackCommand::Unlock { target } => Typed::of(Cmd::Unlock(Unlock), Some(target)),
            },
            Command::Output { mut target, json } => {
                // The output's NAME is the one word after the target that
                // is not a key value.
                let mut names: Vec<String> = Vec::new();
                target.keys.retain(|k| {
                    let key = k.contains('=');
                    if !key {
                        names.push(k.clone());
                    }
                    key
                });
                if names.len() > 1 {
                    bail!("output: one NAME at most, got {}", names.join(" "));
                }
                let name = names.pop();
                Typed::of(Cmd::Output(Output { name, json }), Some(target))
            }
            Command::Status { target, json } => {
                Typed::of(Cmd::Status(Status { json }), Some(target))
            }
            Command::State { cmd } => match cmd {
                StateCommand::Show {
                    addr,
                    from_log,
                    target,
                } => Typed::of(Cmd::StateShow(StateShow { addr, from_log }), Some(target)),
                StateCommand::ForgetHost { host, target } => {
                    Typed::of(Cmd::ForgetHost(ForgetHost { host }), Some(target))
                }
                StateCommand::Mv { from, to, target } => {
                    Typed::of(Cmd::StateMv(StateMv { from, to }), Some(target))
                }
            },
            Command::Secrets { cmd } => {
                let (secrets, target) = cmd.into();
                Typed::of(Cmd::Secrets(secrets), Some(target))
            }
            Command::Provider { cmd } => match cmd {
                ProviderCommand::Check { path } => {
                    Typed::of(Cmd::ProviderCheck(ProviderCheck { path }), None)
                }
                ProviderCommand::Schema { provider } => {
                    Typed::of(Cmd::ProviderSchema(ProviderSchema { provider }), None)
                }
            },
            Command::Init { name } => Typed::of(Cmd::Init(Init { name }), None),
            Command::Completions { shell } => {
                Typed::of(Cmd::Completions(Completions { shell }), None)
            }
            Command::Complete { words } => Typed::of(Cmd::Complete(Complete { words }), None),
            Command::ServeProvider { .. } => {
                bail!("internal: `__provider` serves before a project")
            }
            Command::Lsp => bail!("internal: `lsp` serves before a project"),
            Command::Version => bail!("internal: `version` prints before a project"),
        })
    }
}

/// `dform secrets ..`, and its target: `rotate`, `set` and `unset` take the
/// target's words before their own last one.
impl From<SecretsCommand> for (Secrets, Target) {
    fn from(c: SecretsCommand) -> (Secrets, Target) {
        match c {
            SecretsCommand::List { target, json } => (Secrets::List { json }, target),
            SecretsCommand::Rotate { words } => {
                let (target, key) = target_then(words);
                (Secrets::Rotate { key }, target)
            }
            SecretsCommand::Cycle { target } => (Secrets::Cycle, target),
            SecretsCommand::Set { words } => {
                let (target, name) = target_then(words);
                let remove = false;
                (Secrets::Set { name, remove }, target)
            }
            SecretsCommand::Unset { words } => {
                let (target, name) = target_then(words);
                let remove = true;
                (Secrets::Set { name, remove }, target)
            }
        }
    }
}

/// A `dev` command, and its target.
impl From<DevCommand> for (Cmd, Target) {
    fn from(c: DevCommand) -> (Cmd, Target) {
        match c {
            DevCommand::Run(r) => r.into(),
            DevCommand::Strata { target } => (Cmd::Strata(Strata), target),
            DevCommand::Effects { target, json } => (Cmd::Effects(Effects { json }), target),
            DevCommand::Graph {
                target,
                strata,
                relation,
            } => {
                let what = if strata {
                    Some("strata".into())
                } else {
                    relation
                };
                (Cmd::Graph(Graph { what }), target)
            }
            DevCommand::Eval { target } => (Cmd::Eval(Eval), target),
            DevCommand::Show { addr, target } => (Cmd::Show(Show { addr }), target),
        }
    }
}

impl Cli {
    /// The run of `cmd`, given `inputs` and the mock's flags, its target
    /// not yet resolved. Outside a project nothing writes the state root
    /// (`needs_project`).
    fn new(cmd: Cmd, inputs: Inputs, mock: Mock, in_project: bool) -> Result<Cli> {
        use std::io::IsTerminal;
        let style = inputs.color.style(std::io::stdout().is_terminal());
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
            in_project,
            root: PathBuf::from(crate::project::STATE_DIR),
            manifest: None,
            inventory: mock.inventory,
            audit_sink: inputs.audit_sink,
            style,
            err_style: inputs.color.style(std::io::stderr().is_terminal()),
            table: report::table::Options {
                width: terminal_width().unwrap_or(report::table::Options::PLAIN.width),
                style,
            },
            every_stack: Vec::new(),
            matrix: None,
            held: Held::default(),
            planned: Default::default(),
            sequence: None,
        };
        if let Cmd::Apply(Apply { chaos, .. }) = &mut cli.cmd {
            *chaos = mock.chaos;
        } else if !mock.chaos.is_empty() {
            bail!("--chaos is for apply");
        }
        Ok(cli)
    }

    /// The run's target resolved: a plan file (`apply`'s, whose inputs
    /// name the program), the project module (R-114), the project's every
    /// stack (`apply` with no target), else a stack's program file and key.
    fn target(&mut self, target: Option<Target>, project: Option<&Project>) -> Result<()> {
        if let (
            Cmd::Apply(Apply {
                plan_file,
                destroy: false,
                ..
            }),
            Some(t),
        ) = (&mut self.cmd, &target)
            && let Some(f) = t.target.as_deref().filter(|f| f.ends_with(".json"))
        {
            if !t.keys.is_empty() {
                bail!("apply {f}: a plan file names its deployment; give no key values");
            }
            *plan_file = Some(PathBuf::from(f));
            return Ok(());
        }
        let Some(target) = target else {
            return Ok(());
        };
        // The project module (R-114): with no target, the root's
        // project.df; a target that is one.
        if let Some(module) = matrix::target(&self.cmd, project, &target)? {
            self.matrix = Some(module);
            return Ok(());
        }
        if let (Cmd::Apply(Apply { destroy: false, .. }), None, true, Some(p)) =
            (&self.cmd, &target.target, target.keys.is_empty(), project)
            && let Some(here) = every_stack(p)?
        {
            self.every_stack = here;
            return Ok(());
        }
        let (file, keys) = target_of(project, &target)?;
        self.set
            .extend(keys.iter().map(|(k, v)| format!("{k}={v}")));
        self.keys = keys;
        self.files = vec![file];
        Ok(())
    }
}

/// `--wait-timeout DURATION` (R-201): as dform.toml writes a wait.
fn wait_timeout(s: &str) -> std::result::Result<std::time::Duration, String> {
    crate::store::parse_duration(s)
        .filter(|d| !d.is_zero())
        .ok_or_else(|| format!("{s:?} is not a duration: `500ms`, `30s` or `10m`"))
}

/// `apply` with no target applies the project: every stack under the
/// working directory, in dependency order, each confirmed on its own;
/// `None` when there is one.
fn every_stack(p: &Project) -> Result<Option<Vec<PathBuf>>> {
    let d = crate::project::discover(p);
    d.check()?;
    let here: Vec<PathBuf> = d
        .stacks
        .iter()
        .filter(|s| s.file.is_relative() && !s.file.starts_with(".."))
        .map(|s| s.file.clone())
        .collect();
    if here.len() <= 1 {
        return Ok(None);
    }
    for w in &d.warnings {
        eprintln!("warning: {w}");
    }
    Ok(Some(here))
}

/// A command that runs on a target, and the target.
impl From<Run> for (Cmd, Target) {
    fn from(r: Run) -> (Cmd, Target) {
        match r {
            Run::Plan {
                target,
                out,
                json,
                destroy,
                new_master,
                why,
            } => (
                Cmd::Plan(Plan {
                    out,
                    json,
                    destroy,
                    why: why.level(),
                    new_master,
                }),
                target,
            ),
            Run::Apply {
                target,
                max_ticks,
                parallel,
                approval,
                yes,
                allow_empty,
                wait_timeout,
                new_master,
                why,
            } => (
                Cmd::Apply(Apply {
                    plan_file: None,
                    chaos: Vec::new(),
                    max_ticks,
                    parallel,
                    approval,
                    yes,
                    allow_empty,
                    wait_timeout,
                    why: why.level(),
                    destroy: false,
                    new_master,
                }),
                target,
            ),
            Run::Destroy {
                target,
                max_ticks,
                parallel,
                approval,
                yes,
                wait_timeout,
                why,
            } => (
                Cmd::Apply(Apply {
                    plan_file: None,
                    chaos: Vec::new(),
                    max_ticks,
                    parallel,
                    approval,
                    yes,
                    allow_empty: Vec::new(),
                    wait_timeout,
                    why: why.level(),
                    destroy: true,
                    new_master: false,
                }),
                target,
            ),
            Run::Why {
                pattern,
                target,
                tree,
                all,
                core,
                verbose,
                json,
            } => (
                Cmd::Why(Why {
                    pattern,
                    tree,
                    all,
                    core,
                    whole: verbose >= 2,
                    json,
                }),
                target,
            ),
            Run::Query {
                pattern,
                target,
                json,
            } => (Cmd::Query(Query { pattern, json }), target),
            Run::Diff {
                target,
                since,
                json,
                why,
            } => (
                Cmd::Diff(Diff {
                    since,
                    json,
                    why: why.level(),
                }),
                target,
            ),
            Run::Explain { target, addresses } => (Cmd::Explain(Explain { addresses }), target),
            Run::Test { target } => (Cmd::Test(Test), target),
            Run::Render {
                target,
                json,
                partial,
            } => (Cmd::Render(Render { json, partial }), target),
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
                    Cmd::Log(Log {
                        verify,
                        since,
                        json,
                    }),
                    target,
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
                Cmd::Controller(Controller {
                    poll,
                    once,
                    max_events,
                    max_ticks,
                }),
                target,
            ),
        }
    }
}

/// The program file and key values a target names. A path (`.df`) is the
/// program; a name is the project's stack of that name; `NAME[K=V,...]`
/// and trailing `K=V`s are the key. No target: the one stack under the
/// working directory, else the stacks there are listed.
fn target_of(project: Option<&Project>, t: &Target) -> Result<(PathBuf, Vec<(String, String)>)> {
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
            match d.one(&n)? {
                Some(one) => one.file.clone(),
                None => bail!(
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
                    "no stack under {}: a stack is a file under stacks/, or one a \
                     `[stacks.NAME]` names; name a program file \
                     (`dform plan path/to/file.df`){}",
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

/// `[TARGET] [K=V..] WORD`: the target, then the last word (a secret's
/// key or name).
fn target_then(mut words: Vec<String>) -> (Target, String) {
    let last = words.pop().expect("clap: one word at least");
    let mut words = words.into_iter();
    let target = Target {
        target: words.next(),
        keys: words.collect(),
    };
    (target, last)
}
