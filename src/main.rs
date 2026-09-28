use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dform::ast::Atom;
use dform::ast::Term;
use dform::chaos::Chaos;
use dform::controller;
use dform::engine;
use dform::executor;
use dform::graph;
use dform::inputs;
use dform::ir;
use dform::loader;
use dform::partition;
use dform::plan_print::{self, waits_on};
use dform::plugin::{self, Providers};
use dform::provider::ActionKind;
use dform::query;
use dform::schema;
use dform::state;
use dform::stuck;
use dform::value::Value;
use dform::watch;
use dform::why;
use dform::zset;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug, Clone)]
#[command(name = "dform")]
#[command(about = "Facts + rules + constraints for infra", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// One or more .df files
    #[arg(long = "file", global = true)]
    files: Vec<PathBuf>,

    /// Provide input facts: --set env=prod
    #[arg(long = "set", global = true)]
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

    /// A provider: a schema the mock provider plays (a name,
    /// providers/NAME/schema.df else a built-in, or a path to a schema .df
    /// file), or a plugin executable (a path to one, or to a directory
    /// holding a `dform-provider*`). Repeatable; default `fake`.
    #[arg(long = "provider", global = true)]
    providers: Vec<String>,

    /// The fake provider's world file: what exists. Plan refreshes from it,
    /// apply writes it back. State sits beside it as <stem>.state.json.
    /// Default: .dform/<stack>/remote.json.
    #[arg(long = "world", global = true)]
    world: Option<PathBuf>,

    /// The directory whose `.dform/` holds the stacks' state and the stack
    /// registry (`.dform/stacks.json`). Default: the program file's
    /// directory, so stacks that read each other's outputs share it from
    /// any working directory.
    #[arg(long = "root", global = true)]
    root: Option<PathBuf>,

    /// Discovery inventory file (cloud_exists/cloud_attr/cloud_computed).
    /// Default: <world dir>/inventory.json if --world is given and that file
    /// exists, else .dform/inventory.json.
    #[arg(long = "inventory", global = true)]
    inventory: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
