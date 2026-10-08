use super::*;

/// `query`'s output, a result set (R-63): one column per variable of the
/// goal, or per argument of a bare predicate; `yes` or `no` for a ground
/// goal. `--json` is the rows as an array of objects keyed by column.
pub(super) fn print_query(
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

/// How `diff` evaluates the program at an earlier commit: this executable,
/// on the same program file (relative to the project root) and key
/// values `key`, with this run's mock flags.
pub(super) fn rerun_of(
    cli: &Cli,
    key: &[(String, String)],
    keys: Vec<String>,
) -> Result<crate::diff::Rerun> {
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
pub(super) fn reads_schema(pattern: &str) -> bool {
    match query::parse(pattern) {
        Ok(query::Query::Pred(p)) => schema::is_schema_pred(&p),
        Ok(query::Query::Body { body, .. }) => body.iter().any(|l| {
            matches!(l, crate::ast::Lit::Pos(a) | crate::ast::Lit::Not(a)
                if schema::is_schema_pred(&a.pred))
        }),
        Err(_) => false,
    }
}
