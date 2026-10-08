//! The command line as typed (clap's definitions) and what `resolve` makes
//! of it: one run, [`Cli`].

use super::apply::Apply;
use super::complete::{Complete, Completions};
use super::controller_cmd::Controller;
use super::dev::{Effects, Eval, Graph, Show, Strata};
use super::explain::{Diff, Explain, Query, Why};
use super::plan::Plan;
use super::provider_cmd::{ProviderCheck, ProviderSchema};
use super::secrets::Secrets;
use super::source::{Doc, Fmt, Init};
use super::stack::{Handover, Rekey, StackList};
use super::state_cmd::{ForgetHost, Log, Output, StateMv, StateShow, Unlock};
use super::test::Test;
use super::{Cli, Cmd, Held, matrix};
use crate::report;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// The width of the terminal stdout is, if it is one.
pub(super) fn terminal_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let (cols, _) = crossterm::terminal::size().ok()?;
    (cols > 0).then_some(cols as usize)
}

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
    /// Format .df files in place: spacing, indentation and the commas a
    /// newline makes redundant (line breaks are kept). No PATH formats the
    /// project's .df files. `--check` changes nothing and fails if any
    /// file would.
    Fmt {
        paths: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// The project's doc comments (`#|` lines above an item) as Markdown on
    /// stdout: every .df file's, or with a TARGET its program's; then the
    /// standard library's functions.
    Doc {
        #[command(flatten)]
        target: Target,
    },
    /// The project's stacks and their deployments.
    Stack {
        #[command(subcommand)]
        cmd: StackCommand,
    },
    /// A result set: a deployment's outputs as of its last apply, the
    /// scalars as a key/value table and each relation as its own table;
    /// with NAME, that output's bare value for the shell (a string's bytes
    /// unquoted, a relation's rows tab-separated).
    Output {
        /// The stack (or deployment) and its key values, then the output's
        /// NAME: `dform output app env=prod url`.
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// A deployment's state.
    State {
        #[command(subcommand)]
        cmd: StateCommand,
    },
    /// A deployment's secrets: list them, rotate one (R-161), give one
    /// (R-108).
    Secrets {
        #[command(subcommand)]
        cmd: SecretsCommand,
    },
    /// Provider tools.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCommand,
    },
    /// Make the working directory a project: a minimal dform.toml, and
    /// dform.state/ in the nearest .gitignore.
    Init { name: Option<String> },
    /// Print dform's version and the release of the time zone database
    /// built into it (R-62: zones never come from the host).
    Version,
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
    /// of the selected environment, a contributors hover and docs at
    /// point, schema completion, signature help (docs/reference.md,
    /// "Language server").
    Lsp,
    /// The completion scripts' helper: candidates for the next word.
    #[command(name = "__complete", hide = true)]
    Complete { words: Vec<String> },
    /// Serve a provider built into dform (`fake`, the mock) over gRPC:
    /// how dform starts the mock, so it is always of the same build.
    #[command(name = "__provider", hide = true)]
    ServeProvider { name: String },
}

/// How much each change of a report says of why it is planned (R-79,
/// R-111): `-q`, the default, `-v`, `-vv`; `--why=LEVEL` by name.
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

impl Ladder {
    pub(super) fn level(&self) -> report::Why {
        report::Why::of(self.quiet, self.verbose, self.why)
    }
}

