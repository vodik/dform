use super::*;

/// `dform dev strata`: the partition graph's strata, or the negative cycle.
pub(super) fn print_strata(
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
pub(super) fn format_strata(
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
pub(super) fn print_effects(
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

/// The providers' schema: what `strata` and `graph strata` read, with no
/// world.
pub(super) fn load_schema(providers: &[String]) -> Result<schema::Schema> {
    Ok(
        Providers::start(launch(), providers, &plugin::Config::default())?
            .schema()
            .clone(),
    )
}
