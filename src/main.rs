use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use dform::ast::Atom;
use dform::engine;
use dform::fakecloud::{ActionKind, FakeCloud};
use dform::ir;
use dform::loader;
use dform::ast::Term;
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
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Eval,
    Plan,
    Apply,
    Query {
        pred: String,
    },
    Show {
        typ: String,
        name: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let files = default_files(&cli.files)?;
    let program = loader::load_program(&files)?;

    let mut extra = build_extra_facts(&cli.set, &cli.data)?;
    // Inject discovery facts from fake backend (inventory).
    let backend = FakeCloud::new(PathBuf::from(".dform"));
    extra.extend(backend.discover()?);
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

    let resources = ir::compile_resources(res.facts.iter().cloned())?;
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
        Cmd::Plan => {
            let plan = backend.plan(&resources, &adopts)?;
            print_plan(&plan, cli.show_noop);
        }
        Cmd::Apply => {
            let plan = backend.plan(&resources, &adopts)?;
            print_plan(&plan, cli.show_noop);
            let changed = plan
                .actions
                .iter()
                .any(|a| !matches!(a.kind, ActionKind::Noop));
            if changed {
                backend.apply(&resources, &plan, &adopts)?;
                println!("apply: complete");
            } else {
                println!("apply: nothing to do");
            }
        }
    }

    Ok(())
}

fn print_plan(plan: &dform::fakecloud::Plan, show_noop: bool) {
    let mut creates = 0usize;
    let mut adopts = 0usize;
    let mut updates = 0usize;
    let mut deletes = 0usize;
    let mut noops = 0usize;
    for a in &plan.actions {
        match a.kind {
            ActionKind::Create => creates += 1,
            ActionKind::Adopt => adopts += 1,
            ActionKind::Update => updates += 1,
            ActionKind::Delete => deletes += 1,
            ActionKind::Noop => noops += 1,
        }
    }

    let mut suffix = String::new();
    if adopts > 0 {
        suffix.push_str(&format!(", {adopts} to adopt"));
    }
    if show_noop {
        suffix.push_str(&format!(", {noops} no-op"));
    }
    println!("plan: {creates} to create, {updates} to update, {deletes} to delete{suffix}");
    for a in &plan.actions {
        if matches!(a.kind, ActionKind::Noop) && !show_noop {
            continue;
        }
        let prefix = match a.kind {
            ActionKind::Create => "+",
            ActionKind::Adopt => ">",
            ActionKind::Update => "~",
            ActionKind::Delete => "-",
            ActionKind::Noop => "=",
        };
        println!("{prefix} {}.{}", a.addr.typ, a.addr.name);

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
            match a.kind {
                ActionKind::Create => {
                    println!("  {} = {}", ch.path, fmt_json_opt(ch.after.as_ref()));
                }
                ActionKind::Adopt => {
                    println!("  {} = {}", ch.path, fmt_json_opt(ch.after.as_ref()));
                }
                ActionKind::Delete => {
                    println!("  {} was {}", ch.path, fmt_json_opt(ch.before.as_ref()));
                }
                ActionKind::Update => {
                    println!(
                        "  {}: {} -> {}",
                        ch.path,
                        fmt_json_opt(ch.before.as_ref()),
                        fmt_json_opt(ch.after.as_ref())
                    );
                }
                ActionKind::Noop => {}
            }
        }
    }
}

fn fmt_json_opt(v: Option<&serde_json::Value>) -> String {
    match v {
        None => "<none>".to_string(),
        Some(serde_json::Value::String(s)) => format!("\"{}\"", s),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Null) => "null".to_string(),
        Some(other) => {
            // For non-leaf values (should be rare with our flattening).
            serde_json::to_string(other).unwrap_or_else(|_| "<unprintable>".to_string())
        }
    }
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
