//! The evaluator's own views (`dform dev ..`): the evaluation's size, a
//! resource's document, the strata, the effects, a graph.

use super::evaluated::Evaluated;
use super::{Cli, Outcome, launch};
use crate::plugin::{self, Providers};
use crate::{address, deployment, graph, partition, query, report, schema};
use anyhow::{Result, bail};

/// `dform dev eval`: the evaluation's size and resources.
#[derive(Debug, Clone)]
pub(super) struct Eval;

impl Eval {
    pub(super) fn run(&self, run: &Evaluated, compiled: &deployment::Compiled) -> Result<Outcome> {
        println!("facts: {}", run.ev.res.facts.len());
        println!("resources: {}", compiled.resources.len());
        for r in &compiled.resources {
            println!("- {}", r.addr);
        }
        Ok(Outcome::Done)
    }
}

/// `dform dev show ADDR`: a resource's desired document.
#[derive(Debug, Clone)]
pub(super) struct Show {
    pub(super) addr: String,
}

impl Show {
    pub(super) fn run(&self, run: &Evaluated, compiled: &deployment::Compiled) -> Result<Outcome> {
        let addr = address::parse_resource(&self.addr)?;
        let Some(r) = compiled.resources.iter().find(|r| r.addr == addr) else {
            bail!("no resource {} in this deployment", report::address(&addr));
        };
        let json = serde_json::to_string_pretty(&run.ev.redact.json(&r.attrs))?;
        println!("{}", json);
        Ok(Outcome::Done)
    }
}

/// `dform dev strata`.
#[derive(Debug, Clone)]
pub(super) struct Strata;

impl Strata {
    /// `dform dev strata`: the partition graph's strata, or the negative cycle.
    pub(super) fn run(&self, cli: &Cli, loaded: &deployment::Loaded) -> Result<Outcome> {
        let (files, program, o) = (&cli.files, &loaded.program, &cli.table);
        let schema = &schema_of(loaded)?;
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
        Ok(Outcome::Done)
    }
}

/// `dform dev effects`.
#[derive(Debug, Clone)]
pub(super) struct Effects {
    pub(super) json: bool,
}

impl Effects {
    /// `dform dev effects`, a result set (R-63): what each scope (the stack,
    /// each module instance, each pack in use) reads, writes and offers
    /// (DESIGN.org R-11c), one row per effect.
    pub(super) fn run(&self, cli: &Cli, loaded: &deployment::Loaded) -> Result<Outcome> {
        let (program, json, o) = (&loaded.program, self.json, &cli.table);
        let schema = &schema_of(loaded)?;
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
        Ok(Outcome::Done)
    }
}

/// `dform dev graph`: the resource dependency DAG, the partition graph
/// (`what` is `strata`), or a binary relation.
#[derive(Debug, Clone)]
pub(super) struct Graph {
    pub(super) what: Option<String>,
}

impl Graph {
    /// The partition graph: drawn from the program alone.
    pub(super) fn strata(&self) -> bool {
        self.what.as_deref() == Some("strata")
    }

    /// `--strata`: the partition graph's strata, or the negative cycle.
    pub(super) fn run_strata(&self, loaded: &deployment::Loaded) -> Result<Outcome> {
        let graph = partition::build(&loaded.program, &schema_of(loaded)?)?;
        match partition::stratify(&graph) {
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
        }
    }

    pub(super) fn run(&self, run: &Evaluated, compiled: &deployment::Compiled) -> Result<Outcome> {
        match &self.what {
            None => print!("{}", graph::resources(&compiled.resources)),
            Some(spec) => {
                let res = &run.ev.res;
                let redact = query::Redactor::new(&res.facts, run.ev.schema());
                print!("{}", graph::relation(spec, &res.facts, &redact)?);
            }
        }
        Ok(Outcome::Done)
    }
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

/// The providers' schema: what `strata`, `effects` and `graph strata` read,
/// with no world: none when the program names only built-in ones.
fn schema_of(loaded: &deployment::Loaded) -> Result<schema::Schema> {
    if loaded.starts_none() {
        return Ok(schema::Schema::default());
    }
    let providers = &loaded.providers;
    Ok(
        Providers::start(launch(), providers, &plugin::Config::default())?
            .schema()
            .clone(),
    )
}
