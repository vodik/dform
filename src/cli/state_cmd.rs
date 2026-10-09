//! The commands that read or move a located deployment's objects:
//! `log`, `state show`, `output`, `state mv`, `state forget-host`,
//! `stack unlock`.

use super::Outcome;
use super::evaluated::Objects;
use crate::value::Value;
use crate::{ir, query, report, state, store};
use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// `dform log [verify]`.
#[derive(Debug, Clone)]
pub(super) struct Log {
    pub(super) verify: bool,
    pub(super) since: Option<String>,
    pub(super) json: bool,
}

impl Log {
    /// `dform log [verify]`: the deployment's audit log.
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        let (log, deployment) = (&cx.audit, cx.deployment());
        let (verify, since, json) = (self.verify, self.since.as_deref(), self.json);
        let at = log.locate();
        if verify {
            let Some(text) = log.text()? else {
                bail!("stack {deployment} has no audit log at {at}");
            };
            return match crate::audit::verify(&text) {
                (n, None) => {
                    println!("audit log {at}: {n} entries, the chain holds");
                    Ok(Outcome::Done)
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
        Ok(Outcome::Done)
    }
}

/// `dform stack unlock`: the apply lock of an apply that is gone, removed.
#[derive(Debug, Clone)]
pub(super) struct Unlock;

impl Unlock {
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        println!("{}", cx.dep().unlock()?);
        Ok(Outcome::Done)
    }
}

/// `dform state show [--address ADDR] [--from-log]`.
#[derive(Debug, Clone)]
pub(super) struct StateShow {
    pub(super) addr: Option<String>,
    pub(super) from_log: bool,
}

impl StateShow {
    /// `dform state show [ADDR]`, a result set (R-63): the deployment's
    /// objects, one row per address (with ADDR, that object's only), then its
    /// outputs as a key/value table.
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        let (dep, o) = (cx.dep(), &cx.cli.table);
        let (only, from_log) = (self.addr.as_deref(), self.from_log);
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
            return Ok(Outcome::Done);
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
            Some(f) if f.destroy => {
                println!("a destroy was interrupted: the next destroy resumes it")
            }
            Some(_) => println!("an apply was interrupted: the next apply resumes it"),
            None => {}
        }
        Ok(Outcome::Done)
    }
}

/// `dform output TARGET [NAME]`.
#[derive(Debug, Clone)]
pub(super) struct Output {
    pub(super) name: Option<String>,
    pub(super) json: bool,
}

impl Output {
    /// `dform output TARGET [NAME]`, a result set (R-63): the deployment's
    /// outputs as of its last apply, what other stacks read. The scalars are
    /// a key/value table and each relation (`output p`) its own table headed
    /// by its name, its columns its `decl`'s. With NAME, that output's value
    /// for the shell: a string's bytes as they are, another scalar in surface
    /// spelling, a relation's rows tab-separated. `--json` is the same, as
    /// JSON. A secret output prints as `secret`: state keeps no bytes of it.
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        let dep = cx.dep();
        let deployment = dep.name();
        if !dep.has_state()? {
            bail!(
                "stack {deployment} has no state at {}: it was never applied",
                dep.locate(crate::store::STATE)
            );
        }
        let outputs = Outputs::new(&cx.located.loaded.program, dep.load_state()?);
        match self.name.as_deref() {
            None => outputs.print_all(deployment, self.json, &cx.cli.table)?,
            Some(name) => outputs.print_one(deployment, name, self.json)?,
        }
        Ok(Outcome::Done)
    }
}

/// A deployment's outputs as of its last apply, as `output` prints them.
struct Outputs<'a> {
    program: &'a crate::ast::Program,
    st: state::State,
    /// The outputs that are relations (`output p`).
    relations: BTreeSet<&'a str>,
    redact: query::Redactor,
}

impl<'a> Outputs<'a> {
    fn new(program: &'a crate::ast::Program, st: state::State) -> Outputs<'a> {
        let relations = program
            .statements
            .iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Output(o) if o.relation.is_some() => Some(o.name.as_str()),
                _ => None,
            })
            .collect();
        Outputs {
            program,
            st,
            relations,
            redact: query::Redactor::default(),
        }
    }

    /// A relation's rows as a table: one row per list of values.
    fn rows(&self, k: &str, v: &Value) -> report::table::Table {
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
                self.program
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
        let mut t = report::table::Table::new(query::columns(k, arity, self.program));
        for r in rows {
            t.push(
                r.iter()
                    .map(|v| report::table::Cell::value(v, &self.redact))
                    .collect(),
            );
        }
        t
    }

    /// Every output: the scalars as a key/value table, each relation its
    /// own table.
    fn print_all(&self, deployment: &str, json: bool, o: &report::table::Options) -> Result<()> {
        let mut scalars = output_table(&self.st);
        scalars
            .rows
            .retain(|r| !self.relations.contains(r[0].text_of()));
        let blocks: Vec<(String, report::table::Table)> = self
            .st
            .outputs
            .iter()
            .filter(|(k, _)| self.relations.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), self.rows(k, v)))
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
        Ok(())
    }

    /// The output `name`'s value for the shell.
    fn print_one(&self, deployment: &str, name: &str, json: bool) -> Result<()> {
        let st = &self.st;
        if st.secret_outputs.contains_key(name) {
            bail!(
                "output {name} of stack {deployment} is secret: its state keeps no bytes of it, \
                 only its label and digest"
            );
        }
        let Some(v) = st.outputs.get(name) else {
            let mut names: Vec<&String> =
                st.outputs.keys().chain(st.secret_outputs.keys()).collect();
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
            v => self.redact.surface(v),
        };
        if self.relations.contains(name) {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&self.rows(name, v).json())?
                );
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
            println!("{}", serde_json::to_string_pretty(&self.redact.json(v))?);
        } else if let Value::Str(s) = v {
            use std::io::Write;
            std::io::stdout().write_all(s.as_bytes())?;
        } else {
            println!("{}", bare(v));
        }
        Ok(())
    }
}

/// `dform state mv FROM TO`.
#[derive(Debug, Clone)]
pub(super) struct StateMv {
    pub(super) from: String,
    pub(super) to: String,
}

impl StateMv {
    /// `dform state mv FROM TO`: the object state maps at FROM, at TO; under
    /// the deployment's lock, logged.
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        let (dep, audit) = (cx.dep(), &cx.audit);
        let (from, to) = (self.from.as_str(), self.to.as_str());
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
        st.apply_moves(&[(old, new)])?;
        dep.save_state(&st)?;
        audit.append(
            "state_mv",
            serde_json::json!({ "from": from, "to": to, "who": crate::audit::who() }),
        )?;
        println!("moved {from} to {to} in stack {deployment}");
        lock.release()?;
        Ok(Outcome::Done)
    }
}

/// `dform state forget-host HOST`.
#[derive(Debug, Clone)]
pub(super) struct ForgetHost {
    pub(super) host: String,
}

impl ForgetHost {
    pub(super) fn run(&self, cx: &Objects) -> Result<Outcome> {
        let (dep, audit) = (cx.dep(), &cx.audit);
        let host = self.host.as_str();
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
        lock.release()?;
        Ok(Outcome::Done)
    }
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
