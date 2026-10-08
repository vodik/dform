//! `dform test` (R-32): the program's denies over its input space.

use super::run_inputs::{build_extra_facts, split_kv};
use super::{Cli, Outcome, launch};
use crate::ast::Term;
use crate::plugin::{self, Providers};
use crate::value::Value;
use crate::{deployment, engine, inputs, ir, partition, query, report, schema, state, zset};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// `dform test [TARGET]`.
#[derive(Debug, Clone)]
pub(super) struct Test;

impl Test {
    /// The program's denies over its input space (`testing::space`), each
    /// combination evaluated against an empty mock world (the provider's
    /// schema, no world, no state). A result set (R-63): a row per
    /// combination, its inputs and its result. A combination fails when
    /// anything is denied or it does not compile, printed after the
    /// matrix as the command that plans it.
    pub(super) fn run(&self, cli: &Cli, loaded: &deployment::Loaded) -> Result<Outcome> {
        let space = Space::new(cli, loaded)?;
        // What the target and `--set` pin, as given.
        let mut pinned: Vec<(String, Value)> = Vec::new();
        for kv in &cli.set {
            let (k, v) = split_kv(kv)?;
            pinned.push((k.to_string(), v));
        }
        let names: Vec<String> = pinned.iter().map(|(k, _)| k.clone()).collect();
        let axes = crate::testing::space(&space.lowered.inputs, &names, &|key: &str| {
            space.applied(key)
        })?;
        let combinations = crate::testing::combinations(&axes);
        let stack = &loaded.stack;
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
        // A column per input; a dependent input not declared in a
        // combination (R-104) is `-` there.
        let mut columns: Vec<String> = pinned.iter().map(|(k, _)| k.clone()).collect();
        for a in &axes {
            if !columns.contains(&a.input) {
                columns.push(a.input.clone());
            }
        }
        let mut matrix = Matrix::new(columns);
        for combination in &combinations {
            let mut pairs = pinned.clone();
            pairs.extend(combination.iter().cloned());
            let (result, lines) = space.result(&pairs);
            matrix.push(&pairs, result, space.reproduce(&pairs), lines);
        }
        space.notes();
        let failed = matrix.print(&cli.table);
        println!("test {stack}: {n} combination{s}, {failed} failed");
        if failed > 0 {
            bail!("{failed} of {n} combination{s} failed");
        }
        Ok(Outcome::Done)
    }
}

/// What every combination of a test is evaluated with: the program
/// lowered, its providers' schemas (none when it names only built-in
/// ones), a reader of locations that reads no provider's, and what the
/// test stood in for the world.
struct Space<'a> {
    cli: &'a Cli,
    program: &'a crate::ast::Program,
    stack: &'a str,
    lowered: crate::transform::Lowered,
    backend: Providers,
    reader: std::sync::Arc<crate::files::Files>,
    /// Whether an image's digest was stood in (no registry is asked).
    images: std::cell::Cell<bool>,
    /// The key inputs, given on the target rather than by `--set`.
    keys: BTreeSet<String>,
    /// Each deny's doc comment, by its message.
    docs: BTreeMap<String, String>,
}

impl<'a> Space<'a> {
    fn new(cli: &'a Cli, loaded: &'a deployment::Loaded) -> Result<Space<'a>> {
        let program = &loaded.program;
        // The run's reader of locations, as plan's (R-153), except that a
        // test reads nothing through a provider: what reads a provider's
        // location is undetermined.
        let reader = std::sync::Arc::new(
            crate::files::Files::new(
                crate::files::Settings::of(cli.manifest.as_ref()),
                Default::default(),
            )
            .standing_in(),
        );
        let backend = match loaded.starts_none() {
            true => {
                let p = Providers::none();
                p.load_schema(None)?;
                p
            }
            false => {
                let grants = cli.manifest.iter().flat_map(|m| m.grants());
                let config = plugin::Config {
                    // What the program configures is left unconfigured: a
                    // test configures no provider, so what it serves waits.
                    configured: deployment::provider_configs(program),
                    grants: grants
                        .map(|(k, mut g)| {
                            g.files = crate::files::Shared(Some(reader.clone()));
                            (k, g)
                        })
                        .collect(),
                    files: reader.clone(),
                    ..Default::default()
                };
                Providers::start(launch(), &loaded.providers, &config)?
            }
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
        let keys = lowered
            .inputs
            .iter()
            .filter(|d| d.scope.is_empty() && d.decl.key)
            .map(|d| d.decl.name.clone())
            .collect();
        Ok(Space {
            cli,
            program,
            stack: &loaded.stack,
            lowered,
            backend,
            reader,
            images: std::cell::Cell::new(false),
            keys,
            docs: deny_docs(program),
        })
    }