/// The commands that run on a target, at the top level and under `dev`.
#[derive(Subcommand, Debug, Clone)]
pub(super) enum Run {
    /// A report: plan a deployment, what apply would do, and what it
    /// waits on.
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
        /// every secret digest changes, on purpose (R-163).
        #[arg(long = "new-master")]
        new_master: bool,
        #[command(flatten)]
        why: Ladder,
    },
    /// A report: apply a deployment, every key value named (`apply app
    /// env=prod`), after the deployments it reads, or a plan file from `plan
    /// --out` (`apply PLAN.json`): refresh, re-evaluate, and refuse unless the
    /// delta is the file's.
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
        /// A rule or relation this apply may empty (R-80): the plan's
        /// `warning` names it and apply asks for it on its own, also
        /// under `--yes`, unless named here: the rule's `FILE:LINE`, a
        /// resource type it derives, or the relation. Repeatable; dform.toml
        /// `[stacks.NAME] allow_empty` names them for every apply.
        #[arg(long = "allow-empty", value_name = "RULE")]
        allow_empty: Vec<String>,
        /// Take the master this run has (`RANDOM_MASTER`, a restored or a
        /// new key file) though state was applied with another, or make
        /// one where the key file is missing: every `random.*` value and
        /// every secret digest changes, on purpose (R-163).
        #[arg(long = "new-master")]
        new_master: bool,
        #[command(flatten)]
        why: Ladder,
    },
    /// A report: remove a deployment (`destroy platform env=lab`), the
    /// plan against an empty wanted set: every object its state holds is
    /// deleted, dependents first, asked for as `apply` asks. Its state is
    /// left empty and `stack list` no longer shows it; its audit log
    /// stays. `plan --destroy` prints the plan.
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
        #[command(flatten)]
        why: Ladder,
    },
    /// A derivation: how a value was made, one `= EXPRESSION   SITE`
    /// step per expression it passed through (an attribute, an input); a
    /// resource's header and each attribute's steps, and what it waits on
    /// when `later` holds it; any other fact's rule, bindings and the
    /// facts it read, recursively. Variables are allowed; every match is
    /// printed. What the program does not derive (an address, an
    /// attribute, a row), why not: each rule that could have, the first
    /// condition of it that failed and the nearest rows or name.
    /// `why 'deny "MESSAGE"'`: whether that deny holds, and if not, which
    /// clause failed on what.
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
    /// A result set: query the final fact store, a predicate name (every
    /// fact of it) or body literals with variables, one column per
    /// variable: `dform query 'attr(net.vpc, n, .cidr, c)'`.
    Query {
        pattern: String,
        #[command(flatten)]
        target: Target,
        /// Print the answer as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// A report: what the applies since REF did, each change with why it was
    /// planned (as `plan --why`, by the program as it was at that apply),
    /// and which inputs and stated rows changed since the apply before.
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
    /// A result set: run the program's denies over its input space (each enum
    /// input's values, a bool both ways, a key's enum or applied values; the
    /// rest their defaults), once per combination against an empty mock world.
    /// `K=V` and `--set` pin inputs. Fails if any combination is denied,
    /// printing the command that plans it.
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
    /// Controller mode: experimental, listed only with
    /// `DFORM_EXPERIMENTAL=1`.
    #[command(hide = !experimental())]
    Controller {
        #[command(subcommand)]
        cmd: ControllerCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum ControllerCommand {
    /// Wait for a source it read (a table, a document, a program file) or
    /// the world to change, then
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
        /// Per event, stop after this many ticks if changes remain.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum LogCommand {
    /// Check the chain: every entry's hash is its content's and names the
    /// entry before. Fails naming the first broken link.
    Verify {
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum StackCommand {
    /// A result set: every stack of the project, its key, the deployments with
    /// state, and per deployment the last apply (commit, time, actor, from the
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
    /// refuses it. Experimental (R-41), as controller mode is.
    #[command(hide = !experimental())]
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
pub(super) enum StateCommand {
    /// A result set: the deployment's state, each address, its provider and
    /// remote id; with `--address ADDR` (`T["N"]`, as plan prints it), that
    /// object's only.
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
    /// Forget the host key `use ssh` recorded for HOST (as the
    /// program names it, `10.0.0.5` or `name:2222`): the next contact
    /// records the key the host offers then. For a host rebuilt with a new
    /// key.
    ForgetHost {
        host: String,
        #[command(flatten)]
        target: Target,
    },
    /// Give the object at FROM the address TO (each `T["N"]`, as plan
    /// prints it): nothing in the cloud changes, and the next plan sees the
    /// object under TO.
    Mv {
        from: String,
        to: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum SecretsCommand {
    /// A result set: every secret the deployment's program holds, by key:
    /// its kind (random, memo, given, held), generation, age, the cells
    /// that read it and how a new value lands there (update, forces
    /// replace, refused by prevent_destroy). Never a value.
    List {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Rotate one secret, KEY after the deployment (`rotate apps env=lab
    /// synapse-db`): a `random.*` key's generation moves on, a memo
    /// forgets what it keeps, recorded in state and the audit log with who
    /// and when. The next plan changes exactly that value, with the
    /// reason. A given or held secret is rotated where it lives: rotate
    /// says where, and fails.
    Rotate {
        /// [TARGET] [K=V..] KEY
        #[arg(value_name = "TARGET K=V.. KEY", required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// Make a new master, the next epoch, sealed under the passphrase
    /// beside the current one (R-165). No value changes: each secret stays
    /// on the epoch it was derived on until it is rotated, which moves it
    /// to the new one; the apply that moves an epoch's last secret retires
    /// it. Needs `[secrets] passphrase`, and the passphrase.
    Cycle {
        #[command(flatten)]
        target: Target,
    },
    /// Give a secret, NAME after the deployment (`set apps env=lab
    /// admin-pw`): the value is read from stdin, else asked on the
    /// terminal, never an argument; sealed (SOPS's format) to the
    /// deployment's age recipients, and its master's own key where a
    /// passphrase or the key file opens it, into the file of given secrets
    /// its program reads (`set from secrets.decode(io.read(..))`); NAME is
    /// one of its `secret(T)` inputs. Every other value is kept as it is.
    /// The audit log has a `given` entry. Commit the file; the next plan
    /// reads it.
    Set {
        /// [TARGET] [K=V..] NAME
        #[arg(value_name = "TARGET K=V.. NAME", required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// Remove a given secret, NAME after the deployment, from its file.
    Unset {
        /// [TARGET] [K=V..] NAME
        #[arg(value_name = "TARGET K=V.. NAME", required = true, num_args = 1..)]
        words: Vec<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum ProviderCommand {
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
pub(super) enum DevCommand {
    #[command(flatten)]
    Run(Run),
    /// A result set: the stratification of the program (partition graph
    /// strata), a row per node.
    Strata {
        #[command(flatten)]
        target: Target,
    },
    /// A result set: what each scope reads, writes and offers (R-11c): the
    /// stack, each module instance and each pack in use. From the lowered
    /// program and the partition graph; no evaluation.
    Effects {
        #[command(flatten)]
        target: Target,
        /// Print as one JSON document instead of text.
        #[arg(long)]
        json: bool,
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
    /// A resource's desired document, by its address (`T["N"]`, as plan
    /// prints it).
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

impl Args {
    /// The run the command line asks for: `-C` taken, the working project
    /// found, the target resolved to its program file and key.
    pub(super) fn resolve(self) -> Result<Cli> {
        let args = self;
        if let Some(dir) = &args.dir {
            std::env::set_current_dir(dir)
                .map_err(|e| anyhow::anyhow!("-C {}: {e}", dir.display()))?;
        }
        let version = env!("CARGO_PKG_VERSION");
        let project = crate::project::Project::find(Path::new("."), version)?;
        let mut mock = Mock::default();
        let (cmd, target): (Cmd, Option<Target>) = match args.cmd {
            Command::Run(r) => {
                let (cmd, target) = r.into();
                (cmd, Some(target))
            }
            Command::Dev { mock: m, cmd } => {
                mock = m;
                match cmd {
                    DevCommand::Run(r) => {
                        let (cmd, target) = r.into();
                        (cmd, Some(target))
                    }
                    DevCommand::Strata { target } => (Cmd::Strata(Strata), Some(target)),
                    DevCommand::Effects { target, json } => {
                        (Cmd::Effects(Effects { json }), Some(target))
                    }
                    DevCommand::Graph {
                        target,
                        strata,
                        relation,
                    } => (
                        Cmd::Graph(Graph {
                            what: if strata {
                                Some("strata".into())
                            } else {
                                relation
                            },
                        }),
                        Some(target),
                    ),
                    DevCommand::Eval { target } => (Cmd::Eval(Eval), Some(target)),
                    DevCommand::Show { addr, target } => (Cmd::Show(Show { addr }), Some(target)),
                }
            }
            Command::Fmt { paths, check } => (Cmd::Fmt(Fmt { paths, check }), None),
            Command::Doc { target } => (Cmd::Doc(Doc), target.target.is_some().then_some(target)),
            Command::Stack { cmd } => match cmd {
                StackCommand::List => (Cmd::StackList(StackList), None),
                StackCommand::Rekey { stack, pairs } => {
                    let target = Target {
                        target: Some(stack.clone()),
                        keys: vec![],
                    };
                    (Cmd::Rekey(Rekey { stack, pairs }), Some(target))
                }
                StackCommand::Handover { stack, to } => {
                    (Cmd::Handover(Handover { stack, to }), None)
                }
                StackCommand::Unlock { target } => (Cmd::Unlock(Unlock), Some(target)),
            },
            Command::Output { mut target, json } => {
                // The output's NAME is the one word after the target that is
                // not a key value.
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
                (
                    Cmd::Output(Output {
                        name: names.pop(),
                        json,
                    }),
                    Some(target),
                )
            }
            Command::State { cmd } => match cmd {
                StateCommand::Show {
                    addr,
                    from_log,
                    target,
                } => (Cmd::StateShow(StateShow { addr, from_log }), Some(target)),
                StateCommand::ForgetHost { host, target } => {
                    (Cmd::ForgetHost(ForgetHost { host }), Some(target))
                }
                StateCommand::Mv { from, to, target } => {
                    (Cmd::StateMv(StateMv { from, to }), Some(target))
                }
            },
            Command::Secrets { cmd } => match cmd {
                SecretsCommand::List { target, json } => {
                    (Cmd::Secrets(Secrets::List { json }), Some(target))
                }
                SecretsCommand::Rotate { words } => {
                    let (target, key) = target_then(words);
                    (Cmd::Secrets(Secrets::Rotate { key }), Some(target))
                }
                SecretsCommand::Cycle { target } => (Cmd::Secrets(Secrets::Cycle), Some(target)),
                SecretsCommand::Set { words } => {
                    let (target, name) = target_then(words);
                    let remove = false;
                    (Cmd::Secrets(Secrets::Set { name, remove }), Some(target))
                }
                SecretsCommand::Unset { words } => {
                    let (target, name) = target_then(words);
                    let remove = true;
                    (Cmd::Secrets(Secrets::Set { name, remove }), Some(target))
                }
            },
            Command::Provider { cmd } => match cmd {
                ProviderCommand::Check { path } => {
                    (Cmd::ProviderCheck(ProviderCheck { path }), None)
                }
                ProviderCommand::Schema { provider } => {
                    (Cmd::ProviderSchema(ProviderSchema { provider }), None)
                }
            },
            Command::Init { name } => (Cmd::Init(Init { name }), None),
            Command::Completions { shell } => (Cmd::Completions(Completions { shell }), None),
            Command::Complete { words } => (Cmd::Complete(Complete { words }), None),
            Command::ServeProvider { .. } => {
                bail!("internal: `__provider` serves before a project")
            }
            Command::Lsp => bail!("internal: `lsp` serves before a project"),
            Command::Version => bail!("internal: `version` prints before a project"),
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
            style: {
                use std::io::IsTerminal;
                inputs.color.style(std::io::stdout().is_terminal())
            },
            table: Default::default(),
            every_stack: Vec::new(),
            matrix: None,
            held: Held::default(),
        };
        cli.table = report::table::Options {
            width: terminal_width().unwrap_or(report::table::Options::PLAIN.width),
            style: cli.style,
        };
        if let Cmd::Apply(Apply { chaos, .. }) = &mut cli.cmd {
            *chaos = mock.chaos;
        } else if !mock.chaos.is_empty() {
            bail!("--chaos is for apply");
        }
        // A plan file is `apply`'s target: its inputs name the program.
        if let (
            Cmd::Apply(Apply {
                plan_file,
                destroy: false,
                ..
            }),
            Some(t),
        ) = (&mut cli.cmd, &target)
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
        // The project module (R-114): with no target, the root's project.df;
        // a target that is one.
        if let Some(module) = matrix::target(&cli.cmd, project.as_ref(), &target)? {
            cli.matrix = Some(module);
            return Ok(cli);
        }
        // `apply` with no target applies the project: every stack under the
        // working directory, in dependency order, each confirmed on its own.
        if let (Cmd::Apply(Apply { destroy: false, .. }), None, true, Some(p)) = (
            &cli.cmd,
            &target.target,
            target.keys.is_empty(),
            project.as_ref(),
        ) {
            let d = crate::project::discover(p);
            d.check()?;
            let here: Vec<PathBuf> = d
                .stacks
                .iter()
                .filter(|s| s.file.is_relative() && !s.file.starts_with(".."))
                .map(|s| s.file.clone())
                .collect();
            if here.len() > 1 {
                for w in &d.warnings {
                    eprintln!("warning: {w}");
                }
                cli.every_stack = here;
                return Ok(cli);
            }
        }
        let (file, keys) = target_of(project.as_ref(), &target)?;
        cli.set.extend(keys.iter().map(|(k, v)| format!("{k}={v}")));
        cli.keys = keys;
        cli.files = vec![file];
        Ok(cli)
    }
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
pub(super) fn target_of(
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
                    "no stack under {}: a stack is a file under stacks/ (or at the root, in \
                     a project with no stacks/); name a program file \
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
pub(super) fn listing(stacks: &[crate::project::Found]) -> String {
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
pub(super) fn target_then(mut words: Vec<String>) -> (Target, String) {
    let last = words.pop().expect("clap: one word at least");
    let mut words = words.into_iter();
    let target = Target {
        target: words.next(),
        keys: words.collect(),
    };
    (target, last)
}

/// Whether the experimental commands (R-41: `controller`, `stack
/// handover`) are listed in `--help` and completions: `DFORM_EXPERIMENTAL=1`.
/// They run either way, each run with [`EXPERIMENTAL`] on stderr.
pub(super) fn experimental() -> bool {
    std::env::var_os("DFORM_EXPERIMENTAL").is_some_and(|v| v == "1")
}

/// The line an experimental command prints on every run.
pub(super) const EXPERIMENTAL: &str = "warning: controller mode is experimental: its process model (where it \
                            runs, how it is supervised, what an operator sees) is not decided; \
                            see docs/experimental/controller.md";
