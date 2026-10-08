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

/// The width of the terminal stdout is, if it is one.
fn terminal_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let (cols, _) = crossterm::terminal::size().ok()?;
    (cols > 0).then_some(cols as usize)
}

/// How this run reaches its providers (`main`'s `launch`).
static LAUNCH: std::sync::OnceLock<&'static (dyn plugin::Launch + Sync)> =
    std::sync::OnceLock::new();

fn launch() -> &'static dyn plugin::Launch {
    *LAUNCH
        .get()
        .expect("internal: cli::main sets the providers' backend")
}

/// The command line as typed (docs/reference.md). `resolve` turns it into
/// one run, [`Cli`]: the target's files, its key, the project's state.
#[derive(Parser, Debug, Clone)]
#[command(name = "dform")]
#[command(about = "Facts + rules + constraints for infra", long_about = None)]
#[command(after_help = "See dform(1) for the full documentation.")]
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
    /// A stack input: --set region=us-east1, an `@override` that wins over
    /// the input's default and every settings block. A key input's value is
    /// the target's (`dform plan app env=prod`), never --set's.
    #[arg(long = "set", global = true, value_name = "K=V")]
    set: Vec<String>,

    /// Stack inputs from a .df file of facts, one `name(value).` per input
    /// (repeatable). Each is a normal contribution, like a settings block's.
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

    /// Colour the plan and errors: auto (when the output is a terminal and
    /// NO_COLOR is unset), always, never. `--json` and the plan file are
    /// never coloured.
    #[arg(long = "color", global = true, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,
}

/// `--color`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ColorWhen {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorWhen {
    /// Colour output to a stream that is a terminal (`terminal`) or not:
    /// `auto` colours a terminal unless NO_COLOR is set (non-empty).
    fn style(self, terminal: bool) -> report::Style {
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
struct Mock {
    /// A provider: a schema the mock provider plays (a name,
    /// providers/NAME/schema.df else a built-in, or a path to a schema .df
    /// file), or a plugin executable (a path to one, or to a directory
    /// holding a `dform-provider*`). Repeatable; overrides the program's
    /// providers' `use`s.
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
struct Ladder {
    /// Quiet: the bare diff in apply order, each change by its full
    /// address with its values, no site or binding (`--why=none`).
    #[arg(short = 'q', long = "quiet", conflicts_with_all = ["verbose", "why"])]
    quiet: bool,
    /// Say how: `-v` adds the deriving statement's bindings, the
    /// expression behind each value and the writes that lost, with their
    /// ranks (`--why=how`); `-vv` also each change's derivation,
    /// compressed to the facts, table rows, inputs and extern answers it
    /// rests on, one line each (`--why=full`).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count,
          conflicts_with = "why")]
    verbose: u8,
    /// The level by name: `none` (`-q`); `line` (the default): where each
    /// change is derived, where a value written outside its block was
    /// written, the leaf that changed since the last apply; `how` (`-v`);
    /// `full` (`-vv`, `--why` alone).
    #[arg(long, value_name = "LEVEL", default_value = "line",
          num_args = 0..=1, require_equals = true, default_missing_value = "full")]
    why: report::Why,
}

impl Ladder {
    fn level(&self) -> report::Why {
        report::Why::of(self.quiet, self.verbose, self.why)
    }
}