    /// The values of the key input `key` the stack's applied deployments
    /// have.
    fn applied(&self, key: &str) -> Vec<String> {
        let mut out: Vec<String> = crate::stack::registry(&self.cli.root)
            .unwrap_or_default()
            .into_keys()
            .filter_map(|n| {
                let (s, seg) = n.strip_suffix(']')?.split_once('[')?;
                (s == self.stack).then_some(())?;
                seg.split(',')
                    .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key))
                    .map(|(_, v)| v.to_string())
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The command that plans the combination `pairs`.
    fn reproduce(&self, pairs: &[(String, Value)]) -> String {
        let text = |(k, v): &(String, Value)| (k.clone(), partition::fmt_bare(v));
        let (on_target, set): (Vec<_>, Vec<_>) = pairs
            .iter()
            .map(text)
            .partition(|(k, _)| self.keys.contains(k.as_str()));
        let target = if self.cli.in_project {
            self.stack.to_string()
        } else {
            self.cli.files[0].display().to_string()
        };
        crate::testing::reproduce(&target, &on_target, &set)
    }

    /// The combination `pairs`' result, `ok`, `denied` or `error`, and
    /// what it printed: its denies, each with its doc, or its error.
    fn result(&self, pairs: &[(String, Value)]) -> (&'static str, Vec<String>) {
        use std::io::IsTerminal;
        match self.evaluate(pairs) {
            Ok(denied) if denied.is_empty() => ("ok", Vec::new()),
            Ok(denied) => (
                "denied",
                denied.iter().map(|d| self.documented(d)).collect(),
            ),
            Err(e) => {
                let text = crate::diag::report(&e, std::io::stdout().is_terminal());
                ("error", text.lines().map(String::from).collect())
            }
        }
    }

    /// The program evaluated with the inputs `pairs`: what it denies.
    fn evaluate(&self, pairs: &[(String, Value)]) -> Result<Vec<String>> {
        let (program, lowered, backend) = (self.program, &self.lowered, &self.backend);
        let mut given = deployment::input_fact_keys(program);
        given.extend(pairs.iter().map(|(k, _)| k.clone()));
        inputs::check_required(&lowered.inputs, &given)?;
        let mut extra = inputs::set_facts(&lowered.inputs, pairs)?;
        extra.extend(self.cli.manifest.iter().flat_map(|m| m.facts()));
        extra.extend(build_extra_facts(&self.cli.data)?);
        extra.extend(backend.catalog(schema::named_types(&lowered.program, &extra).as_ref())?);
        let tables = crate::tables::Tables::with_files(self.reader.clone());
        // Nothing is kept and nothing applied: a memo answers its
        // candidate, `random.*` derive from a master of the test's own.
        let memos =
            crate::memo::Memos::new(&state::State::default(), &crate::custody::Master::none());
        crate::functions::random::set_master(Some(b"dform test".to_vec()), None, self.stack);
        let externs =
            crate::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
                if let Some(r) = tables.answer(f, ins) {
                    return r;
                }
                if let Some(r) = crate::externs::time(f) {
                    return r;
                }
                if let Some(r) = crate::files::oci::stand_in(f, ins) {
                    self.images.set(true);
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
        // What the plan refuses before any provider is asked (R-184): a
        // resource that leaves unset what its schema requires.
        let schema = backend.schema();
        let unset: Vec<String> = ir::compile_resources(res.facts.iter().cloned(), schema)?
            .iter()
            .filter_map(|r| {
                let m = schema.unset_message(&r.addr.typ, &engine::value_to_json(&r.attrs))?;
                let site = report::sites(&res, [&r.addr], None).into_values().next();
                Some(report::Failure::located(&r.addr, m).at(site).to_string())
            })
            .collect();
        if !unset.is_empty() {
            bail!(unset.join("\n"));
        }
        violations.extend(inputs::violations(&res.facts, &lowered.inputs));
        let redact = query::Redactor::new(&res.facts, backend.schema());
        Ok(violations.iter().map(|v| redact.text(v)).collect())
    }

    /// A deny's line, with its doc comment beside it.
    fn documented(&self, d: &str) -> String {
        match self
            .docs
            .iter()
            .filter(|(name, _)| d == *name || d.starts_with(&format!("{name} ")))
            .max_by_key(|(name, _)| name.len())
        {
            Some((_, doc)) => format!("- {d}   #| {doc}"),
            None => format!("- {d}"),
        }
    }

    /// What the test did not ask the world, said once.
    fn notes(&self) {
        if self.images.get() {
            println!(
                "note: no registry is asked: an image's digest is the one this machine last \
                 resolved, else a stand-in"
            );
        }
        let stood = self.reader.stood_in();
        if !stood.is_empty() {
            println!(
                "note: a provider's location is not read, so what reads it is undetermined: {}",
                stood.join(", ")
            );
        }
    }
}

/// A deny's doc comment (`#|` above it) is its test's doc, printed beside
/// it (R-30: with `scenario` gone, the deny is the test).
fn deny_docs(program: &crate::ast::Program) -> BTreeMap<String, String> {
    program
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
        .collect()
}

/// The test's result set: a row per combination, its inputs then its
/// result; and each that failed, as the command that plans it with its
/// denies or its error.
struct Matrix {
    columns: Vec<String>,
    table: Option<report::table::Table>,
    failures: Vec<(String, Vec<String>)>,
}

impl Matrix {
    fn new(columns: Vec<String>) -> Matrix {
        Matrix {
            columns,
            table: None,
            failures: Vec::new(),
        }
    }

    /// A combination's row, `command` the one that plans it.
    fn push(
        &mut self,
        pairs: &[(String, Value)],
        result: &str,
        command: String,
        lines: Vec<String>,
    ) {
        let mut row: Vec<report::table::Cell> = self
            .columns
            .iter()
            .map(|c| match pairs.iter().find(|(k, _)| k == c) {
                Some((_, v)) => report::table::Cell::text(partition::fmt_bare(v)),
                None => report::table::Cell::text("-"),
            })
            .collect();
        let t = self.table.get_or_insert_with(|| {
            let mut columns = self.columns.clone();
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
            self.failures.push((format!("{result}  {command}"), lines));
        }
    }

    /// The table, then each failure; how many failed.
    fn print(self, o: &report::table::Options) -> usize {
        if let Some(t) = self.table {
            print!("{}", t.render(o));
        }
        for (head, lines) in &self.failures {
            println!("{head}");
            for l in lines {
                println!("  {l}");
            }
        }
        self.failures.len()
    }
}
