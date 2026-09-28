use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dform::ast::Atom;
use dform::ast::Term;
use dform::chaos::Chaos;
use dform::engine;
use dform::executor;
use dform::fakecloud::FakeCloud;
use dform::graph;
use dform::ir;
use dform::loader;
use dform::partition;
use dform::plan_print::{self, waits_on};
use dform::provider::{ActionKind, Provider};
use dform::query;
use dform::schema;
use dform::state;
use dform::stuck;
use dform::value::Value;
use dform::why;
use dform::zset;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
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

    /// Provide data facts: --data zone=us-test-1a
    #[arg(long = "data", global = true)]
    data: Vec<String>,

    /// Show no-op actions in plan
    #[arg(long, global = true)]
    show_noop: bool,

    /// Provider schema to mock: a name (providers/NAME/schema.df, else a
    /// built-in) or a path to a schema .df file. Repeatable; default `fake`.
    #[arg(long = "provider", global = true)]
    providers: Vec<String>,

    /// The fake provider's world file: what exists. Plan refreshes from it,
    /// apply writes it back. State sits beside it as <stem>.state.json.
    /// Default: .dform/<stack>/remote.json.
    #[arg(long = "world", global = true)]
    world: Option<PathBuf>,

    /// Discovery inventory file (cloud_exists/cloud_attr/cloud_computed).
    /// Default: <world dir>/inventory.json if --world is given and that file
    /// exists, else .dform/inventory.json.
    #[arg(long = "inventory", global = true)]
    inventory: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Eval,
    Plan {
        /// Write the plan file: inputs, a digest of the world, and the
        /// deformation delta with its nulls and tick schedule.
        /// `apply PLAN.json` applies exactly this delta or refuses.
        #[arg(long = "out")]
        out: Option<PathBuf>,
    },
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
    /// variable: `dform query 'attr(net.vpc, N, cidr, C)'`.
    Query {
        pattern: String,
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
    /// Graphviz DOT: the resource dependency DAG (no argument), the
    /// partition graph (`strata`), or a binary relation (`PRED` or `PRED/2`).
    Graph {
        what: Option<String>,
    },
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let plan_file = match &cli.cmd {
        Cmd::Apply { plan_file, .. } => plan_file.clone(),
        _ => None,
    };
    let saved = match plan_file {
        Some(p) => Some((p.clone(), with_plan_inputs(&mut cli, &p)?)),
        None => None,
    };

    let files = default_files(&cli.files)?;
    let program = loader::load_program(&files)?;
    if let Cmd::Strata = cli.cmd {
        return print_strata(&files, &program, &load_schema(&cli.providers)?);
    }
    if let Cmd::Graph { what: Some(w) } = &cli.cmd
        && w == "strata"
    {
        let graph = partition::build(
            &program,
            &load_schema(&cli.providers)?,
            &partition::Options::default(),
        )?;
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
    let inputs = plan_inputs(&cli, &files)?;
    if let Some((path, saved)) = &saved {
        let diff = saved.input_differences(&inputs);
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

    let set_keys: Vec<String> = cli
        .set
        .iter()
        .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_string()))
        .collect();
    for w in dform::lint::lint(&program, &set_keys) {
        eprintln!("warning: {w}");
    }

    let root = PathBuf::from(".dform");
    for note in state::migrate_unscoped(&root)? {
        eprintln!("note: {note}");
    }
    let mut paths = match &cli.world {
        Some(w) => state::world_paths(&root, w),
        None => state::stack_paths(&root, &state::stack_name(&files[0])),
    };
    paths.inventory = resolve_inventory(&cli.inventory, &cli.world, &paths.inventory);
    let chaos = match &cli.cmd {
        Cmd::Apply { chaos, .. } => Chaos::parse(chaos)?,
        _ => Chaos::default(),
    };
    let backend =
        FakeCloud::with_paths(&paths.world, &paths.inventory, load_schema(&cli.providers)?)
            .with_chaos(chaos.clone());

    let mut st = state::State::load(&paths.state)?;
    backend.bootstrap_state(&mut st)?;

    let mut base_extra = build_extra_facts(&cli.set, &cli.data)?;
    base_extra.extend(backend.catalog()?);
    base_extra.extend(backend.discover()?);
    // Refresh as facts: round 0 resolves every null the world can answer.
    let evaluate = |st: &state::State| -> Result<(engine::EvalResult, Vec<String>)> {
        let mut extra = base_extra.clone();
        extra.extend(backend.world_facts(st)?);
        engine::eval(&program, &extra)
    };
    let (mut res, mut violations) = evaluate(&st)?;
    // moved/3 rewrites state's identity before the diff (E §3.4); round 0
    // must see the new addresses, so the program is evaluated again.
    let moves = st.apply_moves(&zset::Lifecycle::from_facts(&res.facts)?.moved);
    if !moves.is_empty() {
        (res, violations) = evaluate(&st)?;
    }
    for w in &res.warnings {
        eprintln!("warning: {w}");
    }
    let blocked = |violations: &[String]| -> Result<()> {
        if violations.is_empty() {
            return Ok(());
        }
        eprintln!("constraint violations:");
        for v in violations {
            eprintln!("- {v}");
        }
        bail!("blocked by constraints");
    };
    // A plan prints what it would do, conflicts included (E §2.8: a
    // conflict is a fact, not an abort), and then refuses.
    if !matches!(cli.cmd, Cmd::Plan { .. }) {
        blocked(&violations)?;
    }

    let resources = ir::compile_resources(res.facts.iter().cloned(), backend.schema())?;
    let stack = state::stack_name(&files[0]);
    let adopts = ir::compile_adopts(res.facts.iter())?;
    let lifecycle = zset::Lifecycle::from_facts(&res.facts)?;
    let schema = backend.schema();
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
        let now = zset::file::delta(plan, sections, &report, schema);
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
        Cmd::Query { pattern } => {
            let redact = query::Redactor::new(&res.facts, backend.schema());
            match query::parse(&pattern)? {
                query::Query::Pred(pred) => {
                    let mut count = 0usize;
                    for a in res.facts.iter().filter(|a| a.pred == pred) {
                        count += 1;
                        println!("{}", redact.fmt_atom(a));
                    }
                    println!("matches: {count}");
                }
                query::Query::Body { body, vars } => {
                    print!(
                        "{}",
                        query::table(&body, &vars, &res.facts)?.render(&redact)
                    );
                }
            }
        }
        Cmd::Why { pattern, all } => {
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
            let json = serde_json::to_string_pretty(&r.attrs)?;
            println!("{}", json);
        }
        Cmd::Strata => unreachable!("handled before evaluation"),
        Cmd::Graph { what: None } => print!("{}", graph::resources(&resources)),
        Cmd::Graph { what: Some(spec) } => {
            let redact = query::Redactor::new(&res.facts, backend.schema());
            print!("{}", graph::relation(&spec, &res.facts, &redact)?);
        }
        Cmd::Plan { out } => {
            let sections = plan_sections(&res, &resources, backend.schema());
            let plan = backend.plan(&resources, &adopts, &lifecycle, &st)?;
            let denies = lifecycle.denies(&plan.actions);
            let report = report_of(&plan, &res, &sections, 1, &moves, &denies);
            print!("{}", report.text());
            blocked(&[violations, denies].concat())?;
            if let Some(out) = out {
                let mut deformations = zset::file::delta(&plan, &sections, &report, schema);
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
                    inputs,
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
                        resolved: zset::file::resolved(&res.facts, schema),
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
                };
                file.save(&out)?;
                println!("plan file: {}", out.display());
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
                let changed = executor::changed_under(
                    &backend,
                    &executor::remaining(&f),
                    &backend.observe(&st)?,
                );
                if !changed.is_empty() {
                    eprint!(
                        "the world changed under a remaining action:\n{}",
                        executor::format_changes(&changed)
                    );
                    persist(&st)?;
                    bail!(
                        "apply stopped: the world changed under {} remaining actions of the \
                         interrupted apply; review `dform plan`, then apply again",
                        changed.len()
                    );
                }
            }
            // Ticks (E §2.7): each applies every definite deformation in
            // dependency order; what waits on a null is held. At the
            // boundary the results come back as world facts, round 0
            // resolves them, everything is re-derived and policy is checked
            // again before the next tick.
            let (mut res, mut resources, mut adopts, mut lifecycle) =
                (res, resources, adopts, lifecycle);
            let mut tick = 1;
            loop {
                let sections = plan_sections(&res, &resources, backend.schema());
                let mut plan = backend.plan(&resources, &adopts, &lifecycle, &st)?;
                check_saved(&plan, &res, &sections, tick)?;
                let held: Vec<String> = plan
                    .actions
                    .iter()
                    .filter_map(|a| waits_on(a, &sections))
                    .flatten()
                    .collect();
                // A create_before_destroy replacement deposes an object that
                // is deleted at the next tick, once what depends on it has
                // moved to the replacement.
                let boundary = !held.is_empty()
                    || !sections.pending_groups.is_empty()
                    || !sections.undetermined.is_empty()
                    || plan
                        .actions
                        .iter()
                        .any(|a| matches!(a.kind, ActionKind::Replace { create_first: true }));
                if tick > 1 || boundary {
                    println!("tick {tick}:");
                }
                let denies = lifecycle.denies(&plan.actions);
                show(&plan, &res, &sections, tick, &[], &denies);
                if !denies.is_empty() {
                    eprintln!("constraint violations:");
                    for d in denies {
                        eprintln!("- {d}");
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
                    persist(&st)?;
                    if changed {
                        println!("apply: complete");
                    } else {
                        println!("apply: nothing to do");
                    }
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
                // The boundary.
                executor::check_boundary(&backend, &seen, &pending, &st, tick)?;
                let (next, violations) = evaluate(&st)?;
                for w in &next.warnings {
                    eprintln!("warning: {w}");
                }
                if !violations.is_empty() {
                    eprintln!("constraint violations after tick {tick}:");
                    for v in &violations {
                        eprintln!("- {v}");
                    }
                    bail!("apply stopped after tick {tick}: blocked by constraints");
                }
                resources = ir::compile_resources(next.facts.iter().cloned(), backend.schema())?;
                adopts = ir::compile_adopts(next.facts.iter())?;
                lifecycle = zset::Lifecycle::from_facts(&next.facts)?;
                res = next;
                tick += 1;
            }
        }
    }

    Ok(())
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
    let graph = partition::build(program, schema, &partition::Options::default())?;
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
    stuck::sections(&res.stuck, &res.facts, &docs, schema)
}

