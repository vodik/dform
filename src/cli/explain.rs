//! The commands that explain a deployment's evaluation: `query`, `why`,
//! `diff` and its helper `__explain`; and `secrets`, which reads the same
//! evaluation.

use super::evaluated::Evaluated;
use super::{Cli, Cmd, Outcome};
use crate::ast::{Atom, Term};
use crate::spell;
use crate::value::Value;
use crate::{deployment, ir, query, report, schema};
use anyhow::Result;
use std::path::Path;

/// `dform query PATTERN`.
#[derive(Debug, Clone)]
pub(super) struct Query {
    pub(super) pattern: String,
    pub(super) json: bool,
}

/// `dform why PATTERN`.
#[derive(Debug, Clone)]
pub(super) struct Why {
    pub(super) pattern: String,
    pub(super) tree: bool,
    pub(super) all: bool,
    pub(super) core: bool,
    /// `-vv`: a long value whole (R-176).
    pub(super) whole: bool,
    pub(super) json: bool,
}

/// `dform diff --since REF`.
#[derive(Debug, Clone)]
pub(super) struct Diff {
    pub(super) since: String,
    pub(super) json: bool,
    /// How much each change says of why it was planned.
    pub(super) why: report::Why,
}

/// `dform __explain --address ADDR..`, `diff`'s helper.
#[derive(Debug, Clone)]
pub(super) struct Explain {
    pub(super) addresses: Vec<String>,
}

/// Run a command that explains the evaluation: it reads the policy pass,
/// so a deny over the plan can be asked for and explained.
pub(super) fn run(run: &mut Evaluated) -> Result<Outcome> {
    let schedule = match &run.cx.cli.cmd {
        Cmd::Why(_) => Why::schedule(&run.ev),
        _ => None,
    };
    let x = run.ev.explained();
    let run = &*run;
    match &run.cx.cli.cmd {
        Cmd::Query(q) => q.run(run, &x),
        Cmd::Why(w) => w.run(run, &x, schedule.as_ref()),
        Cmd::Explain(e) => e.run(&x),
        Cmd::Diff(d) => d.run(run, &x),
        Cmd::Secrets(s) => s.run(run, &x),
        _ => unreachable!("explains is query, why, diff, __explain or secrets"),
    }
}

impl Query {
    fn run(&self, run: &Evaluated, x: &deployment::Explained) -> Result<Outcome> {
        self.print(
            &run.ev.located.loaded.program,
            &x.res.facts,
            &x.redact,
            &run.cx.cli.table,
        )?;
        Ok(Outcome::Done)
    }
    /// `query`'s output, a result set (R-63): one column per variable of the
    /// goal, or per argument of a bare predicate; `yes` or `no` for a ground
    /// goal. `--json` is the rows as an array of objects keyed by column.
    fn print(
        &self,
        program: &crate::ast::Program,
        facts: &std::collections::BTreeSet<Atom>,
        redact: &query::Redactor,
        o: &report::table::Options,
    ) -> Result<()> {
        let (pattern, json) = (self.pattern.as_str(), self.json);
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
                    let t = by.entry(a.args.len()).or_insert_with(|| {
                        Table::new(query::columns(&pred, a.args.len(), program))
                    });
                    t.push(
                        a.args
                            .iter()
                            .map(|t| match t {
                                Term::Val(v) => Cell::value(v, redact),
                                t => Cell::text(spell::term(t)),
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
                        let open =
                            matches!(v, Value::Obj(_) | Value::List(_)) && !redact.is_secret(v);
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
}

impl Cli {
    /// How `diff` evaluates the program at an earlier commit: this executable,
    /// on the same program file (relative to the project root) and key
    /// values `key`, with this run's mock flags.
    pub(super) fn rerun(
        &self,
        key: &[(String, String)],
        keys: Vec<String>,
    ) -> Result<crate::diff::Rerun> {
        let cli = self;
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

impl Why {
    /// When each change the plan holds runs (After R-156): `why` says its
    /// tick, `later` only for what no tick of the plan makes. A group by
    /// the address its statement names, as the plan prints it.
    fn schedule(ev: &deployment::Evaluation) -> Option<report::Report> {
        let Some(Ok(p)) = &ev.policy else {
            return None;
        };
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
        r.explain(
            report::Why::Line,
            &p.res,
            &query::Redactor::new(&p.res.facts, ev.schema()),
        );
        Some(r)
    }

    fn run(
        &self,
        run: &Evaluated,
        x: &deployment::Explained,
        schedule: Option<&report::Report>,
    ) -> Result<Outcome> {
        let ev = &run.ev;
        let waits = |t: &str| ev.evaluator.provider_wait(t);
        let when = |at: &str| schedule.and_then(|r| r.when(at));
        let keys = ev
            .located
            .instance
            .key
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        let top = super::planning::site_root(&run.cx.cli.files);
        // What an object was made with, where the plan keeps a value given
        // at its creation (R-198): the object's value, and the apply that
        // made it.
        let applies = crate::diff::applies(&run.cx.audit.entries().unwrap_or_default());
        let kept = |a: &ir::Address, path: &str| {
            let value = schedule?.kept_value(a, path)?;
            let made = match crate::diff::made_by(&applies, &a.to_string()) {
                Some(m) => format!("made by apply {} at {} by {}", m.seq, m.time, m.who),
                None => "as it was made".to_string(),
            };
            Some(format!("kept (bootstrap): the object's = {value}  {made}"))
        };
        let cx = crate::why::Context {
            res: &x.res,
            schema: Some(ev.schema()),
            redact: &x.redact,
            signatures: ev.located.loaded.lowered.as_ref().map(|l| &l.signatures),
            stack_keys: &keys,
            top: top.as_deref(),
            waits: &waits,
            when: schedule
                .is_some()
                .then_some(&when as &dyn Fn(&str) -> Option<String>),
            kept: Some(&kept),
        };
        let how = crate::why::As {
            tree: self.tree || self.all,
            all: self.all,
            core: self.core,
            whole: self.whole,
        };
        if self.json {
            let j = crate::why::why_json(&self.pattern, how, &cx)?;
            println!("{}", serde_json::to_string_pretty(&j)?);
        } else {
            print!("{}", crate::why::why(&self.pattern, how, &cx)?);
        }
        Ok(Outcome::Done)
    }
}

impl Explain {
    fn run(&self, x: &deployment::Explained) -> Result<Outcome> {
        let addresses = self
            .addresses
            .iter()
            .map(|a| ir::parse_resource_address(a))
            .collect::<Result<Vec<_>>>()?;
        let s = crate::diff::snapshot(&x.res, &x.redact, &addresses);
        println!("{}", serde_json::to_string(&s)?);
        Ok(Outcome::Done)
    }
}

impl Diff {
    fn run(&self, run: &Evaluated, x: &deployment::Explained) -> Result<Outcome> {
        let located = &run.ev.located;
        let keys: Vec<String> = located
            .loaded
            .cfg
            .keys
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        let cli = &run.cx.cli;
        let rerun = cli.rerun(&located.instance.key, keys)?;
        let d = crate::diff::diff(
            &run.cx.audit.entries()?,
            &self.since,
            &cli.files,
            &rerun,
            &|a| crate::diff::snapshot(&x.res, &x.redact, a),
        )?;
        if self.json {
            let mut j = d.json();
            j["deployment"] = serde_json::json!(run.cx.deployment);
            println!("{}", serde_json::to_string_pretty(&j)?);
        } else {
            print!("{}", d.text(self.why));
        }
        Ok(Outcome::Done)
    }
}