enum Cmd {
    Eval,
    Plan {
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
    /// Run every scenario against an empty mock world: each passes when
    /// nothing is denied. Fails if any scenario is denied.
    Test,
    Apply {
        /// A plan file from `plan --out`: refresh, re-evaluate, and refuse
        /// unless the delta is the file's. Its inputs are the defaults for
        /// --file, --set, --data, --provider, --world and --inventory.
        plan_file: Option<PathBuf>,
        /// Inject a failure into the fake provider (repeatable):
        /// fail=T/N, timeout=T/N, crash=T/N, read-lag=T/N:READS, mutate=T/N:PATH=JSON,
        /// latency=T/N:MS. Deterministic; nothing sleeps.
        #[arg(long = "chaos")]
        chaos: Vec<String>,
        /// Stop after this many ticks (phase boundaries) if the stack is
        /// still deformed.
        #[arg(long = "max-ticks", default_value_t = 8)]
        max_ticks: usize,
        /// At most this many provider Apply calls in flight: a tick's
        /// independent actions overlap.
        #[arg(long = "parallel", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        parallel: u64,
    },
    /// Query the final fact store: a predicate name (every fact of it) or
    /// body literals with variables, printed as a table with one column per
    /// variable: `dform query 'attr(net.vpc, n, .cidr, c)'`.
    Query {
        pattern: String,
        /// Print the answer as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Print how a fact was derived: rule, bindings, the facts it read,
    /// recursively. Variables are allowed; every match is printed.
    Why {
        pattern: String,
        /// Show every alternative derivation, not only the first.
        #[arg(long)]
        all: bool,
    },
    Show {
        typ: String,
        name: String,
    },
    /// Print the stratification of the program (partition graph strata)
    Strata,
    /// Format .df files in place: spacing, indentation and the commas a
    /// newline makes redundant (line breaks are kept). No PATH formats the
    /// --file files. `--check` changes nothing and fails if any file would.
    Fmt {
        paths: Vec<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// Graphviz DOT: the resource dependency DAG (no argument), the
    /// partition graph (`strata`), or a binary relation (`PRED` or `PRED/2`).
    Graph {
        what: Option<String>,
    },
    /// Controller mode: wait for an input relation's source or the world to
    /// change, then refresh, evaluate, plan, gate on policy and apply, one
    /// log line per event and per tick. Refuses a `role = bootstrap` stack.
    Controller {
        /// The stack to run: must be the program's own.
        #[arg(long = "stack")]
        stack: Option<String>,
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
    /// Forget one persisted extern answer (`extern ... persist`) of STACK:
    /// the answer for EXTERN with the input values ARGS, each written as
    /// `--set` takes a value. The next plan asks the provider again.
    Taint {
        stack: String,
        #[arg(value_name = "EXTERN")]
        pred: String,
        args: Vec<String>,
    },
    /// Stack operations.
    Stack {
        #[command(subcommand)]
        cmd: StackCmd,
    },
    /// Provider tools.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum StackCmd {
    /// Move a stack's state to another backend and record it in the
    /// registry: `local("DIR")`, or `k8s("ns/name")`, the in-cluster backend
    /// (for now a directory in the bootstrap stack's state). The controller
    /// runs the stack from there; a batch `apply` refuses it.
    Handover {
        stack: String,
        #[arg(long = "to")]
        to: String,
    },
    /// Move one deployment of a keyed stack to another key value: `rekey
    /// app env=staging env=stg` moves the state of `app[env=staging]` to
    /// `app[env=stg]`, and its registry entry. Nothing in the cloud
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
}

#[derive(Subcommand, Debug, Clone)]
enum ProviderCmd {
    /// The conformance suite: run every protocol method against the
    /// provider at PATH (an executable, a directory holding one, or a mock
    /// schema, which the mock provider plays) with a synthetic schema, and
    /// report what deviates. Fails if anything does.
    Check { path: String },
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse(), None) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            use std::io::IsTerminal;
            eprint!(
                "{}",
                dform::diag::report(&e, std::io::stderr().is_terminal())
            );
            std::process::ExitCode::FAILURE
        }
    }
}

/// One run of the command line. `hook`: controller mode's part of an apply
/// (`controller::Hook`).
fn run(mut cli: Cli, mut hook: Option<&mut controller::Hook>) -> Result<()> {
    if let Cmd::Controller { .. } = cli.cmd {
        return run_controller(cli);
    }
    if let Cmd::Stack {
        cmd: StackCmd::Handover { stack, to },
    } = &cli.cmd
    {
        let dir = dform::stack::handover(&state_root(&cli, &cli.files), stack, to)?;
        println!("stack {stack} handed over to {to}: {}", dir.display());
        return Ok(());
    }
    if let Cmd::Taint { stack, pred, args } = &cli.cmd {
        return taint(&cli, stack, pred, args);
    }
    let plan_file = match &cli.cmd {
        Cmd::Apply { plan_file, .. } => plan_file.clone(),
        _ => None,
    };
    let saved = match plan_file {
        Some(p) => Some((p.clone(), with_plan_inputs(&mut cli, &p)?)),
        None => None,
    };

    if let Cmd::Provider {
        cmd: ProviderCmd::Check { path },
    } = &cli.cmd
    {
        let (lines, failed) = plugin::check::run(path)?;
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
    if let Cmd::Fmt { paths, check } = &cli.cmd {
        let paths = if paths.is_empty() {
            default_files(&cli.files)?
        } else {
            paths.clone()
        };
        return fmt_files(&paths, *check);
    }
    let files = default_files(&cli.files)?;
    let mut program = loader::load_program(&files)?;
    // Input relations: declared, and stated as their sources hold them now.
    let relations = watch::take(&mut program)?;
    if let Some(h) = hook.as_deref_mut() {
        h.inputs(&relations);
    }
    program.statements.extend(watch::read(&relations)?);
    if let Cmd::Plan {
        scenario: Some(name),
        ..
    } = &cli.cmd
    {
        program = dform::scenario::select(&program, name)?;
    }
    // `stack` and `provider` statements; `--provider` overrides the latter.
    let stack_cfg = dform::stack::config(&program)?;
    // `stack rekey`: the run is of the old deployment (its state, its
    // world), the provenance of its names is listed, and its state moves.
    let rekey = match cli.cmd.clone() {
        Cmd::Stack {
            cmd: StackCmd::Rekey { stack, pairs },
        } => Some(rekey_args(&mut cli, &stack_cfg, &files, &stack, &pairs)?),
        _ => None,
    };
    let providers = if cli.providers.is_empty() {
        stack_cfg.providers.clone()
    } else {
        cli.providers.clone()
    };
    // The stack's and the instances' typed inputs, when the program lowers
    // (when it does not, evaluation reports why).
    let lowered = dform::transform::lower(&program).ok();
    let declared = lowered
        .as_ref()
        .map(|l| l.inputs.clone())
        .unwrap_or_default();
    // An input a fact of the program gives (a scenario's `with k = v`).
    let mut given: BTreeSet<String> = input_fact_keys(&program);
    for f in &cli.input_files {
        let src = std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("read --input-file {}: {e}", f.display()))?;
        let facts = dform::parser::parse_file(&f.display().to_string(), &src)?;
        let stmts = inputs::file_stmts(&facts, &declared)?;
        given.extend(facts.statements.iter().filter_map(|s| match s {
            dform::ast::Stmt::Fact(a) => Some(a.pred.clone()),
            _ => None,
        }));
        program.statements.extend(stmts);
    }
    if let Cmd::Test = cli.cmd {
        return run_tests(&program, &providers, &cli.set, &cli.data, &files);
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
    for w in dform::lint::lint(&program, &set_keys) {
        eprintln!("warning: {w}");
    }
    // Every evaluation carries the rules that derive the lifecycle denies
    // from the deformation the planner hands back (`zset::POLICY_RULES`).
    let program = zset::with_policy_rules(program)?;

    let root = state_root(&cli, &files);
    for note in state::migrate_unscoped(&root)? {
        eprintln!("note: {note}");
    }
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
        None => dform::stack::instance(&stack_cfg, &stack, &program, &set_facts)?,
    };
    let deployment = instance.name();
    inputs::check_required(&declared, &given)?;
    // The stack's directory, and the deployment's in it.
    let base = match &stack_cfg.backend {
        Some(dir) => state::local_dir(&root, dir),
        None => root.join(&stack),
    };
    let mut paths = match &cli.world {
        Some(w) => state::world_paths(&root, w),
        None => state::backend_paths(&root, &instance.dir(&base)),
    };
    // A keyed stack whose state is still the unkeyed one.
    if cli.world.is_none()
        && rekey.is_none()
        && instance.segment().is_some()
        && !paths.state.exists()
        && base.join("state.json").exists()
    {
        eprintln!(
            "note: stack {stack} is keyed now, and its state from before is not any \
             deployment's: {}; move it to this one with `dform stack rekey {stack} {}`",
            base.join("state.json").display(),
            instance
                .key
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    // A stack handed over to another backend lives there now.
    let handed = match &cli.world {
        None => dform::stack::handed_over(&root, &deployment)?,
        Some(_) => None,
    };
    if let Some((_, dir)) = &handed {
        paths = state::backend_paths(&root, dir);
    }
    paths.inventory = resolve_inventory(&cli.inventory, &cli.world, &paths.inventory);
    // The stack's plan-file key: a plan that writes a file, or an apply of
    // one, digests secrets with it.
    let key = match (&saved, &cli.cmd) {
        (Some(_), _) | (None, Cmd::Plan { out: Some(_), .. }) => {
            Some(zset::file::Key::load_or_create(&paths.state)?)
        }
        _ => None,
    };
    let secret_inputs: BTreeSet<String> = declared
        .iter()
        .filter(|d| d.scope.is_empty())
        .filter(|d| matches!(&d.decl.ty, dform::ast::TypeExpr::Apply(n, _) if n == "secret"))
        .map(|d| d.decl.name.clone())
        .collect();
    // What the plan file records, when one is written or read.
    let inputs = match &key {
        Some(k) => Some(plan_inputs(&cli, &files, &secret_inputs, k)?),
        None => None,
    };
    if let (Some((path, saved)), Some(inputs)) = (&saved, &inputs) {
        let diff = saved.input_differences(inputs);
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
        h.open(
            &paths.state,
            &paths.world,
            root.parent().unwrap_or(Path::new("")),
        )?;
    } else if let (Cmd::Apply { .. }, Some((to, _))) = (&cli.cmd, &handed) {
        bail!(
            "stack {deployment} was handed over to {to}: the controller runs it \
             (`dform controller --stack {deployment}`), not a batch apply"
        );
    }
    let chaos = match &cli.cmd {
        Cmd::Apply { chaos, .. } => Chaos::parse(chaos)?,
        _ => Chaos::default(),
    };
    let chaos_specs = match &cli.cmd {
        Cmd::Apply { chaos, .. } => chaos.clone(),
        _ => Vec::new(),
    };
    // The schema is asked for once the run knows the types it names.
    let backend = Providers::start_deferred(
        &providers,
        &plugin::Config {
            world: paths.world.clone(),
            inventory: paths.inventory.clone(),
            chaos: chaos_specs,
        },
    )?;
    // Externs are asked on demand: of the file provider, else of the mock.
    let (no_program, no_fns) = (dform::ast::Program { statements: vec![] }, vec![]);
    let program_dir = files[0].parent().unwrap_or(Path::new("")).to_path_buf();
    let externs = dform::externs::Externs::new(
        lowered.as_ref().map_or(&no_program, |l| &l.program),
        lowered.as_ref().map_or(&no_fns, |l| &l.extern_fns),
        |f, inputs| {
            if let Some(r) = dform::externs::file(f, inputs, &program_dir) {
                return r;
            }
            let plus: Vec<bool> = f.args.iter().map(|b| b.input).collect();
            backend.query(&f.name, &plus, inputs)
        },
    );

    let mut st = state::State::load(&paths.state)?;
    backend.bootstrap_state(&mut st)?;
    // What the plan file read, then what state persisted, before asking.
    if let Some((_, saved)) = &saved {
        externs.preload(saved.externs.clone());
    }
    externs.preload(st.externs.clone());

    let mut base_extra = set_facts;
    base_extra.extend(build_extra_facts(&cli.data)?);
    base_extra.extend(dform::stack::stack_outputs(&root, &deployment)?);
    let discovered = backend.discover()?;
    let scope = catalog_scope(&cli.cmd, &program, &base_extra, &discovered, &st);
    backend.load_schema(scope.as_ref())?;
    // The static secret pass and the refinement checks (a literal that
    // violates one, E0306), against the provider's schema.
    if let Some(l) = &lowered {
        dform::secrets::check(l, backend.schema())?;
        dform::refine::check(&l.program, backend.schema())?;
    }
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
            let (res, violations, resumable) =
                externs.eval_resumable(&program, &extra, zset::POLICY_INPUTS)?;
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
    // Policy messages quote values and rule text: printed redacted.
    // The collision lint of a keyed stack: a name every deployment writes
    // the same. A deny under strict mode.
    if !stack_cfg.keys.is_empty()
        && !stack_cfg.isolated
        && matches!(cli.cmd, Cmd::Plan { .. } | Cmd::Apply { .. })
    {
        let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
        for c in dform::lint::key_collisions(
            &res.facts,
            &res.circuit,
            backend.schema(),
            &keys,
            &deployment,
        ) {
            match stack_cfg.unknowns {
                dform::stack::Unknowns::Strict => violations.push(c),
                dform::stack::Unknowns::Permissive => eprintln!("warning: {c}"),
            }
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
        Cmd::Plan { .. } | Cmd::Query { .. } | Cmd::Why { .. } | Cmd::Stack { .. }
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
    let report_of = |plan: &dform::provider::Plan,
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
    let show = |plan: &dform::provider::Plan,
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
    let check_saved = |plan: &dform::provider::Plan,
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

    match cli.cmd {
        Cmd::Eval => {
            println!("facts: {}", res.facts.len());
            println!("resources: {}", resources.len());
            for r in &resources {
                println!("- {}.{}", r.addr.typ, r.addr.name);
            }
        }
        Cmd::Query { pattern, json } => {
            let res = explained(res);
            let redact = query::Redactor::new(&res.facts, backend.schema());
            print_query(&pattern, &res.facts, &redact, json)?;
        }
        Cmd::Why { pattern, all } => {
            let res = explained(res);
            let redact = query::Redactor::new(&res.facts, backend.schema());
            let query::Query::Body { body, .. } = query::parse(&pattern)? else {
                bail!("why: expected a fact pattern such as 'want(net.vpc, N)', got '{pattern}'");
            };
            let [dform::ast::Lit::Pos(pat)] = body.as_slice() else {
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
        Cmd::Stack {
            cmd: StackCmd::Rekey { .. },
        } => {
            let Some(r) = rekey else {
                unreachable!("rekey_args ran for rekey");
            };
            let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
            let named = dform::lint::key_named(&res.facts, &res.circuit, backend.schema(), &keys);
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
            let dir = dform::stack::rekey(&root, &base, &r.from, &r.to)?;
            println!(
                "stack {} rekeyed to {}: {}",
                r.from.name(),
                r.to.name(),
                dir.display()
            );
        }
        Cmd::Fmt { .. }
        | Cmd::Controller { .. }
        | Cmd::Stack { .. }
        | Cmd::Provider { .. }
        | Cmd::Taint { .. } => {
            unreachable!("handled before loading")
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
            if json {
                println!("{}", serde_json::to_string_pretty(&report.json())?);
            } else {
                print!("{}", report.text());
            }
            blocked(&[violations, denies].concat())?;
            if let Some(out) = out {
                let redact = query::Redactor::new(&res.facts, schema);
                let key = key
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("internal: no plan key"))?;
                let mut deformations =
                    zset::file::delta(&plan, &sections, &report, schema, &redact, key);
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
                        if let Some((dform::provider::NULL_KEY, l)) =
                            c.after.as_ref().and_then(dform::provider::marker)
                        {
                            unresolved.insert(l.to_string());
                        }
                    }
                }
                let file = zset::file::PlanFile {
                    version: zset::file::VERSION,
                    stack: stack.clone(),
                    inputs: inputs.ok_or_else(|| anyhow::anyhow!("internal: no plan key"))?,
                    world_digest: zset::file::world_digest(&backend.world_facts(&st)?),
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
                };
                file.save(&out)?;
                eprintln!("plan file: {}", out.display());
            }
        }
        Cmd::Apply {
            max_ticks,
            parallel,
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
            // One apply at a time per deployment.
            let _lock = dform::stack::Lock::acquire(&paths.state, &deployment)?;
            persist_externs(&mut st, &externs);
            let persist = |st: &state::State| st.save(&paths.state);
            if !moves.is_empty() {
                print_moves(&moves);
                persist(&st)?;
            }
            if let Some(f) = st.in_flight.take() {
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
                let remaining = executor::remaining(&f);
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
                // Controller mode: the report is one log line, and the
                // policy pass gates what this tick may apply.
                let mut undeformed = false;
                if let Some(h) = hook.as_deref_mut() {
                    let text = report_of(&plan, &res, &sections, tick, &[], &denies).text();
                    undeformed = text
                        .lines()
                        .next()
                        .is_some_and(|l| l.ends_with(" is undeformed"));
                    h.gate(tick, &mut plan, &res.facts, &text);
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
                let observed = backend.observe(&st)?;
                executor::begin(&mut st, tick, &plan, &observed);
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
                    let opts = executor::Options {
                        parallel: parallel as usize,
                        persist: &persist,
                    };
                    let applied = executor::run_tick(
                        &backend, &resources, &adopts, &lifecycle, &mut st, &plan, &opts,
                    );
                    for note in backend.take_notes() {
                        println!("chaos: {note}");
                    }
                    seen.extend(applied?);
                }
                if !boundary {
                    st.in_flight = None;
                    // The stack's outputs, as the world now is, for other
                    // stacks to read (evaluated again only when it has any:
                    // the evaluation refreshes). A --world fixture is not
                    // registered: everything stays beside the world file.
                    persist_externs(&mut st, &externs);
                    st.outputs = if dform::stack::has_outputs(&res.facts) {
                        dform::stack::outputs(&evaluate(&st)?.0.facts)
                    } else {
                        Default::default()
                    };
                    persist(&st)?;
                    // Every deployment of a keyed stack is registered.
                    let keyed = instance.segment().is_some();
                    if (!st.outputs.is_empty() || stack_cfg.bootstrap || keyed)
                        && cli.world.is_none()
                    {
                        dform::stack::register(
                            &root,
                            &deployment,
                            &paths.state,
                            stack_cfg.bootstrap,
                        )?;
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

/// `dform controller`: a run per event (`controller::Hook`), until
/// `--once` or `--max-events` says stop. A run that fails is logged and the
/// controller goes on watching; the first one failing ends it.
fn run_controller(cli: Cli) -> Result<()> {
    let Cmd::Controller {
        stack,
        poll,
        once,
        max_events,
        max_ticks,
    } = cli.cmd.clone()
    else {
        unreachable!("run_controller is for `controller`");
    };
    let files = default_files(&cli.files)?;
    let program = loader::load_program(&files)?;
    let cfg = dform::stack::config(&program)?;
    let name = cfg
        .name
        .clone()
        .unwrap_or_else(|| state::stack_name(&files[0]));
    // One controller per deployment: of a keyed stack, the one `--set`
    // selects.
    let set = cli
        .set
        .iter()
        .map(|kv| split_kv(kv).map(|(k, v)| (k.to_string(), v)))
        .collect::<Result<Vec<_>>>()?;
    let own = dform::stack::instance(&cfg, &name, &program, &inputs::set_facts(&[], &set)?)?.name();
    if let Some(s) = &stack
        && *s != own
    {
        bail!(
            "controller --stack {s}: the program ({}) owns stack {own}",
            files[0].display()
        );
    }
    let registered = dform::stack::registry(&state_root(&cli, &files))?
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
        },
        ..cli
    };
    let mut hook = controller::Hook::default();
    let mut events = 0;
    loop {
        // What the run registers for diagnostics is dropped with it; the
        // program's files stay parsed (`loader`) until they change.
        let sources = dform::diag::Scope::new();
        if let Err(e) = run(apply.clone(), Some(&mut hook)) {
            let text = dform::diag::report(&e, false);
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
            writeln!(f, "{}", dform::diag::source_count())?;
        }
        if once || max_events.is_some_and(|n| events >= n) {
            return Ok(());
        }
        while !hook.changed() {
            std::thread::sleep(std::time::Duration::from_millis(poll));
        }
    }
}

/// `stack rekey`: the deployment whose state moves, and where to.
struct Rekey {
    from: dform::stack::Instance,
    to: dform::stack::Instance,
}

/// `stack rekey STACK K=V...`: STACK is the program's stack, and keyed; the
/// pairs are the old key, then the new one, each naming every key input
/// once (the old one left out: the state from before the stack was keyed).
/// The run is set to the old values (the new ones for the unkeyed state),
/// so it evaluates the deployment whose state moves.
fn rekey_args(
    cli: &mut Cli,
    cfg: &dform::stack::Stack,
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
    let instance = |key| dform::stack::Instance {
        stack: own.clone(),
        key,
    };
    Ok(Rekey {
        from: instance(from),
        to: instance(to),
    })
}

/// The keys of the program's own `input("k", v)` facts.
fn input_fact_keys(program: &dform::ast::Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            dform::ast::Stmt::Fact(a) if a.pred == "input" => match a.args.first() {
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
    program: &dform::ast::Program,
    providers: &[String],
    set: &[String],
    data: &[String],
    files: &[PathBuf],
) -> Result<()> {
    use std::io::IsTerminal;
    let names = dform::scenario::names(program)?;
    if names.is_empty() {
        bail!("no scenarios: write `scenario NAME {{ with k = v; deny rules }}`");
    }
    let backend = Providers::start(providers, &plugin::Config::default())?;
    let program_dir = files[0].parent().unwrap_or(Path::new("")).to_path_buf();
    let mut failed = 0;
    for name in &names {
        let run = || -> Result<Vec<String>> {
            let p = dform::scenario::select(program, name)?;
            let lowered = dform::transform::lower(&p)?;
            dform::secrets::check(&lowered, backend.schema())?;
            dform::refine::check(&lowered.program, backend.schema())?;
            let mut given = input_fact_keys(&p);
            let mut pairs = Vec::new();
            for kv in set {
                let (k, v) = split_kv(kv)?;
                given.insert(k.to_string());
                pairs.push((k.to_string(), v));
            }
            inputs::check_required(&lowered.inputs, &given)?;
            let mut extra = inputs::set_facts(&lowered.inputs, &pairs)?;
            extra.extend(build_extra_facts(data)?);
            extra.extend(backend.catalog(schema::named_types(&lowered.program, &extra).as_ref())?);
            let externs =
                dform::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
                    if let Some(r) = dform::externs::file(f, ins, &program_dir) {
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
                let text = dform::diag::report(&e, std::io::stdout().is_terminal());
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
/// `dform taint STACK EXTERN ARGS...`: remove the answer from the stack's
/// state (beside `--world`, else where the registry has it, else under the
/// state root), under the stack's lock.
fn taint(cli: &Cli, stack: &str, pred: &str, args: &[String]) -> Result<()> {
    let root = state_root(cli, &cli.files);
    let path = match &cli.world {
        Some(w) => state::world_paths(&root, w).state,
        None => match dform::stack::registry(&root)?.remove(stack) {
            Some(e) => e.state,
            None => state::state_path(dform::stack::instance_dir(&root, stack)),
        },
    };
    let call = format!("{pred}({})", args.join(", "));
    if !path.exists() {
        bail!(
            "taint {call}: stack {stack} has no state at {}",
            path.display()
        );
    }
    let _lock = dform::stack::Lock::acquire(&path, stack)?;
    let mut st = state::State::load(&path)?;
    if st.taint(pred, args).is_none() {
        bail!("taint {call}: stack {stack} has no persisted answer for it");
    }
    st.save(&path)?;
    println!("tainted {call} of stack {stack}: the next plan asks again");
    Ok(())
}

fn persist_externs(st: &mut state::State, externs: &dform::externs::Externs) {
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
    plan: dform::provider::Plan,
    sections: stuck::Sections,
    /// Denies over the plan: what the policy pass derives beyond the plan's
    /// own evaluation (`lifecycle prevent_destroy`, a policy on
    /// `deformation/4`).
    denies: Vec<String>,
}

/// `moved/3` rewrites applied to state before the plan.
fn print_moves(moves: &[(ir::Address, ir::Address)]) {
    print!("{}", plan_print::moved_text(moves));
}

/// `dform strata`: the partition graph's strata, or the negative cycle.
fn print_strata(
    files: &[PathBuf],
    program: &dform::ast::Program,
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
/// line, sorted) so `dform strata` can be pinned as a golden snapshot.
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
    program: &dform::ast::Program,
    given: &[Atom],
    discovered: &[Atom],
    st: &state::State,
) -> Option<BTreeSet<String>> {
    if let Cmd::Query { pattern, .. } | Cmd::Why { pattern, .. } = cmd {
        let reads_schema = match query::parse(pattern).ok()? {
            query::Query::Pred(p) => schema::is_schema_pred(&p),
            query::Query::Body { body, .. } => body.iter().any(|l| {
                matches!(l, dform::ast::Lit::Pos(a) | dform::ast::Lit::Not(a)
                    if schema::is_schema_pred(&a.pred))
            }),
        };
        if reads_schema {
            return None;
        }
    }
    let lowered = dform::transform::lower(program).ok()?;
    let facts: Vec<Atom> = given.iter().chain(discovered).cloned().collect();
    let mut named = schema::named_types(&lowered.program, &facts)?;
    named.extend(
        st.resources
            .keys()
            .chain(st.deposed.keys())
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
                    let label = dform::value::null_label(dform::modules::INPUT, "", k);
                    serde_json::json!({ "sensitive": label, "digest": key.digest(v.as_bytes()) })
                }
                _ => serde_json::Value::String(kv.clone()),
            })
            .collect(),
        data: cli.data.clone(),
        providers: cli.providers.clone(),
        world: show(&cli.world),
        inventory: show(&cli.inventory),
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
    Ok(Providers::start(providers, &plugin::Config::default())?
        .schema()
        .clone())
}

/// `--inventory PATH`, else `<world dir>/inventory.json` when `--world` is
/// given and that file exists, else the stack's default (`.dform/inventory.json`).
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
        let out = dform::fmt::format_source(&p.display().to_string(), &src)?;
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

/// The stacks' state root, `.dform/`: in `--root`, else in the directory of
/// the first program file (the working directory when there is none).
fn state_root(cli: &Cli, files: &[PathBuf]) -> PathBuf {
    let base = match (&cli.root, files.first()) {
        (Some(root), _) => root.clone(),
        (None, Some(f)) => f.parent().map(Path::to_path_buf).unwrap_or_default(),
        (None, None) => PathBuf::new(),
    };
    base.join(".dform")
}

fn default_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    if Path::new("dform.df").exists() {
        return Ok(vec![PathBuf::from("dform.df")]);
    }
    bail!("no input files: pass --file <path.df> (or create ./dform.df)")
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