/// The commands that run on a target, at the top level and under `dev`.
#[derive(Subcommand, Debug, Clone)]
enum Run {
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
enum ControllerCommand {
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
enum StateCommand {
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
enum SecretsCommand {
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
    /// How plan text is painted on stdout (`--color`).
    style: report::Style,
    /// How a result set prints on stdout: `style`, at the terminal's width.
    table: report::table::Options,
    /// `apply` with no target in a project of several stacks: every one,
    /// in dependency order (`run_command`).
    every_stack: Vec<PathBuf>,
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

/// One deployment `apply` applies, of the stack in `file`, and the
/// stack's inputs (not its key).
struct Dependency {
    name: String,
    file: PathBuf,
    keys: Vec<(String, String)>,
    inputs: BTreeSet<String>,
}

/// `apply X` in a project: the deployments of the project's stacks X
/// reads (a keyed read of a deployment, R-73), and theirs, each before its
/// readers, then X; nothing that reads X (R-30: the stack is the unit of
/// partial work).
/// Empty when X reads none, and for a plan file, a world fixture or a
/// program outside a project. A cycle is an error naming it.
fn apply_order(cli: &Cli) -> Result<Vec<Dependency>> {
    let Cmd::Apply {
        plan_file: None,
        destroy: false,
        ..
    } = &cli.cmd
    else {
        return Ok(Vec::new());
    };
    if !cli.in_project || cli.world.is_some() {
        return Ok(Vec::new());
    }
    // The project (`apply` with no target): every stack, each with its
    // default key; else the target.
    let roots: Vec<(PathBuf, Vec<(String, String)>)> = match cli.files.as_slice() {
        [] => cli
            .every_stack
            .iter()
            .map(|f| (f.clone(), Vec::new()))
            .collect(),
        [one] => vec![(one.clone(), cli.keys.clone())],
        _ => return Ok(Vec::new()),
    };
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let Some(project) = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?
    else {
        return Ok(Vec::new());
    };
    let found = crate::project::discover(&project);
    // The stack in `file` keyed by `keys`: its name and the deployments it
    // reads that are this project's.
    let reads = |file: &Path,
                 keys: &[(String, String)]|
     -> Result<(String, BTreeSet<String>, Vec<Dependency>)> {
        let t = deployment::Target {
            files: vec![file.to_path_buf()],
            input_files: Vec::new(),
            providers: Vec::new(),
        };
        let loaded = deployment::load(
            &t,
            env!("CARGO_PKG_VERSION"),
            &|p: &Path| std::fs::read_to_string(p),
            &mut deployment::Notes::default(),
        )?;
        // The key the target gives, a key it does not at its default.
        let given: Vec<crate::ast::Atom> = keys
            .iter()
            .map(|(k, v)| {
                crate::ast::atom(
                    "input",
                    vec![crate::ast::str_term(k), crate::ast::str_term(v)],
                    Default::default(),
                )
            })
            .collect();
        let instance = crate::stack::instance(&loaded.cfg, &loaded.stack, &loaded.program, &given)
            .unwrap_or_else(|_| crate::stack::Instance {
                stack: loaded.stack.clone(),
                key: keys.to_vec(),
                defaulted: Vec::new(),
            });
        let (mut names, any) =
            crate::stack::reads(&loaded.program, &loaded.deployed, &instance.key);
        // A deployment named by what the program computes: any of the
        // stack's may be read, so every one there is goes first.
        if !any.is_empty() {
            for name in crate::stack::registry(&project.state_root())?.into_keys() {
                let stack = name.split_once('[').map_or(name.as_str(), |(s, _)| s);
                if any.contains(stack) {
                    names.insert(name);
                }
            }
        }
        let mut deps = Vec::new();
        for name in names {
            let (stack, key) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((s, k)) => (s.to_string(), k),
                None => (name.clone(), ""),
            };
            // Another project's (`acme.platform`) is that project's to apply.
            let [one] = found.named(&stack)[..] else {
                continue;
            };
            let keys = key
                .split(',')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .ok_or_else(|| anyhow::anyhow!("a read of {name}: expected K=V in the key"))
                })
                .collect::<Result<_>>()?;
            deps.push(Dependency {
                name,
                file: one.file.clone(),
                keys,
                inputs: BTreeSet::new(),
            });
        }
        let inputs = loaded
            .program
            .statements
            .iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Input(i) if !i.key => Some(i.name.clone()),
                _ => None,
            })
            .collect();
        Ok((instance.name(), inputs, deps))
    };
    let mut order: Vec<Dependency> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    type Reads<'a> = dyn Fn(&Path, &[(String, String)]) -> Result<(String, BTreeSet<String>, Vec<Dependency>)>
        + 'a;
    fn visit(
        mut d: Dependency,
        reads: &Reads,
        order: &mut Vec<Dependency>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        if order.iter().any(|o| o.name == d.name) {
            return Ok(());
        }
        if let Some(i) = path.iter().position(|p| *p == d.name) {
            bail!(
                "apply {}: the stacks read each other's outputs in a cycle: {} -> {}",
                path[0],
                path[i..].join(" -> "),
                d.name
            );
        }
        let (_, inputs, deps) = reads(&d.file, &d.keys)?;
        d.inputs = inputs;
        path.push(d.name.clone());
        for dep in deps {
            visit(dep, reads, order, path)?;
        }
        path.pop();
        order.push(d);
        Ok(())
    }
    for (file, keys) in &roots {
        let (name, inputs, _) = reads(file, keys)?;
        let target = Dependency {
            name,
            file: file.clone(),
            keys: keys.clone(),
            inputs,
        };
        visit(target, &reads, &mut order, &mut path)?;
    }
    if order.len() == 1 && cli.every_stack.is_empty() {
        order.clear();
    }
    Ok(order)
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
                DevCommand::Effects { target, json } => (Cmd::Effects { json }, Some(target)),
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
                DevCommand::Show { addr, target } => (Cmd::Show { addr }, Some(target)),
            }
        }
        Command::Fmt { paths, check } => (Cmd::Fmt { paths, check }, None),
        Command::Doc { target } => (Cmd::Doc, target.target.is_some().then_some(target)),
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
                Cmd::Output {
                    name: names.pop(),
                    json,
                },
                Some(target),
            )
        }
        Command::State { cmd } => match cmd {
            StateCommand::Show {
                addr,
                from_log,
                target,
            } => (Cmd::StateShow { addr, from_log }, Some(target)),
            StateCommand::ForgetHost { host, target } => (Cmd::ForgetHost { host }, Some(target)),
            StateCommand::Mv { from, to, target } => (Cmd::StateMv { from, to }, Some(target)),
        },
        Command::Secrets { cmd } => match cmd {
            SecretsCommand::List { target, json } => (Cmd::SecretsList { json }, Some(target)),
            SecretsCommand::Rotate { words } => {
                let (target, key) = target_then(words);
                (Cmd::SecretsRotate { key }, Some(target))
            }
            SecretsCommand::Cycle { target } => (Cmd::SecretsCycle, Some(target)),
            SecretsCommand::Set { words } => {
                let (target, name) = target_then(words);
                let remove = false;
                (Cmd::SecretsSet { name, remove }, Some(target))
            }
            SecretsCommand::Unset { words } => {
                let (target, name) = target_then(words);
                let remove = true;
                (Cmd::SecretsSet { name, remove }, Some(target))
            }
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
    };
    cli.table = report::table::Options {
        width: terminal_width().unwrap_or(report::table::Options::PLAIN.width),
        style: cli.style,
    };
    if let Cmd::Apply { chaos, .. } = &mut cli.cmd {
        *chaos = mock.chaos;
    } else if !mock.chaos.is_empty() {
        bail!("--chaos is for apply");
    }
    // A plan file is `apply`'s target: its inputs name the program.
    if let (
        Cmd::Apply {
            plan_file,
            destroy: false,
            ..
        },
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
    // `apply` with no target applies the project: every stack under the
    // working directory, in dependency order, each confirmed on its own.
    if let (Cmd::Apply { destroy: false, .. }, None, true, Some(p)) = (
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

/// A command that runs on a target, and the target.
fn run_cmd(r: Run) -> (Cmd, Option<Target>) {
    match r {
        Run::Plan {
            target,
            out,
            json,
            destroy,
            new_master,
            why,
        } => (
            Cmd::Plan {
                out,
                json,
                destroy,
                why: why.level(),
                new_master,
            },
            Some(target),
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
            Cmd::Apply {
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
            },
            Some(target),
        ),
        Run::Destroy {
            target,
            max_ticks,
            parallel,
            approval,
            yes,
            why,
        } => (
            Cmd::Apply {
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
            },
            Some(target),
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
            Cmd::Why {
                pattern,
                tree,
                all,
                core,
                whole: verbose >= 2,
                json,
            },
            Some(target),
        ),
        Run::Query {
            pattern,
            target,
            json,
        } => (Cmd::Query { pattern, json }, Some(target)),
        Run::Diff {
            target,
            since,
            json,
            why,
        } => (
            Cmd::Diff {
                since,
                json,
                why: why.level(),
            },
            Some(target),
        ),
        Run::Explain { target, addresses } => (Cmd::Explain { addresses }, Some(target)),
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
        match verbose {
            true => println!("deployment: {}", located.instance.describe()),
            false => println!("deployment: {}", located.instance.name()),
        }
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
        // A secret answer the plan read (`ssh.read`), read again: by its
        // digest. One that is "not yet" now is waited on, not compared.
        let now: std::collections::BTreeMap<String, serde_json::Value> =
            answer_inputs(externs, key)
                .into_iter()
                .filter_map(|a| {
                    Some((
                        a.get("sensitive")?.as_str()?.to_string(),
                        a["digest"].clone(),
                    ))
                })
                .collect();
        for a in &saved.inputs.answers {
            let Some(label) = a.get("sensitive").and_then(|l| l.as_str()) else {
                continue;
            };
            if now.get(label).is_some_and(|d| *d != a["digest"]) {
                diff.push(format!("{label}: changed since the plan"));
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
                print!("{}", rendered(&report));
                print!("{}", unreachable_text(&unreachable));
                // Who each secret output no provider holds is sealed to:
                // the grant (R-166).
                let unheld = crate::stack::unheld_secret_outputs(
                    &res.facts,
                    &crate::stack::secret_output_types(program),
                );
                if !unheld.is_empty() && cli.world.is_none() {
                    let readers = readers_of(&root, &deployment, &open_s3(&root, false))?;
                    print!("{}", grants_text(&unheld, &readers));
                }
                // The digest to approve, when a change is held for an
                // approval (the bare diff lists those changes after it);
                // a plan file's digest is on stderr beside its path.
                if let Some(f) = file.as_ref().filter(|f| !f.needs_approval.is_empty()) {
                    if why == report::Why::None {
                        print!("{}", needs_text(&f.needs_approval));
                    }
                    println!("plan digest: {}", f.digest.as_deref().unwrap_or_default());
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

/// The nulls a tick waits on that waiting can resolve (R-81,
/// `deployment::waitable`): of the stuck rules' and the held resources'.
fn waiting_on(
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
fn log_retries(
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

/// The project's root, else the program's directory: what a site's place
/// is relative to.
fn site_root(files: &[PathBuf]) -> Option<PathBuf> {
    files.first().and_then(|f| {
        let f = std::path::absolute(f).ok()?;
        crate::project::manifest_root(&f).or_else(|| f.parent().map(Path::to_path_buf))
    })
}

/// How `diff` evaluates the program at an earlier commit: this executable,
/// on the same program file (relative to the project root) and key
/// values `key`, with this run's mock flags.
fn rerun_of(cli: &Cli, key: &[(String, String)], keys: Vec<String>) -> Result<crate::diff::Rerun> {
    let file = std::path::absolute(&cli.files[0])?;
    let top = crate::project::manifest_root(&file)
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let mut target = vec![
        file.strip_prefix(&top)
            .unwrap_or(&file)
            .display()
            .to_string(),
    ];
    target.extend(key.iter().map(|(k, v)| format!("{k}={v}")));
    // A path given relative to here is absolute there.
    let path = |p: &Path| -> String {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    let mut dev = Vec::new();
    if let Some(w) = &cli.world {
        dev.extend(["--world".to_string(), path(w)]);
    }
    if let Some(i) = &cli.inventory {
        dev.extend(["--inventory".to_string(), path(i)]);
    }
    for p in &cli.providers {
        let p = if Path::new(p).exists() {
            path(Path::new(p))
        } else {
            p.clone()
        };
        dev.extend(["--provider".to_string(), p]);
    }
    if !dev.is_empty() {
        dev.insert(0, "dev".into());
    }
    Ok(crate::diff::Rerun {
        exe: std::env::current_exe()?,
        top,
        dev,
        target,
        keys,
    })
}

/// A `query` or `why` pattern reads the schema's predicates: the whole
/// schema is asked for.
fn reads_schema(pattern: &str) -> bool {
    match query::parse(pattern) {
        Ok(query::Query::Pred(p)) => schema::is_schema_pred(&p),
        Ok(query::Query::Body { body, .. }) => body.iter().any(|l| {
            matches!(l, crate::ast::Lit::Pos(a) | crate::ast::Lit::Not(a)
                if schema::is_schema_pred(&a.pred))
        }),
        Err(_) => false,
    }
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

/// `dform controller run`: a run per event (`controller::Hook`), until
/// `--once` or `--max-events` says stop. A run that fails is logged and the
/// controller goes on watching; the first one failing ends it.
fn run_controller(cli: Cli) -> Result<Outcome> {
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

/// Ask on the terminal whether to apply `n` changes to `deployment`
/// (`destroy`: to delete its `n` objects), or (`new`) tick `tick`, which
/// adds what no plan listed: only `y` or `yes` proceeds.
/// With no terminal to ask on, a refusal naming `--yes`, never a wait.
/// Answered no, `false`: a decline, not an error.
fn confirm(
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
fn confirm_emptied(e: &zset::Emptied, deployment: &str, style: report::Style) -> Result<bool> {
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
fn still_held(session: &Option<Session>) -> Result<()> {
    match session {
        Some(s) => s.lock.check(),
        None => Ok(()),
    }
}

/// An apply whose confirmation of `tick` was answered no: at tick 1
/// nothing was applied, and nothing is said; later, what the earlier ticks
/// did. The audit log's `apply_end` says `declined`, and at which tick.
fn declined(deployment: &str, tick: usize) -> Outcome {
    let why = (tick > 1).then(|| {
        format!(
            "apply {deployment}: not confirmed at tick {tick}; ticks 1 to {} were applied, \
             and the next apply resumes from there",
            tick - 1
        )
    });
    Outcome::Declined { tick, why }
}

fn stopped(s: Stopped) -> Outcome {
    Outcome::Stopped {
        tick: s.tick,
        why: s.to_string(),
    }
}

/// An apply of a plan file or an approval that stopped after `tick`: the
/// next tick adds `new` changes no printed plan named (`unnamed`, the
/// groups the last plan held them as). The audit log's `apply_end` says `stopped`.
#[derive(Debug)]
struct Stopped {
    tick: usize,
    new: usize,
    unnamed: Vec<String>,
    /// The changes are what `later` held waiting on a provider's settings,
    /// named but planned only now (R-45): the plan file or approval did
    /// not see their diff.
    on_provider: bool,
    /// The tick re-planned at its boundary differs from the tick the plan
    /// file or approval showed (After R-156): what it differs in, each an
    /// address or an attribute as the plan prints it.
    differs: Vec<String>,
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
            "stack rekey {stack}: the stack has no key; key it first (`key env: ..` in its file)"
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

/// `dform test` (R-32): the program's denies over its input space
/// (`testing::space`), each combination evaluated against an empty mock
/// world (the provider's schema, no world, no state). A result set (R-63):
/// a row per combination, its inputs and its result. A combination fails
/// when anything is denied or it does not compile, printed after the
/// matrix as the command that plans it.
fn run_tests(
    program: &crate::ast::Program,
    stack: &str,
    providers: &[String],
    none: bool,
    cli: &Cli,
    files: &[PathBuf],
) -> Result<()> {
    use std::io::IsTerminal;
    let backend = match none {
        true => {
            let p = Providers::none();
            p.load_schema(None)?;
            p
        }
        false => Providers::start(launch(), providers, &plugin::Config::default())?,
    };
    let lowered = crate::transform::lower(program)?;
    crate::secrets::check(&lowered, backend.schema(), &Default::default())?;
    crate::refine::check(&lowered.program, backend.schema())?;
    crate::infer::infer(
        &lowered.program,
        &lowered.extern_fns,
        &lowered.inputs,
        &lowered.declared,
        Some(backend.schema()),
    )?;
    // What the target and `--set` pin, as given.
    let mut pinned: Vec<(String, Value)> = Vec::new();
    for kv in &cli.set {
        let (k, v) = split_kv(kv)?;
        pinned.push((k.to_string(), v));
    }
    let names: Vec<String> = pinned.iter().map(|(k, _)| k.clone()).collect();
    let applied = |key: &str| -> Vec<String> {
        let mut out: Vec<String> = crate::stack::registry(&cli.root)
            .unwrap_or_default()
            .into_keys()
            .filter_map(|n| {
                let (s, seg) = n.strip_suffix(']')?.split_once('[')?;
                (s == stack).then_some(())?;
                seg.split(',')
                    .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key))
                    .map(|(_, v)| v.to_string())
            })
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let axes = crate::testing::space(&lowered.inputs, &names, &applied)?;
    let combinations = crate::testing::combinations(&axes);
    let keys: BTreeSet<&str> = lowered
        .inputs
        .iter()
        .filter(|d| d.scope.is_empty() && d.decl.key)
        .map(|d| d.decl.name.as_str())
        .collect();
    let run = |pairs: &[(String, Value)]| -> Result<Vec<String>> {
        let mut given = deployment::input_fact_keys(program);
        given.extend(pairs.iter().map(|(k, _)| k.clone()));
        inputs::check_required(&lowered.inputs, &given)?;
        let mut extra = inputs::set_facts(&lowered.inputs, pairs)?;
        extra.extend(cli.manifest.iter().flat_map(|m| m.facts()));
        extra.extend(build_extra_facts(&cli.data)?);
        extra.extend(backend.catalog(schema::named_types(&lowered.program, &extra).as_ref())?);
        let tables = crate::tables::Tables::default();
        // Nothing is kept and nothing applied: a memo answers its
        // candidate, `random.*` derive from a master of the test's own.
        let memos =
            crate::memo::Memos::new(&state::State::default(), &crate::custody::Master::none());
        crate::functions::random::set_master(Some(b"dform test".to_vec()), None, stack);
        let externs =
            crate::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
                if let Some(r) = tables.answer(f, ins) {
                    return r;
                }
                if let Some(r) = crate::externs::time(f) {
                    return r;
                }
                if let Some(r) = memos.answer(f, ins) {
                    return r;
                }
                backend.query_extern(f, ins)
            });
        let mut p = zset::with_policy_rules(program.clone())?;
        // Quantity and time literals read as their attributes' types (R-66).
        crate::types::read(&mut p, backend.schema())?;
        let (res, mut violations) = externs.eval(&p, &extra)?;
        violations.extend(inputs::violations(&res.facts, &lowered.inputs));
        let redact = query::Redactor::new(&res.facts, backend.schema());
        Ok(violations.iter().map(|v| redact.text(v)).collect())
    };
    // A deny's doc comment (`#|` above it) is its test's doc, printed
    // beside it (R-30: with `scenario` gone, the deny is the test).
    let docs: std::collections::BTreeMap<String, String> = program
        .statements
        .iter()
        .filter_map(|st| match st {
            crate::ast::Stmt::Fact(a) if a.pred == "doc" => match a.args.as_slice() {
                [
                    Term::Val(Value::Str(kind)),
                    Term::Val(Value::Str(name)),
                    Term::Val(Value::Str(key)),
                    Term::Val(Value::Str(text)),
                ] if kind == "rule" && key == "description" => Some((name.clone(), text.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let documented = |d: &String| -> String {
        match docs
            .iter()
            .filter(|(name, _)| d == *name || d.starts_with(&format!("{name} ")))
            .max_by_key(|(name, _)| name.len())
        {
            Some((_, doc)) => format!("- {d}   #| {doc}"),
            None => format!("- {d}"),
        }
    };
    let n = combinations.len();
    let s = if n == 1 { "" } else { "s" };
    let over = if axes.is_empty() {
        String::new()
    } else {
        let mut names: Vec<&str> = axes.iter().map(|a| a.input.as_str()).collect();
        names.dedup();
        format!(" of {}", names.join(", "))
    };
    println!("test {stack}: {n} combination{s}{over}");
    // The matrix: a row per combination, its inputs then its result; each
    // that failed is then printed as the command that plans it, with its
    // denies or its error.
    let mut matrix: Option<report::table::Table> = None;
    let mut failures: Vec<(String, Vec<String>)> = Vec::new();
    for combination in &combinations {
        let mut pairs = pinned.clone();
        pairs.extend(combination.iter().cloned());
        let text = |(k, v): &(String, Value)| (k.clone(), partition::fmt_bare(v));
        let (on_target, set): (Vec<_>, Vec<_>) = pairs
            .iter()
            .map(text)
            .partition(|(k, _)| keys.contains(k.as_str()));
        let target = if cli.in_project {
            stack.to_string()
        } else {
            files[0].display().to_string()
        };
        let command = crate::testing::reproduce(&target, &on_target, &set);
        let (result, lines) = match run(&pairs) {
            Ok(denied) if denied.is_empty() => ("ok", Vec::new()),
            Ok(denied) => ("denied", denied.iter().map(documented).collect()),
            Err(e) => {
                let text = crate::diag::report(&e, std::io::stdout().is_terminal());
                ("error", text.lines().map(String::from).collect())
            }
        };
        // A column per input; a dependent input not declared in a
        // combination (R-104) is `-` there.
        let mut columns: Vec<String> = pinned.iter().map(|(k, _)| k.clone()).collect();
        for a in &axes {
            if !columns.contains(&a.input) {
                columns.push(a.input.clone());
            }
        }
        let mut row: Vec<report::table::Cell> = columns
            .iter()
            .map(|c| match pairs.iter().find(|(k, _)| k == c) {
                Some((_, v)) => report::table::Cell::text(partition::fmt_bare(v)),
                None => report::table::Cell::text("-"),
            })
            .collect();
        let t = matrix.get_or_insert_with(|| {
            let mut columns = columns.clone();
            columns.push("result".into());
            report::table::Table::new(columns)
        });
        let cell = report::table::Cell::text(result);
        row.push(match result {
            "ok" => cell,
            _ => cell.painted(report::Paint::Error),
        });
        t.push(row);
        if result != "ok" {
            failures.push((format!("{result}  {command}"), lines));
        }
    }
    if let Some(t) = matrix {
        print!("{}", t.render(&cli.table));
    }
    for (head, lines) in &failures {
        println!("{head}");
        for l in lines {
            println!("  {l}");
        }
    }
    let failed = failures.len();
    println!("test {stack}: {n} combination{s}, {failed} failed");
    if failed > 0 {
        bail!("{failed} of {n} combination{s} failed");
    }
    Ok(())
}

/// When the deployment's master was first applied, from its audit log: its
/// first `master` entry, else (a deployment applied before they were
/// logged) its first apply.
fn born(entries: Option<&Vec<serde_json::Value>>) -> Option<String> {
    let es = entries?;
    es.iter()
        .find(|e| e["kind"] == "master")
        .or_else(|| es.iter().find(|e| e["kind"] == "apply_start"))
        .and_then(|e| e["time"].as_str().map(str::to_string))
}

/// `dform secrets list` (R-161): each secret by key, never a value.
/// A deployment as a command names it: `apps env=lab`.
fn written(i: &crate::stack::Instance) -> String {
    let mut out = i.stack.clone();
    for (k, v) in &i.key {
        out.push_str(&format!(" {k}={v}"));
    }
    out
}

/// The program's secret inputs, each by its address (`admin_pw`,
/// `db.password`) and the type inside `secret(..)`.
fn secret_inputs_of(declared: &[crate::inputs::Declared]) -> Vec<(String, crate::ast::TypeExpr)> {
    declared
        .iter()
        .filter_map(|d| match &d.decl.ty {
            crate::ast::TypeExpr::Apply(n, xs) if n == "secret" && xs.len() == 1 => {
                Some((d.address.clone()?, xs[0].clone()))
            }
            _ => None,
        })
        .collect()
}

/// Each given secret a file of them holds (R-108): where it lives, its
/// generation and when it was set.
fn given_rows(
    list: &mut [crate::secrets::inventory::Secret],
    files: &[crate::custody::given::Read],
    mixing: &crate::custody::Mixing,
) {
    for r in files {
        let Some(f) = &r.file else { continue };
        for l in &f.leaves {
            let name = l.name();
            let Some(s) = list.iter_mut().find(|s| s.key == name) else {
                continue;
            };
            s.lives = Some(format!(
                "{}, {}",
                r.shown,
                crate::custody::given::sealed_to(f, &|k| mixing.name_of(k))
            ));
            if let Some(g) = f.given(&name) {
                s.generation = g.generation;
                s.since = Some(g.at.clone());
            }
        }
    }
}

/// A file of given secrets, as `secrets list` ends: its values and who
/// opens it.
fn print_given_file(r: &crate::custody::given::Read, mixing: &crate::custody::Mixing) {
    match &r.file {
        None => println!(
            "{}: not written yet: `dform secrets set` writes it",
            r.shown
        ),
        Some(f) => println!(
            "{}: {} given secret{}, {}",
            r.shown,
            f.leaves.len(),
            if f.leaves.len() == 1 { "" } else { "s" },
            crate::custody::given::sealed_to(f, &|k| mixing.name_of(k))
        ),
    }
}

/// `dform secrets set NAME` and `unset` (R-108): the value read from stdin
/// or the terminal, sealed into the file of given secrets the program
/// reads, every other value kept; a `given` entry in the audit log.
fn secrets_set(
    deployment: &str,
    secret: &[(String, crate::ast::TypeExpr)],
    name: &str,
    remove: bool,
    files: &[crate::custody::given::Read],
    (mixing, master): (&crate::custody::Mixing, &crate::custody::Master),
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::custody::given;
    let verb = match remove {
        true => "unset",
        false => "set",
    };
    let holds = |r: &&given::Read| {
        r.file
            .as_ref()
            .is_some_and(|f| f.leaves.iter().any(|l| l.name() == name))
    };
    let read = match files {
        [] => bail!(
            "secrets {verb} {name}: {deployment} reads no file of given secrets; read one into \
             its secret inputs, `set from secrets.decode(io.read(\"secrets/{}.json\"))`",
            deployment.split('[').next().unwrap_or(deployment)
        ),
        [r] => r,
        rs => match rs.iter().find(holds) {
            Some(r) => r,
            None => bail!(
                "secrets {verb} {name}: {deployment} reads {} files of given secrets ({}), and \
                 none holds {name}; give it in one of them with sops, then set it here",
                rs.len(),
                rs.iter()
                    .map(|r| r.shown.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    let ty = secret.iter().find(|(a, _)| a == name).map(|(_, t)| t);
    if !remove && ty.is_none() {
        bail!(
            "secrets set {name}: {name} is not a secret input of {deployment} ({}); declare it \
             `input {name}: secret(string)`",
            match secret.is_empty() {
                true => "it declares none".to_string(),
                false => format!(
                    "its secret inputs: {}",
                    secret
                        .iter()
                        .map(|(a, _)| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        );
    }
    let Some(path) = &read.path else {
        bail!(
            "secrets {verb} {name}: {} is not a file of the project, and `secrets {verb}` writes \
             one; read a project file, `io.read(\"secrets/..\")`",
            read.shown
        );
    };
    let stack_key = match given::to_master(mixing) {
        false => None,
        true => match &master.digest {
            Some(k) => Some(given::stack_recipient(k)),
            None => bail!(
                "secrets {verb} {name}: {} is sealed to {deployment}'s master, which this run \
                 does not hold ({})",
                read.shown,
                master.without.as_deref().unwrap_or("no master")
            ),
        },
    };
    let to = given::To {
        recipients: mixing.recipients.iter().map(|r| r.key.clone()).collect(),
        stack_key,
    };
    // The file as it is, every value opened.
    let file = read.file.clone().unwrap_or_default();
    let (values, key) = match &read.file {
        None => (Vec::new(), None),
        Some(f) => {
            let ids = given::identities()?;
            let Some(k) = given::data_key(f, &ids)? else {
                bail!(
                    "secrets {verb} {name}: {}: {}",
                    read.shown,
                    given::why_not(f, &ids, &|r| mixing.name_of(r))
                );
            };
            (given::open(f, &k, &read.shown)?, Some(k))
        }
    };
    if remove && !values.iter().any(|(l, _)| l.name() == name) {
        bail!(
            "secrets unset {name}: {} gives no {name} ({})",
            read.shown,
            match values.is_empty() {
                true => "it gives none".to_string(),
                false => format!(
                    "it gives {}",
                    values
                        .iter()
                        .map(|(l, _)| l.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        );
    }
    let plain = match (remove, ty) {
        (false, Some(ty)) => Some(given::typed(
            name,
            ty,
            given::ask(&format!("{name} of {deployment}"))?,
        )?),
        _ => None,
    };
    let who = crate::audit::who();
    let next = given::with(
        &file,
        (&values, key),
        name,
        plain,
        &to,
        (&who, &crate::memo::now()),
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("make the directory of {}", read.shown))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, given::text(&next)?)
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("write {}", read.shown))?;
    let generation = next.given(name).map(|g| g.generation);
    audit.append(
        "given",
        serde_json::json!({
            "key": name,
            "file": read.shown,
            "generation": generation,
            "removed": remove,
            "recipients": next
                .recipients()
                .iter()
                .map(|k| match next.stack_key() == Some(k.as_str()) {
                    true => "the deployment's master".to_string(),
                    false => mixing.name_of(k),
                })
                .collect::<Vec<_>>(),
            "who": who,
        }),
    )?;
    match generation {
        Some(g) => println!(
            "sealed {name} of {deployment} into {} (generation {g}), {}; commit it: the next plan \
             reads it",
            read.shown,
            given::sealed_to(&next, &|k| mixing.name_of(k))
        ),
        None => println!(
            "removed {name} of {deployment} from {}; commit it: the next plan reads it",
            read.shown
        ),
    }
    Ok(())
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

fn secrets_list(
    deployment: &str,
    list: &[crate::secrets::inventory::Secret],
    current: u32,
    json: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    let mut t = Table::new([
        "key",
        "kind",
        "generation",
        "epoch",
        "age",
        "read by",
        "lands",
    ]);
    for s in list {
        // A given secret's from its file (R-108), which says when it was
        // set.
        let generation = match s.kind {
            crate::secrets::inventory::Kind::Random | crate::secrets::inventory::Kind::Memo => {
                s.generation.to_string()
            }
            crate::secrets::inventory::Kind::Given if s.since.is_some() => s.generation.to_string(),
            _ => String::new(),
        };
        let cells: Vec<String> = s.cells.iter().map(|c| c.to_string()).collect();
        let read = match cells.is_empty() {
            true => s
                .lives
                .clone()
                .map(|l| format!("(lives in {l})"))
                .unwrap_or_default(),
            false => cells.join(", "),
        };
        t.push(vec![
            Cell::text(s.key.clone()),
            Cell::text(s.kind.word()),
            Cell::text(generation.clone()).with_json(match s.generation {
                _ if generation.is_empty() => serde_json::Value::Null,
                g => g.into(),
            }),
            // The master epoch (R-165), once there is more than one.
            Cell::text(match (s.epoch, current) {
                (Some(e), c) if c > 1 && e < c => format!("{e} (earlier)"),
                (Some(e), c) if c > 1 => e.to_string(),
                _ => String::new(),
            })
            .with_json(s.epoch.into()),
            Cell::text(
                s.since
                    .as_deref()
                    .map(crate::secrets::inventory::age)
                    .unwrap_or_default(),
            )
            .with_json(s.since.clone().into()),
            Cell::text(read).with_json(cells.into()),
            Cell::text(s.lands().map(|l| l.words()).unwrap_or_default()),
        ]);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&t.json())?);
        return Ok(());
    }
    println!(
        "{deployment}: {} secret{}",
        list.len(),
        if list.len() == 1 { "" } else { "s" }
    );
    print!("{}", t.without_empty_columns().render(o));
    let earlier = list
        .iter()
        .filter(|s| s.epoch.is_some_and(|e| e < current))
        .count();
    if earlier > 0 {
        println!(
            "epoch {current} is current; {earlier} secret{} on an earlier epoch: rotate each to \
             move it",
            if earlier == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Who opens an epoch of the master, and who could: `secrets list`'s
/// lines after the table, the offboarding list.
fn print_holders(h: &crate::custody::Holders) {
    let which = match h.current {
        true => "current",
        false => "earlier",
    };
    println!(
        "master epoch {} ({which}, id {}): opens with {}",
        h.epoch,
        crate::report::short_id(&h.id),
        match h.opens.is_empty() {
            true => "nothing dform.toml names".to_string(),
            false => h.opens.join(", "),
        }
    );
    if !h.could.is_empty() {
        println!(
            "  could also be opened by {} (removed since it began): each secret on it is theirs \
             until rotated off it",
            h.could
                .iter()
                .map(|(n, at)| format!("{n} (removed {at})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Recipients as an audit entry names them: `{key, name}`.
fn recipients_json(rs: &[crate::custody::Recipient]) -> serde_json::Value {
    rs.iter()
        .map(|r| serde_json::json!({ "key": r.key, "name": r.name }))
        .collect()
}

/// `dform secrets rotate KEY` (R-161): the key's generation moves on (a
/// memo forgets what it keeps), in state under its lock and in the audit
/// log; what reads it, and how a new value lands there, said first.
fn secrets_rotate(
    dep: &crate::store::Deployment,
    list: &[crate::secrets::inventory::Secret],
    kept: &std::collections::BTreeMap<String, crate::memo::Kept>,
    key: &str,
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::secrets::inventory::Kind;
    let deployment = dep.name();
    // A memo state keeps that is no secret (a time, a name) is forgotten
    // the same way.
    let plain;
    let found = match list.iter().find(|s| s.key == key) {
        Some(s) => Some(s),
        None if kept.contains_key(key) => {
            plain = crate::secrets::inventory::Secret::memo(key);
            Some(&plain)
        }
        None => None,
    };
    let Some(s) = found else {
        let keys: Vec<&str> = list
            .iter()
            .filter(|s| matches!(s.kind, Kind::Random | Kind::Memo))
            .map(|s| s.key.as_str())
            .collect();
        bail!(
            "secrets rotate {key}: {deployment} has no secret {key} (its keys: {}); a key the \
             program no longer derives is no secret to rotate",
            match keys.is_empty() {
                true => "none".to_string(),
                false => keys.join(", "),
            }
        );
    };
    if let (Kind::Given | Kind::Held, lives) = (s.kind, &s.lives) {
        bail!(
            "secrets rotate {key}: {key} is {} in {deployment}, and lives in {}: rotate it \
             there, then plan",
            s.kind.word(),
            lives.as_deref().unwrap_or("its source")
        );
    }
    if !dep.has_state()? {
        bail!(
            "secrets rotate {key}: {deployment} was never applied: its secrets are new at its \
             first apply"
        );
    }
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    let who = crate::audit::who();
    let (r, memo) = st.rotate(key, &crate::memo::now(), &who);
    // What the next plan changes, and how (R-161's blast radius).
    println!(
        "rotating {key} of {deployment} ({}): generation {} -> {}",
        s.kind.word(),
        r.generation - 1,
        r.generation
    );
    for c in &s.cells {
        println!("  {c}  {}", c.lands.words());
    }
    if memo.is_some() {
        println!("  forgot what memo.first keeps: the next apply keeps its candidate");
    } else if s.cells.is_empty() {
        println!("  (nothing reads it)");
    }
    dep.save_state(&st)?;
    audit.append(
        "rotated",
        serde_json::json!({
            "key": key,
            "kind": s.kind.word(),
            "generation": r.generation,
            "memo": memo.is_some(),
            "who": who,
        }),
    )?;
    println!(
        "rotated {key} of {deployment}: generation {}, by {who}; the next plan changes it",
        r.generation
    );
    lock.release()
}

/// `dform secrets cycle` (R-165): each `random.*` key pinned to the epoch
/// it derives from, then a new master made the next epoch, recorded in
/// state and the audit log. No value changes.
fn secrets_cycle(
    dep: &crate::store::Deployment,
    list: &[crate::secrets::inventory::Secret],
    master: &crate::custody::Master,
    mixing: &crate::custody::Mixing,
    audit: &crate::audit::Log,
) -> Result<()> {
    use crate::secrets::inventory::Kind;
    let deployment = dep.name();
    if !dep.has_state()? {
        bail!("secrets cycle: {deployment} was never applied: its first apply makes its master");
    }
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    let from = master.epoch.max(1);
    // Pinned first: a key pinned to the epoch it derives from is the same
    // value, so a cycle stopped here changes nothing.
    let pinned = st.pin(
        list.iter()
            .filter(|s| s.kind == Kind::Random)
            .map(|s| s.key.clone()),
        from,
    );
    dep.save_state(&st)?;
    let (epoch, id) = crate::custody::cycle(dep.store().as_ref(), deployment, master, mixing)?;
    let was = st.master.replace(id.clone());
    dep.save_state(&st)?;
    audit.append(
        "cycled",
        serde_json::json!({
            "from": was,
            "to": id,
            "epoch": epoch,
            "pinned": pinned,
            "who": crate::audit::who(),
        }),
    )?;
    let on: Vec<&str> = list
        .iter()
        .filter(|s| matches!(s.kind, Kind::Random | Kind::Memo))
        .map(|s| s.key.as_str())
        .collect();
    println!(
        "cycled the master of {deployment}: epoch {epoch} (id {}) is current, for new secrets and \
         each one rotated; {} stay{} on epoch {from} until rotated{}",
        crate::report::short_id(&id),
        match on.len() {
            1 => "1 secret".to_string(),
            n => format!("{n} secrets"),
        },
        if on.len() == 1 { "s" } else { "" },
        match on.is_empty() {
            true => String::new(),
            false => format!(": {}", on.join(", ")),
        }
    );
    lock.release()
}

fn forget_host(
    dep: &crate::store::Deployment,
    host: &str,
    audit: &crate::audit::Log,
) -> Result<()> {
    let deployment = dep.name();
    if !dep.has_state()? {
        bail!(
            "forget-host {host}: stack {deployment} has no state at {}",
            dep.locate(store::STATE)
        );
    }
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    let Some(was) = st.forget_host(host) else {
        let known: Vec<&str> = st.known_hosts.keys().map(String::as_str).collect();
        bail!(
            "forget-host {host}: stack {deployment} records no key for {host} (it knows {})",
            match known.is_empty() {
                true => "none".to_string(),
                false => known.join(", "),
            }
        );
    };
    dep.save_state(&st)?;
    audit.append(
        "state_forget_host",
        serde_json::json!({
            "host": host,
            "fingerprint": was.fingerprint,
            "who": crate::audit::who(),
        }),
    )?;
    println!(
        "forgot the {} key {} of {host} in stack {deployment}: the next contact records the key it meets",
        was.key_type, was.fingerprint
    );
    lock.release()
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

/// The backend of the project's stack `found`, as the manifest says;
/// `None` when it says none.
fn stack_backend(found: &crate::project::Found) -> Option<crate::stack::Backend> {
    let program = loader::load_program(std::slice::from_ref(&found.file)).ok()?;
    crate::stack::config(&program).ok()?.backend
}

/// Where the deployment `name` (`app`, `app[env=prod]`) is, for a command
/// that runs no program: where the registry has it, else where its stack's
/// program's backend says, else under the state root. With its default
/// directory (its world's when its state is in a bucket) and the lease
/// times.
/// Before `stack handover NAME --to s3(..)`: a bucket never holds a key
/// file (R-164), so a deployment whose master is one is sealed first, as
/// dform.toml's `[secrets]` says (the same master: nothing derived
/// changes), and the sealed master is what moves; with no `[secrets]` the
/// handover is refused, naming the setting.
fn seal_before_handover(
    cli: &Cli,
    name: &str,
    from: &crate::stack::Place,
    to: &str,
    s3: store::OpenS3,
    times: store::LeaseTimes,
) -> Result<()> {
    if !to.trim_start().starts_with("s3") {
        return Ok(());
    }
    let src = from.location.open(s3)?;
    if crate::zset::file::Key::load(src.as_ref())?.is_none() {
        return Ok(());
    }
    let stack = name.split_once('[').map_or(name, |(s, _)| s);
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let mixing = crate::custody::Mixing::of(project.as_ref().map(|p| &p.manifest), stack)?;
    if mixing.key_file() {
        bail!(
            "handover {name} to {to}: its master is the key file {}, and a bucket never holds \
             one (read access to the state would be read access to every derived secret): set \
             `[secrets] passphrase = \"env:NAME\"` (or `recipients = [\"age1..\"]`) in \
             dform.toml, and the handover seals it",
            src.locate(store::KEY)
        );
    }
    let dep = store::Deployment::new(src.clone(), name, times);
    let master = dep.master(&mixing, crate::custody::Want::default())?;
    if master.key.is_none() {
        bail!(
            "handover {name} to {to}: sealing its key file needs the master: {}",
            master.without.as_deref().unwrap_or("not held")
        );
    }
    match crate::custody::reseal(src.as_ref(), name, &master, &mixing)? {
        Some(done) if done.key_file => {
            dep.audit(cli.audit_sink.clone(), crate::audit::SINK_TIMEOUT, false)
                .append(
                    "custody",
                    serde_json::json!({
                        "sealed": store::KEY,
                        "into": store::MASTER,
                        "id": master.id,
                        "who": crate::audit::who(),
                    }),
                )?;
            eprintln!(
                "{name}: its key file is sealed into {} for the handover ({})",
                store::MASTER,
                mixing.describe()
            );
            Ok(())
        }
        _ => bail!(
            "handover {name} to {to}: its key file could not be sealed ({}): a bucket never holds \
             one",
            mixing.describe()
        ),
    }
}

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
                    [one] => stack_backend(one),
                    _ => None,
                }
            });
            deployment::deployment_location(root, stack, backend.as_ref(), seg)
        }
    };
    let place = crate::stack::Place {
        world: crate::stack::world_file(&location, &home),
        location,
    };
    Ok((place, home, times))
}

/// Keep in state each `memo.first` value the apply read that state does
/// not keep yet (R-60), a secret one sealed with the stack's key.
fn keep_memos(
    st: &mut state::State,
    externs: &crate::externs::Externs,
    master: &crate::custody::Master,
) -> Result<()> {
    crate::memo::keep(st, externs.memos(), master, &crate::memo::now())
}

/// The grant a plan prints (R-166): each secret output no provider holds,
/// and the deployments it is sealed to, `output kubeconfig  sealed to
/// apps[env=lab]`; one whose master is not made yet is said so.
fn grants_text(unheld: &[String], readers: &[(String, Option<[u8; 32]>)]) -> String {
    if readers.is_empty() {
        return String::new();
    }
    let to: Vec<String> = readers
        .iter()
        .map(|(r, public)| match public {
            Some(_) => r.clone(),
            None => format!("{r} (no master yet: applied once, it is sealed to by the next apply)"),
        })
        .collect();
    let mut out = String::from("\n");
    for k in unheld {
        out.push_str(&format!("output {k}  sealed to {}\n", to.join(", ")));
    }
    out
}

/// What a secret output `path` of `deployment` sealed to `reader` is
/// bound to (`custody::seal_to`'s label).
fn sealed_label(deployment: &str, path: &str, reader: &str) -> String {
    format!("{deployment}#{path} to {reader}")
}

/// The deployments of the project that read `own`'s outputs (R-166): each
/// registered one whose program reads it (by name, or by one it computes,
/// which may be any), with the public key its master publishes, when it
/// has one. The grant is the reader's use: the producer's plan prints it.
fn readers_of(
    root: &Path,
    own: &str,
    s3: store::OpenS3,
) -> Result<Vec<(String, Option<[u8; 32]>)>> {
    let dir = root.parent().unwrap_or(Path::new("."));
    let Some(project) = crate::project::Project::find(dir, env!("CARGO_PKG_VERSION"))? else {
        return Ok(Vec::new());
    };
    let found = crate::project::discover(&project);
    let own_stack = own.split_once('[').map_or(own, |(s, _)| s);
    let mut loaded: std::collections::BTreeMap<PathBuf, Option<deployment::Loaded>> =
        Default::default();
    let mut out = Vec::new();
    for (name, entry) in crate::stack::registry(root)? {
        if name == own {
            continue;
        }
        let (stack, key) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
            Some((s, k)) => (s.to_string(), k.to_string()),
            None => (name.clone(), String::new()),
        };
        let [one] = found.named(&stack)[..] else {
            continue;
        };
        let l = loaded.entry(one.file.clone()).or_insert_with(|| {
            let t = deployment::Target {
                files: vec![one.file.clone()],
                input_files: Vec::new(),
                providers: Vec::new(),
            };
            deployment::load(
                &t,
                env!("CARGO_PKG_VERSION"),
                &|p: &Path| std::fs::read_to_string(p),
                &mut deployment::Notes::default(),
            )
            .ok()
        });
        let Some(l) = l else { continue };
        let given: Vec<Atom> = key
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| {
                crate::ast::atom(
                    "input",
                    vec![
                        crate::ast::str_term(k.trim()),
                        crate::ast::str_term(v.trim()),
                    ],
                    Default::default(),
                )
            })
            .collect();
        let Ok(instance) = crate::stack::instance(&l.cfg, &l.stack, &l.program, &given) else {
            continue;
        };
        let (names, any) = crate::stack::reads(&l.program, &l.deployed, &instance.key);
        if names.contains(own) || any.contains(own_stack) {
            let public = crate::custody::public_of(entry.state.open(s3)?.as_ref())?;
            out.push((name, public));
        }
    }
    Ok(out)
}

/// Open each secret output sealed to `deployment` among `read` with its
/// master (R-166): its value goes in the read (`stack::Read::opened`).
/// What was opened, by the deployment and output, and what no provider
/// holds and is not sealed to it (said: its producer's next apply seals
/// to it); a run without the master opens none and says so.
fn open_sealed(
    read: &mut [crate::stack::Read],
    deployment: &str,
    master: &crate::custody::Master,
) -> (Vec<String>, Vec<String>) {
    let mut opened = Vec::new();
    let mut unsealed = Vec::new();
    for r in read.iter_mut() {
        let Some(p) = &r.published else { continue };
        for (k, o) in &p.secret {
            let Some(sealed) = o.sealed.get(deployment) else {
                if o.held.is_none() && !o.digest.is_empty() {
                    eprintln!(
                        "{deployment}: {}.{k} is held by no provider and not sealed to it yet: \
                         apply {} again, which seals it to {deployment} (a reader from its \
                         first apply)",
                        r.name, r.name
                    );
                    unsealed.push(format!("{}.{k}", r.name));
                }
                continue;
            };
            // Its stand-in (R-164): a function of its keyed digest, which
            // stays while the value does; what a run without the master
            // reads in its place.
            let label = sealed_label(&p.deployment, k, deployment);
            let standin = format!(
                "sealed-{}",
                &crate::approval::sha256_hex(format!("{label}\0{}", o.digest).as_bytes())[..32]
            );
            let Some(key) = &master.key else {
                eprintln!(
                    "{deployment}: {}.{k} is sealed to it: opening it needs its master ({})",
                    r.name,
                    master.without.as_deref().unwrap_or("not held")
                );
                crate::secrets::standin::register(&standin, &label, &standin);
                r.opened.insert(k.clone(), Value::Str(standin));
                continue;
            };
            // Sealed to the current epoch's key, or (a producer not applied
            // since a cycle, R-165) an earlier one's.
            let earlier = master.earlier.iter().rev().filter_map(|e| e.key.as_ref());
            let plain = std::iter::once(key)
                .chain(earlier)
                .map(|k| crate::custody::open_sealed(k, &label, sealed))
                .find(Result::is_ok)
                .unwrap_or_else(|| crate::custody::open_sealed(key, &label, sealed));
            match plain.and_then(|b| Ok(serde_json::from_slice::<Value>(&b)?)) {
                Ok(v) => {
                    if let Value::Str(text) = &v {
                        crate::secrets::standin::register(text, &label, &standin);
                    }
                    r.opened.insert(k.clone(), v);
                    opened.push(format!("{}.{k}", r.name));
                }
                Err(e) => eprintln!("warning: {}.{k}: {e:#}", r.name),
            }
        }
    }
    (opened, unsealed)
}

/// Each change of `report` a run that does not hold the master plans,
/// marked (R-164): `secret changed, needs the key` when it would send a
/// stand-in, `secrets unchanged` when every secret leaf it derives was
/// proven unchanged (`.., a write-only one needs the key` when an update
/// would send one of those whole: the world does not answer it).
fn custody_marks(
    report: &mut report::Report,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    for d in report.definite.iter_mut() {
        let Some(a) = plan.actions.iter().find(|a| a.addr == d.addr) else {
            continue;
        };
        let need = backend.needs_master(a, None);
        let proven = backend.proven(&a.addr);
        d.custody = match (need.is_empty(), proven.is_empty()) {
            // An update sends a write-only secret whole, unchanged or not.
            (false, _) if need.iter().all(|p| proven.contains(p)) => {
                Some("secrets unchanged, a write-only one needs the key".into())
            }
            (false, _) => Some("secret changed, needs the key".into()),
            // Unchanged in the program; the world's own value of one it
            // answers is compared only by a run with the key.
            (true, false) if !backend.answered(&a.addr, &proven).is_empty() => {
                Some("secrets unchanged, drift unknown without the key".into())
            }
            (true, false) => Some("secrets unchanged".into()),
            (true, true) => None,
        };
    }
}

/// What a run without the master cannot see (After R-164): a secret leaf
/// it proved unchanged in the program whose value the world answers (a
/// Secret's `stringData` key, not a write-only one) may have been changed
/// in the world by someone else; only a run with the key compares it. Said
/// on stderr, each leaf, so the plan never reads as "no drift".
fn drift_unknown(
    deployment: &str,
    plan: &crate::provider::Plan,
    backend: &crate::plugin::Providers,
) {
    if !crate::secrets::standin::active() {
        return;
    }
    let leaves: Vec<String> = plan
        .actions
        .iter()
        .flat_map(|a| {
            backend
                .answered(&a.addr, &backend.proven(&a.addr))
                .into_iter()
                .map(|p| crate::report::attribute(&a.addr, &p))
        })
        .collect();
    if leaves.is_empty() {
        return;
    }
    eprintln!(
        "{deployment}: drift unknown without the key: {} the world holds {} compared with it only \
         by a run with the master: {}",
        match leaves.len() {
            1 => "1 secret".to_string(),
            n => format!("{n} secrets"),
        },
        if leaves.len() == 1 { "is" } else { "are" },
        leaves.join(", ")
    );
}

/// The actions of `plan` a run that does not hold the master cannot make
/// (R-164), each with the paths that need it (`Providers::needs_master`),
/// and what depends on one (no paths): what references it, and the
/// delete of what it references.
fn needing_master(
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
fn needing_text(
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

/// A destroy's objects no Delete can reach ([`Planned::unreachable`]),
/// as the plan says a change and its reason.
fn unreachable_text(unreachable: &[(ir::Address, String)]) -> String {
    if unreachable.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nunreachable  stay in state\n");
    for (a, why) in unreachable {
        out.push_str(&format!("  {}\n      {why}\n", report::address(a)));
    }
    out
}

/// What an approval is of: a plan file, an apply's plan, a destroy's.
#[derive(Clone, Copy, PartialEq)]
enum Asked {
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
fn approve_entry(
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

/// The bare diff's `needs approval:` section: each change and why.
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
    print!("{}", report::moved_text(moves));
}

/// `dform dev strata`: the partition graph's strata, or the negative cycle.
fn print_strata(
    files: &[PathBuf],
    program: &crate::ast::Program,
    schema: &schema::Schema,
    o: &report::table::Options,
) -> Result<()> {
    let graph = partition::build(program, schema)?;
    let verdict = partition::stratify(&graph);
    let name = files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    print!("{}", format_strata(&name, &graph, &verdict, o));
    if let partition::Verdict::Rejected {
        scc,
        negative_edges,
    } = &verdict
    {
        bail!("{}", partition::cycle_error(&graph, scc, negative_edges));
    }
    Ok(())
}

/// The stratified case as a result set (R-63), one row per node, by
/// stratum then node, under a line of counts; `dform dev strata` is pinned
/// as a golden snapshot. Rejected (negative cycle) keeps
/// `partition::report`'s own format.
fn format_strata(
    name: &str,
    g: &partition::Graph,
    v: &partition::Verdict,
    o: &report::table::Options,
) -> String {
    use report::table::{Cell, Table};
    match v {
        partition::Verdict::Stratified { strata } => {
            let max = strata.values().copied().max().unwrap_or(0);
            let mut out = format!(
                "== {name}: {} nodes, {} edges ({} negative), {} strata\n",
                g.nodes.len(),
                g.edges.len(),
                g.edges.iter().filter(|e| e.negative).count(),
                max + 1
            );
            let mut rows: Vec<(usize, String)> =
                strata.iter().map(|(n, s)| (*s, n.to_string())).collect();
            rows.sort();
            let mut t = Table::new(["stratum", "node"]);
            for (s, n) in rows {
                t.push(vec![Cell::text(s.to_string()), Cell::text(n)]);
            }
            out.push_str(&t.render(o));
            out
        }
        partition::Verdict::Rejected { .. } => partition::report(name, g, v),
    }
}

/// `dform dev effects`, a result set (R-63): what each scope (the stack,
/// each module instance, each pack in use) reads, writes and offers
/// (DESIGN.org R-11c), one row per effect.
fn print_effects(
    program: &crate::ast::Program,
    schema: &schema::Schema,
    json: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    let effects = crate::effects::compute(program, schema)?;
    let mut t = Table::new(["scope", "effect", "what"]);
    for (scope, e) in &effects {
        let mut row = |effect: &str, what: String| {
            t.push(vec![
                Cell::text(scope.clone()),
                Cell::text(effect),
                Cell::text(what),
            ])
        };
        e.reads.iter().for_each(|r| row("reads", r.to_string()));
        e.needs.iter().for_each(|n| row("needs", n.to_string()));
        e.writes.iter().for_each(|w| row("writes", w.to_string()));
        e.offers
            .iter()
            .for_each(|(k, ty)| row("offers", format!("{k}: {ty}")));
        e.uses.iter().for_each(|p| row("uses", p.to_string()));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&t.json())?);
    } else {
        print!("{}", t.render(o));
    }
    Ok(())
}

/// `query`'s output, a result set (R-63): one column per variable of the
/// goal, or per argument of a bare predicate; `yes` or `no` for a ground
/// goal. `--json` is the rows as an array of objects keyed by column.
fn print_query(
    pattern: &str,
    program: &crate::ast::Program,
    facts: &std::collections::BTreeSet<Atom>,
    redact: &query::Redactor,
    json: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    // A `let`'s, an input's or an output's cell by its path (R-176),
    // where no relation is so named (a `let k` is the relation `k` too).
    let parsed = match query::parse(pattern)? {
        query::Query::Pred(p) if !facts.iter().any(|a| a.pred == p) => {
            query::cell(pattern, facts).unwrap_or(query::Query::Pred(p))
        }
        q => q,
    };
    let tables: Vec<Table> = match parsed {
        query::Query::Pred(pred) => {
            // One table per arity a predicate is used at.
            let mut by: std::collections::BTreeMap<usize, Table> = Default::default();
            for a in facts.iter().filter(|a| a.pred == pred) {
                let t = by
                    .entry(a.args.len())
                    .or_insert_with(|| Table::new(query::columns(&pred, a.args.len(), program)));
                t.push(
                    a.args
                        .iter()
                        .map(|t| match t {
                            Term::Val(v) => Cell::value(v, redact),
                            t => Cell::text(partition::fmt_term(t)),
                        })
                        .collect(),
                );
            }
            by.into_values().collect()
        }
        query::Query::Body { body, vars } => {
            let table = query::table(&body, &vars, facts)?;
            if vars.is_empty() && !json {
                println!("{}", if table.rows.is_empty() { "no" } else { "yes" });
                return Ok(());
            }
            // One attribute's value (`T["A"].p`), in the formatter's
            // layout, as the plan and `why` print a value (R-124).
            if let (false, Some(_), [row]) =
                (json, query::address(pattern, false)?, table.rows.as_slice())
                && table.vars == ["value"]
            {
                let tree = crate::fmt::value::Tree::of(&row[0], &|v| {
                    let open = matches!(v, Value::Obj(_) | Value::List(_)) && !redact.is_secret(v);
                    (!open).then(|| redact.cell(v))
                });
                for line in crate::fmt::value::layout("", &tree, o.width.min(report::WIDTH)) {
                    println!("{line}");
                }
                return Ok(());
            }
            vec![table.result(redact)]
        }
    };
    if json {
        let rows: Vec<serde_json::Value> = tables
            .iter()
            .flat_map(|t| t.json().as_array().cloned().unwrap_or_default())
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if tables.is_empty() {
        println!("(0 rows)");
    }
    let shown: Vec<String> = tables.iter().map(|t| t.render(o)).collect();
    print!("{}", shown.join("\n"));
    Ok(())
}

/// The environment variables `env_var` reads, by label (`env.var/NAME`),
/// as a plan file records them: the label and the value's digest keyed
/// with the stack's plan key, as a secret input's. One not set now is
/// left out.
fn env_inputs(
    labels: impl Iterator<Item = String>,
    key: Option<&zset::file::Key>,
) -> Vec<serde_json::Value> {
    labels
        .filter_map(|label| {
            let name = label.strip_prefix("env.var/")?;
            let v = std::env::var(name).ok()?;
            Some(keyed(&label, key, v.as_bytes()))
        })
        .collect()
}

/// A secret as a plan file records it: its label, and its digest keyed
/// with the deployment's master; a run that does not hold it records the
/// label alone, never an unkeyed digest (R-164).
fn keyed(label: &str, key: Option<&zset::file::Key>, bytes: &[u8]) -> serde_json::Value {
    match key {
        Some(k) => serde_json::json!({ "sensitive": label, "digest": k.digest(bytes) }),
        None => serde_json::json!({ "sensitive": label }),
    }
}

/// The secret answers of dform's own externs (`ssh.read`) as a plan file
/// records them: each label and its value's digest with the plan key.
fn answer_inputs(
    externs: &crate::externs::Externs,
    key: Option<&zset::file::Key>,
) -> Vec<serde_json::Value> {
    externs
        .secret_answers()
        .into_iter()
        .map(|(label, v)| {
            let bytes = match &v {
                Value::Str(s) => s.clone().into_bytes(),
                v => serde_json::to_vec(v).unwrap_or_default(),
            };
            keyed(&label, key, &bytes)
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
    key: Option<&zset::file::Key>,
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
                    digest: match key {
                        Some(k) => k.digest(&read(f)?),
                        None => String::new(),
                    },
                })
            })
            .collect::<Result<_>>()?,
        set: cli
            .set
            .iter()
            .map(|kv| {
                Ok(match kv.split_once('=') {
                    Some((k, v)) if secret.contains(k) => {
                        let label = crate::value::null_label(crate::modules::INPUT, "", k);
                        let bytes = match v.strip_prefix('@') {
                            Some(f) => read(&PathBuf::from(f))?,
                            None => v.as_bytes().to_vec(),
                        };
                        keyed(&label, key, &bytes)
                    }
                    // `k=@FILE`: the file's digest, keyed, as an
                    // `--input-file`'s.
                    Some((_, v)) if v.starts_with('@') => {
                        let bytes = read(&PathBuf::from(&v[1..]))?;
                        match key {
                            Some(k) => serde_json::json!({ "set": kv, "digest": k.digest(&bytes) }),
                            None => serde_json::json!({ "set": kv }),
                        }
                    }
                    _ => serde_json::Value::String(kv.clone()),
                })
            })
            .collect::<Result<_>>()?,
        data: cli.data.clone(),
        providers: cli.providers.clone(),
        world: show(&cli.world),
        inventory: show(&cli.inventory),
        env: Vec::new(),
        answers: Vec::new(),
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
                file if file["set"].is_string() => cli
                    .set
                    .push(file["set"].as_str().unwrap_or_default().to_string()),
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

/// `dform fmt`: rewrite each file in its formatted form, or with `check`
/// list the files that are not and fail.
/// `dform doc [TARGET]`: the doc comments of the project's .df files, or
/// of the target's program (its file and every file it imports), as
/// Markdown (`syntax::doc::markdown`), then the standard library's
/// functions (`syntax::doc::std_markdown`).
fn doc(files: &[PathBuf]) -> Result<()> {
    let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let (title, files) = match (files, &project) {
        ([], Some(p)) => (p.manifest.project.name.clone(), crate::project::df_files(p)),
        ([], None) => return Err(crate::project::not_in_a_project(Path::new("."))),
        (fs, _) => (None, crate::loader::program_files(fs)?),
    };
    let title = title
        .or_else(|| {
            let f = files.first()?;
            Some(f.file_stem()?.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut trees = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        let name = f.strip_prefix(&cwd).unwrap_or(f).display().to_string();
        let parse = crate::syntax::parser::parse(&text);
        if !parse.errors.is_empty() {
            return Err(crate::parser::syntax_diagnostics(&name, &text, &parse).into());
        }
        trees.push((name, parse.syntax()));
    }
    print!(
        "{}{}",
        crate::syntax::doc::markdown(&title, &trees),
        crate::syntax::doc::std_markdown()
    );
    Ok(())
}

fn fmt_files(paths: &[PathBuf], check: bool) -> Result<()> {
    let mut unformatted = Vec::new();
    // Each project's typing (its providers' schemas, read offline), by root;
    // a file in no project has none.
    let mut typings: std::collections::BTreeMap<PathBuf, crate::fmt::Typing> =
        std::collections::BTreeMap::new();
    for p in paths {
        let src =
            std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("read {}: {e}", p.display()))?;
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
        let project =
            crate::project::Project::find(dir.unwrap_or(Path::new(".")), env!("CARGO_PKG_VERSION"))
                .ok()
                .flatten();
        let typing = project.map(|pr| {
            &*typings
                .entry(pr.root.clone())
                .or_insert_with(|| crate::fmt::Typing::of_project(&pr))
        });
        let out = crate::fmt::format_source_in(&p.display().to_string(), &src, typing)?;
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

/// `dform stack list`, a result set (R-63): one row per deployment with
/// state (a stack with none, one row saying so), its stack, file and
/// where its state is, its last apply and a pending saved plan.
fn stack_list(cli: &Cli) -> Result<()> {
    use report::table::{Cell, Table};
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
    let mut t = Table::new([
        "stack",
        "file",
        "deployment",
        "state",
        "applied",
        "by",
        "commit",
        "result",
        "pending",
    ]);
    for s in &d.stacks {
        let key = if s.keys.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.keys.join(", "))
        };
        let row = |deployment: &str, state: String, last: LastApply| {
            [
                format!("{}{key}", s.name),
                s.file.display().to_string(),
                deployment.to_string(),
                state,
                last.applied,
                last.by,
                last.commit,
                last.result,
                last.pending,
            ]
            .into_iter()
            .map(Cell::text)
            .collect::<Vec<_>>()
        };
        let failed = |e: &anyhow::Error| LastApply {
            result: format!("{e:#}"),
            ..Default::default()
        };
        // Where the stack's deployments are: its backend's, else the state
        // root's.
        let backend = stack_backend(s);
        let base = deployment::stack_location(&cli.root, &s.name, backend.as_ref());
        let shown = |l: &store::Location| match l {
            store::Location::S3(spec) => spec.to_string(),
            store::Location::Local(_) => String::new(),
        };
        let opener = open_s3(&cli.root, false);
        let keys = match base.open(&opener).and_then(|st| st.list("")) {
            Ok(k) => k,
            Err(e) => {
                t.push(row("", base.to_string(), failed(&e)));
                continue;
            }
        };
        let mut deployments: Vec<(String, store::Location)> = Vec::new();
        let keyed = backend.as_ref().and_then(crate::stack::keyed_parent);
        if s.keys.is_empty() {
            deployments.push((s.name.clone(), base.clone()));
        } else if let (Some(b), Some((parent, rest))) = (&backend, keyed) {
            // A backend that names the key (`local("state/app-{env}")`):
            // each place under its directory the template matches.
            let found = deployment::stack_location(&cli.root, &s.name, Some(&parent))
                .open(&opener)
                .and_then(|st| st.list(""))
                .unwrap_or_default();
            let mut segs: Vec<String> = found
                .iter()
                .filter_map(|k| crate::stack::template_key(&rest, &s.keys, k))
                .collect();
            segs.sort();
            segs.dedup();
            for seg in segs {
                deployments.push((
                    format!("{}[{seg}]", s.name),
                    deployment::deployment_location(&cli.root, &s.name, Some(b), Some(&seg)),
                ));
            }
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
                    any = true;
                    t.push(row(&name, location.to_string(), failed(&e)));
                    continue;
                }
            };
            let entries = crate::audit::Log::new(store.clone(), None)
                .entries()
                .unwrap_or_default();
            // A destroyed deployment is gone; its log stays (R-149).
            if (entries.is_empty() && store.get(store::STATE)?.is_none()) || destroyed(&entries) {
                continue;
            }
            any = true;
            let state = match registry.get(&name).and_then(|e| e.backend.clone()) {
                Some(b) => format!("handed over to {b}"),
                None => shown(&location),
            };
            t.push(row(&name, state, last_apply(&entries)));
        }
        if !any {
            let none = LastApply {
                result: "no deployment has state".into(),
                ..Default::default()
            };
            t.push(row("", shown(&base), none));
        }
    }
    print!("{}", t.without_empty_columns().render(&cli.table));
    Ok(())
}

/// The audit log `entries` end in a `destroy` that completed: no apply
/// started since (R-149).
fn destroyed(entries: &[serde_json::Value]) -> bool {
    entries
        .iter()
        .rev()
        .find_map(|e| match e["kind"].as_str() {
            Some("destroyed") => Some(true),
            Some("apply_start") => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// A deployment's last apply and pending saved plan, from its audit log.
#[derive(Debug, Default)]
struct LastApply {
    applied: String,
    by: String,
    commit: String,
    result: String,
    pending: String,
}

fn last_apply(entries: &[serde_json::Value]) -> LastApply {
    let field = |e: &serde_json::Value, k: &str| e[k].as_str().unwrap_or("").to_string();
    let start = entries.iter().rposition(|e| e["kind"] == "apply_start");
    let mut out = match start {
        None => LastApply {
            applied: "never".into(),
            ..Default::default()
        },
        Some(i) => {
            let e = &entries[i];
            let end = entries[i..]
                .iter()
                .find(|e| e["kind"] == "apply_end")
                .map(|e| field(e, "result"))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "running or interrupted".into());
            LastApply {
                applied: field(e, "time"),
                by: field(e, "who"),
                commit: e["commit"]
                    .as_str()
                    .map(|c| crate::report::short_id(c).to_string())
                    .unwrap_or_default(),
                result: end,
                pending: String::new(),
            }
        }
    };
    let plan = entries
        .iter()
        .rposition(|e| e["kind"] == "plan" && e["file"].is_string() && e["digest"].is_string());
    if let Some(p) = plan
        && start.is_none_or(|s| p > s)
    {
        out.pending = format!(
            "{} ({})",
            field(&entries[p], "file"),
            field(&entries[p], "digest")
        );
    }
    out
}

/// `dform state show [ADDR]`, a result set (R-63): the deployment's
/// objects, one row per address (with ADDR, that object's only), then its
/// outputs as a key/value table.
fn state_show(
    dep: &crate::store::Deployment,
    only: Option<&str>,
    from_log: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    let only = only.map(ir::parse_resource_address).transpose()?;
    let (deployment, at) = (dep.name(), dep.locate(crate::store::STATE));
    let (st, rebuilt) = match from_log {
        // The log alone (R-146): its `state` entries from the last whole
        // one, the checkpoint not read.
        true => match dep.state_from_log()? {
            Some((st, n)) => (st, Some(n)),
            None => bail!(
                "stack {deployment}: the audit log {} holds no whole state to rebuild from \
                 (it began before the state was logged, or there is none)",
                dep.locate(crate::store::AUDIT)
            ),
        },
        false => {
            if !dep.has_state()? {
                bail!("stack {deployment} has no state at {at}: it was never applied");
            }
            (dep.load_state()?, None)
        }
    };
    let mut objects = Table::new(["address", "provider", "remote"]);
    let mut push = |addr: String, deposed: bool, e: &state::StateEntry| {
        let addr = if deposed {
            format!("{addr} (deposed)")
        } else {
            addr
        };
        objects.push(vec![
            Cell::text(addr),
            Cell::text(e.provider.clone()),
            Cell::text(e.remote.clone()),
        ]);
    };
    if let Some(a) = &only {
        let key = state::key(a);
        let (live, deposed) = (st.resources.get(&key), st.deposed.get(&key));
        if live.is_none() && deposed.is_none() {
            bail!("stack {deployment} has no object at {a}");
        }
        live.into_iter()
            .for_each(|e| push(report::address(a), false, e));
        deposed
            .into_iter()
            .for_each(|e| push(report::address(a), true, e));
        print!("{}", objects.render(o));
        return Ok(());
    }
    // As the plan prints it (R-111); the stored form is the state file's.
    let addr = |k: &String| state::parse_key(k).map_or(k.clone(), |a| report::address(&a));
    st.resources
        .iter()
        .for_each(|(k, e)| push(addr(k), false, e));
    st.deposed.iter().for_each(|(k, e)| push(addr(k), true, e));
    match rebuilt {
        Some(n) => println!(
            "{deployment}: rebuilt from the log alone, {n} state entries: {}",
            dep.locate(crate::store::AUDIT)
        ),
        None => println!("{deployment}: {at}"),
    }
    print!("{}", objects.render(o));
    if !st.outputs.is_empty() || !st.secret_outputs.is_empty() {
        println!();
        print!("{}", output_table(&st).pairs(o));
    }
    match &st.in_flight {
        Some(f) if f.destroy => println!("a destroy was interrupted: the next destroy resumes it"),
        Some(_) => println!("an apply was interrupted: the next apply resumes it"),
        None => {}
    }
    Ok(())
}

/// A deployment's outputs as of its last apply, as a key/value table: a
/// secret as `secret`, its bytes held nowhere in state.
fn output_table(st: &state::State) -> report::table::Table {
    use report::table::{Cell, Table};
    let redact = query::Redactor::default();
    let mut t = Table::new(["output", "value"]);
    let mut rows: Vec<(&String, Cell)> = st
        .outputs
        .iter()
        .map(|(k, v)| (k, Cell::value(v, &redact)))
        .collect();
    rows.extend(st.secret_outputs.iter().map(|(k, o)| {
        let label = serde_json::json!({ "sensitive": ir::label(&o.label) });
        (k, Cell::secret(None, label))
    }));
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (k, c) in rows {
        t.push(vec![Cell::text(k.clone()), c]);
    }
    t
}

/// `dform output TARGET [NAME]`, a result set (R-63): the deployment's
/// outputs as of its last apply, what other stacks read. The scalars are
/// a key/value table and each relation (`output p`) its own table headed
/// by its name, its columns its `decl`'s. With NAME, that output's value
/// for the shell: a string's bytes as they are, another scalar in surface
/// spelling, a relation's rows tab-separated. `--json` is the same, as
/// JSON. A secret output prints as `secret`: state keeps no bytes of it.
fn print_output(
    dep: &crate::store::Deployment,
    program: &crate::ast::Program,
    name: Option<&str>,
    json: bool,
    o: &report::table::Options,
) -> Result<()> {
    use report::table::{Cell, Table};
    let deployment = dep.name();
    if !dep.has_state()? {
        bail!(
            "stack {deployment} has no state at {}: it was never applied",
            dep.locate(crate::store::STATE)
        );
    }
    let st = dep.load_state()?;
    let relations: BTreeSet<&str> = program
        .statements
        .iter()
        .filter_map(|s| match s {
            crate::ast::Stmt::Output(o) if o.relation.is_some() => Some(o.name.as_str()),
            _ => None,
        })
        .collect();
    let redact = query::Redactor::default();
    // A relation's rows as a table: one row per list of values.
    let rows = |k: &str, v: &Value| -> Table {
        let rows: Vec<&Vec<Value>> = match v {
            Value::List(xs) => xs
                .iter()
                .filter_map(|r| match r {
                    Value::List(r) => Some(r),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let arity = rows.first().map_or_else(
            || {
                program
                    .statements
                    .iter()
                    .find_map(|s| match s {
                        crate::ast::Stmt::Decl(d) if d.pred == k => Some(d.fields.len()),
                        _ => None,
                    })
                    .unwrap_or(0)
            },
            |r| r.len(),
        );
        let mut t = Table::new(query::columns(k, arity, program));
        for r in rows {
            t.push(r.iter().map(|v| Cell::value(v, &redact)).collect());
        }
        t
    };
    let Some(name) = name else {
        let mut scalars = output_table(&st);
        scalars.rows.retain(|r| !relations.contains(r[0].text_of()));
        let blocks: Vec<(String, Table)> = st
            .outputs
            .iter()
            .filter(|(k, _)| relations.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), rows(k, v)))
            .collect();
        if json {
            let mut doc = serde_json::Map::new();
            for r in &scalars.rows {
                doc.insert(r[0].text_of().to_string(), r[1].json_of().clone());
            }
            for (k, t) in &blocks {
                doc.insert(k.clone(), t.json());
            }
            println!("{}", serde_json::to_string_pretty(&doc)?);
            return Ok(());
        }
        if scalars.rows.is_empty() && blocks.is_empty() {
            println!("stack {deployment} has no outputs");
            return Ok(());
        }
        print!("{}", scalars.pairs(o));
        if !scalars.rows.is_empty() && !blocks.is_empty() {
            println!();
        }
        print!("{}", report::table::blocks(&blocks, o));
        return Ok(());
    };
    if st.secret_outputs.contains_key(name) {
        bail!(
            "output {name} of stack {deployment} is secret: its state keeps no bytes of it, \
             only its label and digest"
        );
    }
    let Some(v) = st.outputs.get(name) else {
        let mut names: Vec<&String> = st.outputs.keys().chain(st.secret_outputs.keys()).collect();
        names.sort();
        let names: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
        bail!(
            "stack {deployment} has no output {name} (its outputs: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        );
    };
    let bare = |v: &Value| match v {
        Value::Str(s) => s.clone(),
        v => redact.surface(v),
    };
    if relations.contains(name) {
        let t = rows(name, v);
        if json {
            println!("{}", serde_json::to_string_pretty(&t.json())?);
            return Ok(());
        }
        if let Value::List(xs) = v {
            for r in xs {
                let Value::List(r) = r else { continue };
                println!("{}", r.iter().map(bare).collect::<Vec<_>>().join("\t"));
            }
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&redact.json(v))?);
    } else if let Value::Str(s) = v {
        use std::io::Write;
        std::io::stdout().write_all(s.as_bytes())?;
    } else {
        println!("{}", bare(v));
    }
    Ok(())
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
    let (old, new) = (
        ir::parse_resource_address(from)?,
        ir::parse_resource_address(to)?,
    );
    let lock = dep.lock()?;
    let mut st = dep.load_state()?;
    if st.get(&old).is_none() {
        bail!("state mv: stack {deployment} has no object at {old}");
    }
    if st.get(&new).is_some() {
        bail!("state mv: stack {deployment} already has an object at {new}");
    }
    let (from, to) = (old.to_string(), new.to_string());
    st.apply_moves(&[(old, new)]);
    dep.save_state(&st)?;
    audit.append(
        "state_mv",
        serde_json::json!({ "from": from, "to": to, "who": crate::audit::who() }),
    )?;
    println!("moved {from} to {to} in stack {deployment}");
    lock.release()
}

/// Whether the experimental commands (R-41: `controller`, `stack
/// handover`) are listed in `--help` and completions: `DFORM_EXPERIMENTAL=1`.
/// They run either way, each run with [`EXPERIMENTAL`] on stderr.
fn experimental() -> bool {
    std::env::var_os("DFORM_EXPERIMENTAL").is_some_and(|v| v == "1")
}

/// The line an experimental command prints on every run.
const EXPERIMENTAL: &str = "warning: controller mode is experimental: its process model (where it \
                            runs, how it is supervised, what an operator sees) is not decided; \
                            see docs/experimental/controller.md";

/// The commands and subcommands only `DFORM_EXPERIMENTAL=1` lists.
const EXPERIMENTAL_COMMANDS: &[&str] = &["controller", "handover"];

/// The top-level commands, and each noun's subcommands.
const COMMANDS: &[&str] = &[
    "plan",
    "apply",
    "destroy",
    "why",
    "query",
    "diff",
    "test",
    "fmt",
    "doc",
    "log",
    "output",
    "stack",
    "state",
    "secrets",
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
        "state" => &["show", "forget-host", "mv"],
        "secrets" => &["list", "rotate", "cycle"],
        "provider" => &["check", "schema"],
        "controller" => &["run"],
        "log" => &["verify"],
        "completions" => &["zsh", "bash", "fish"],
        "dev" => &[
            "plan",
            "apply",
            "destroy",
            "why",
            "query",
            "diff",
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

/// `names` without the experimental ones, unless they are listed.
fn listed<'a>(names: &[&'a str]) -> Vec<&'a str> {
    names
        .iter()
        .copied()
        .filter(|n| experimental() || !EXPERIMENTAL_COMMANDS.contains(n))
        .collect()
}

/// `dform completions SHELL`: a script that completes commands, and asks
/// `dform __complete` for targets.
fn completion_script(shell: Shell) -> String {
    let commands = listed(COMMANDS).join(" ");
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
        [noun] if !subcommands(noun).is_empty() => listed(subcommands(noun))
            .iter()
            .map(|s| s.to_string())
            .collect(),
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
        if !i.key {
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
    match raw.strip_prefix('@') {
        Some(path) => Ok((k, set_file(k, Path::new(path))?)),
        None => Ok((k, deployment::value_of(raw))),
    }
}

/// `--set k=@FILE`: the value the document FILE holds, read as the input's
/// type later as any `--set` is: YAML, JSON or TOML by its extension, or
/// a `.df` file of the one fact `k(value)`.
fn set_file(k: &str, path: &Path) -> Result<Value> {
    let at = || format!("--set {k}=@{}", path.display());
    let ext = path.extension().and_then(|e| e.to_str());
    if !matches!(ext, Some("df" | "yaml" | "yml" | "json" | "toml")) {
        bail!("{}: a .yaml, .json, .toml or .df file", at());
    }
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", at()))?;
    match ext {
        Some("df") => {
            let program = crate::parser::parse_program(&text).with_context(at)?;
            match program.statements.as_slice() {
                [crate::ast::Stmt::Fact(a)] if a.pred == k => match a.args.as_slice() {
                    [t] => t.ground(),
                    _ => None,
                }
                .ok_or_else(|| anyhow::anyhow!("{}: the fact {k}(..) holds one value", at())),
                _ => bail!("{}: a .df file of one fact, `{k}(value)`", at()),
            }
        }
        Some(ext @ ("yaml" | "yml" | "json" | "toml")) => {
            let format = if ext == "yml" { "yaml" } else { ext };
            crate::tables::document(format, &text).with_context(at)
        }
        _ => bail!("{}: a .yaml, .json, .toml or .df file", at()),
    }
}

fn atom_kv(pred: &str, k: &str, v: Value) -> Atom {
    Atom {
        pred: pred.to_string(),
        args: vec![Term::Val(Value::Str(k.to_string())), Term::Val(v)],
        record: None,
        span: Default::default(),
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
