use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dform::ast::Atom;
use dform::ast::Term;
use dform::chaos::Chaos;
use dform::engine;
use dform::fakecloud::FakeCloud;
use dform::ir;
use dform::loader;
use dform::partition;
use dform::provider::{ActionKind, Provider, fmt_value};
use dform::schema;
use dform::state;
use dform::stuck;
use dform::value::Value;
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
    Plan,
    Apply {
        /// Inject a failure into the fake provider (repeatable):
        /// fail=T/N, timeout=T/N, read-lag=T/N:TICKS, mutate=T/N:PATH=JSON,
        /// latency=T/N:MS. Deterministic; nothing sleeps.
        #[arg(long = "chaos")]
        chaos: Vec<String>,
    },
    Query {
        pred: String,
    },
    Show {
        typ: String,
        name: String,
    },
    /// Print the stratification of the program (partition graph strata)
    Strata,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let files = default_files(&cli.files)?;
    let program = loader::load_program(&files)?;
    if let Cmd::Strata = cli.cmd {
        return print_strata(&files, &program);
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
        Cmd::Apply { chaos } => Chaos::parse(chaos)?,
        _ => Chaos::default(),
    };
    let backend =
        FakeCloud::with_paths(&paths.world, &paths.inventory, load_schema(&cli.providers)?)
            .with_chaos(chaos.clone());

    let mut st = state::State::load(&paths.state)?;
    backend.bootstrap_state(&mut st)?;

    let mut extra = build_extra_facts(&cli.set, &cli.data)?;
    extra.extend(backend.catalog()?);
    extra.extend(backend.discover()?);
    extra.extend(backend.world_facts(&st)?);
    let (res, violations) = engine::eval(&program, &extra)?;
    for w in &res.warnings {
        eprintln!("warning: {w}");
    }
    if !violations.is_empty() {
        eprintln!("constraint violations:");
        for v in violations {
            eprintln!("- {v}");
        }
        bail!("blocked by constraints");
    }

    let resources = ir::compile_resources(res.facts.iter().cloned(), backend.schema())?;
    let stack = state::stack_name(&files[0]);
    let adopts = ir::compile_adopts(res.facts.iter())?;

    match cli.cmd {
        Cmd::Eval => {
            println!("facts: {}", res.facts.len());
            println!("resources: {}", resources.len());
            for r in &resources {
                println!("- {}.{}", r.addr.typ, r.addr.name);
            }
        }
        Cmd::Query { pred } => {
            let mut count = 0usize;
            for a in &res.facts {
                if a.pred == pred {
                    count += 1;
                    println!("{:?}", a);
                }
            }
            println!("matches: {count}");
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
        Cmd::Plan => {
            let sections = plan_sections(&res, &resources, backend.schema());
            let plan = backend.plan(&resources, &adopts, &st)?;
            print_plan(&plan, cli.show_noop, &sections, &stack);
        }
        Cmd::Apply { .. } => {
            for addr in chaos.addresses() {
                if !resources.iter().any(|r| &r.addr == addr) && st.get(addr).is_none() {
                    bail!(
                        "--chaos: {}/{} is not a resource of this stack",
                        addr.typ,
                        addr.name
                    );
                }
            }
            let sections = plan_sections(&res, &resources, backend.schema());
            let mut plan = backend.plan(&resources, &adopts, &st)?;
            print_plan(&plan, cli.show_noop, &sections, &stack);
            // A pending deformation waits for a boundary: not this apply.
            plan.actions.retain(|a| {
                !sections
                    .pending
                    .contains_key(&(a.addr.typ.clone(), a.addr.name.clone()))
            });
            let changed = plan
                .actions
                .iter()
                .any(|a| !matches!(a.kind, ActionKind::Noop | ActionKind::Pending));
            // Every apply is one tick of the fake world, also when there is
            // nothing to do. State keeps every action that returned, also
            // when a later one fails.
            let res = backend.apply(&resources, &adopts, &mut st, &plan);
            st.save(&paths.state)?;
            for note in backend.take_notes() {
                println!("chaos: {note}");
            }
            res?;
            if changed {
                println!("apply: complete");
            } else {
                println!("apply: nothing to do");
            }
        }
    }

    Ok(())
}