/// This run's inputs as a plan file records them.
fn plan_inputs(cli: &Cli, files: &[PathBuf]) -> Result<zset::file::Inputs> {
    let digests = files
        .iter()
        .map(|f| {
            let bytes =
                std::fs::read(f).map_err(|e| anyhow::anyhow!("read {}: {e}", f.display()))?;
            Ok(zset::file::FileDigest {
                path: f.display().to_string(),
                fnv64: zset::file::fnv64(&bytes),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let show = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
    Ok(zset::file::Inputs {
        files: digests,
        set: cli.set.clone(),
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
        cli.set = i.set.clone();
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

fn load_schema(providers: &[String]) -> Result<schema::Schema> {
    let names: Vec<&str> = if providers.is_empty() {
        vec!["fake"]
    } else {
        providers.iter().map(String::as_str).collect()
    };
    let mut out = schema::Schema::default();
    for n in names {
        out = out.merge(schema::load_provider(n)?)?;
    }
    Ok(out)
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

fn default_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    if Path::new("dform.df").exists() {
        return Ok(vec![PathBuf::from("dform.df")]);
    }
    bail!("no input files: pass --file <path.df> (or create ./dform.df)")
}

fn build_extra_facts(set: &[String], data: &[String]) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    for kv in set {
        let (k, v) = split_kv(kv)?;
        out.push(atom_kv("input", k, v));
    }
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
    }
}