/// `dform strata`: the partition graph's strata, or the negative cycle.
fn print_strata(files: &[PathBuf], program: &dform::ast::Program) -> Result<()> {
    let schema = dform::schema::fake();
    let graph = partition::build(program, &schema, &partition::Options::default())?;
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

/// What a deformation waits on: a boundary (the evaluator's sections), or
/// a comparison against an open null (the Z-set's pending update). `None`
/// when it is definite.
fn waits_on(a: &dform::provider::Action, sections: &stuck::Sections) -> Option<Vec<String>> {
    let key = (a.addr.typ.clone(), a.addr.name.clone());
    let on: Vec<String> = match (sections.pending.get(&key), &a.kind) {
        (Some(ns), _) => ns.iter().cloned().collect(),
        (None, ActionKind::Pending) => a.on.iter().cloned().collect(),
        (None, _) => return None,
    };
    Some(on)
}

fn print_plan(
    plan: &dform::provider::Plan,
    show_noop: bool,
    sections: &stuck::Sections,
    stack: &str,
) {
    let (held, plan_actions): (Vec<dform::provider::Action>, Vec<_>) = plan
        .actions
        .iter()
        .cloned()
        .partition(|a| waits_on(a, sections).is_some());
    let mut creates = 0usize;
    let mut adopts = 0usize;
    let mut updates = 0usize;
    let mut deletes = 0usize;
    let mut noops = 0usize;
    for a in &plan_actions {
        match a.kind {
            ActionKind::Create => creates += 1,
            ActionKind::Adopt => adopts += 1,
            ActionKind::Update | ActionKind::Drift => updates += 1,
            ActionKind::Delete => deletes += 1,
            ActionKind::Noop => noops += 1,
            ActionKind::Pending => {}
        }
    }

    let mut suffix = String::new();
    if adopts > 0 {
        suffix.push_str(&format!(", {adopts} to adopt"));
    }
    if show_noop {
        suffix.push_str(&format!(", {noops} no-op"));
    }
    if !held.is_empty() {
        suffix.push_str(&format!(", {} pending", held.len()));
    }
    println!("plan: {creates} to create, {updates} to update, {deletes} to delete{suffix}");
    print_actions(&plan_actions, show_noop);

    // Held for a boundary, grouped by what they wait on; their diffs are
    // shown now.
    let mut by_nulls: std::collections::BTreeMap<String, Vec<dform::provider::Action>> =
        Default::default();
    for a in held.iter().cloned() {
        let on = waits_on(&a, sections)
            .unwrap_or_default()
            .iter()
            .map(|n| format!("?{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        by_nulls.entry(on).or_default().push(a);
    }
    for (on, actions) in by_nulls {
        println!("pending on {on}:");
        print_actions(&actions, true);
    }
    if !sections.pending_groups.is_empty() {
        println!("pending groups:");
        for g in &sections.pending_groups {
            println!("? {g}");
        }
    }
    if !sections.undetermined.is_empty() {
        println!("undetermined:");
        for u in &sections.undetermined {
            println!("? {u}");
        }
    }
    // E §2.7: undeformed is the zero Z-set, nothing stuck, no cell stuck.
    let zero = plan_actions
        .iter()
        .all(|a| matches!(a.kind, ActionKind::Noop));
    if zero && held.is_empty() && sections.blocking.is_empty() && sections.undetermined.is_empty() {
        println!("stack {stack} is undeformed");
    }
}

fn print_actions(actions: &[dform::provider::Action], show_noop: bool) {
    for a in actions {
        if matches!(a.kind, ActionKind::Noop) && !show_noop {
            continue;
        }
        let prefix = match a.kind {
            ActionKind::Create => "+",
            ActionKind::Adopt => ">",
            ActionKind::Update | ActionKind::Drift | ActionKind::Pending => "~",
            ActionKind::Delete => "-",
            ActionKind::Noop => "=",
        };
        let note = match a.kind {
            ActionKind::Drift => {
                "  (drift: a fresh null where the world has a value; its identity is stale)"
            }
            _ => "",
        };
        println!("{prefix} {}.{}{note}", a.addr.typ, a.addr.name);

        if a.changes.is_empty() {
            continue;
        }

        // Keep plan output readable.
        let max = 40usize;
        for (i, ch) in a.changes.iter().enumerate() {
            if i == max {
                println!("  ... ({} more changes)", a.changes.len() - max);
                break;
            }
            let side = |v: Option<&serde_json::Value>| {
                if ch.sensitive && v.is_some() {
                    "(sensitive)".to_string()
                } else {
                    fmt_value(v)
                }
            };
            match a.kind {
                ActionKind::Create | ActionKind::Adopt => {
                    println!("  {} = {}", ch.path, side(ch.after.as_ref()));
                }
                ActionKind::Delete => {
                    println!("  {} was {}", ch.path, side(ch.before.as_ref()));
                }
                ActionKind::Update | ActionKind::Drift | ActionKind::Pending => {
                    println!(
                        "  {}: {} -> {}",
                        ch.path,
                        side(ch.before.as_ref()),
                        side(ch.after.as_ref())
                    );
                }
                ActionKind::Noop => {}
            }
        }
    }
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
