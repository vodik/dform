//! The project's stacks: `stack list`, `stack rekey`, `stack handover`.

use super::evaluated::Evaluated;
use super::{Cli, Outcome, open_s3};
use crate::{deployment, loader, report, state, store};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// `dform stack list`.
#[derive(Debug, Clone)]
pub(super) struct StackList;

impl StackList {
    /// `dform stack list`, a result set (R-63): one row per deployment with
    /// state (a stack with none, one row saying so), its stack, file and
    /// where its state is, its last apply and a pending saved plan.
    pub(super) fn run(&self, cli: &Cli) -> Result<Outcome> {
        let project = crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
        let d = crate::project::discover(&project);
        for w in &d.warnings {
            eprintln!("warning: {w}");
        }
        d.check()?;
        if d.stacks.is_empty() {
            println!("no stacks in {}", project.root.display());
            return Ok(Outcome::Done);
        }
        let mut listing = Listing::new(cli, &project)?;
        for s in &d.stacks {
            listing.stack(s)?;
        }
        print!("{}", listing.into_table().render(&cli.table));
        Ok(Outcome::Done)
    }
}

/// `stack list`'s table, filled a stack at a time.
struct Listing<'a> {
    cli: &'a Cli,
    registry: std::collections::BTreeMap<String, crate::stack::Entry>,
    /// The project module's deployments (R-114): the matrix is the source
    /// of which there are, the registry of where each one's state is.
    listed: std::collections::BTreeMap<String, bool>,
    label: String,
    table: report::table::Table,
}

impl<'a> Listing<'a> {
    fn new(cli: &'a Cli, project: &crate::project::Project) -> Result<Listing<'a>> {
        let registry = crate::stack::registry(&cli.root)?;
        let (listed, label) = super::matrix::listed(project, &cli.root);
        let table = report::table::Table::new([
            "stack",
            "file",
            "deployment",
            "listed",
            "state",
            "applied",
            "by",
            "commit",
            "result",
            "pending",
        ]);
        Ok(Listing {
            cli,
            registry,
            listed,
            label,
            table,
        })
    }

    /// The table, its empty columns left out.
    fn into_table(self) -> report::table::Table {
        self.table.without_empty_columns()
    }

    /// A row of the stack `s`.
    fn row(&mut self, s: &crate::project::Found, deployment: &str, state: String, last: LastApply) {
        let key = if s.keys.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.keys.join(", "))
        };
        let listed = match self.listed.get(deployment) {
            Some(true) => self.label.clone(),
            Some(false) => format!("removed from {}", self.label),
            None => String::new(),
        };
        let row = [
            format!("{}{key}", s.name),
            s.file.display().to_string(),
            deployment.to_string(),
            listed,
            state,
            last.applied,
            last.by,
            last.commit,
            last.result,
            last.pending,
        ]
        .into_iter()
        .map(report::table::Cell::text)
        .collect();
        self.table.push(row);
    }

    /// The rows of the stack `s`: each deployment with state, each the
    /// project module lists that has none yet, else one row saying none
    /// has.
    fn stack(&mut self, s: &crate::project::Found) -> Result<()> {
        let root = &self.cli.root;
        // Where the stack's deployments are: its backend's, else the state
        // root's.
        let backend = stack_backend(s);
        let base = deployment::stack_location(root, &s.name, backend.as_ref());
        let opener = open_s3(root, false);
        let keys = match base.open(&opener).and_then(|st| st.list("")) {
            Ok(k) => k,
            Err(e) => {
                self.row(s, "", base.to_string(), LastApply::failed(&e));
                return Ok(());
            }
        };
        let deployments = self.deployments(s, backend.as_ref(), &base, &keys);
        // A deployment the module lists that has no state yet.
        let mut unapplied: Vec<String> = self
            .listed
            .iter()
            .filter(|(n, l)| {
                **l && n.split_once('[').map_or(n.as_str(), |(st, _)| st) == s.name
                    && !deployments.iter().any(|(d, _)| d == *n)
            })
            .map(|(n, _)| n.clone())
            .collect();
        let mut any = false;
        for (name, location) in deployments {
            let store = match location.open(&opener) {
                Ok(st) => st,
                Err(e) => {
                    any = true;
                    self.row(s, &name, location.to_string(), LastApply::failed(&e));
                    continue;
                }
            };
            let entries = crate::audit::Log::new(store.clone(), None)
                .entries()
                .unwrap_or_default();
            // A destroyed deployment is gone; its log stays (R-149).
            if (entries.is_empty() && store.get(store::STATE)?.is_none()) || destroyed(&entries) {
                if let Some(true) = self.listed.get(&name) {
                    unapplied.push(name);
                }
                continue;
            }
            any = true;
            let state = match self.registry.get(&name).and_then(|e| e.backend.clone()) {
                Some(b) => format!("handed over to {b}"),
                None => shown(&location),
            };
            self.row(s, &name, state, entries.as_slice().into());
        }
        unapplied.sort();
        unapplied.dedup();
        for name in unapplied {
            any = true;
            let never = LastApply {
                applied: "never".into(),
                ..Default::default()
            };
            self.row(s, &name, String::new(), never);
        }
        if !any {
            let none = LastApply {
                result: "no deployment has state".into(),
                ..Default::default()
            };
            self.row(s, "", shown(&base), none);
        }
        Ok(())
    }

    /// The deployments of the stack `s` that have a place: the stack's own,
    /// or each key value under its backend (`keys`, what `base` holds), and
    /// each the registry has.
    fn deployments(
        &self,
        s: &crate::project::Found,
        backend: Option<&crate::stack::Backend>,
        base: &store::Location,
        keys: &[String],
    ) -> Vec<(String, store::Location)> {
        let root = &self.cli.root;
        let mut deployments: Vec<(String, store::Location)> = Vec::new();
        let keyed = backend.and_then(crate::stack::keyed_parent);
        if s.keys.is_empty() {
            deployments.push((s.name.clone(), base.clone()));
        } else if let (Some(b), Some((parent, rest))) = (backend, keyed) {
            // A backend that names the key (`local("state/app-{env}")`):
            // each place under its directory the template matches.
            let found = deployment::stack_location(root, &s.name, Some(&parent))
                .open(&open_s3(root, false))
                .and_then(|st| st.list(""))
                .unwrap_or_default();
            let mut segs: Vec<String> = found
                .iter()
                .filter_map(|k| crate::stack::template_key(&rest, &s.keys, k))
                .collect();
            segs.sort();
            segs.dedup();
            for seg in segs {
                deployments.push((
                    format!("{}[{seg}]", s.name),
                    deployment::deployment_location(root, &s.name, Some(b), Some(&seg)),
                ));
            }
        } else {
            let mut segs: Vec<&str> = keys
                .iter()
                .filter_map(|k| k.split_once('/').map(|(seg, _)| seg))
                .filter(|seg| seg.contains('='))
                .collect();
            segs.dedup();
            for seg in segs {
                deployments.push((format!("{}[{seg}]", s.name), base.child(Some(seg))));
            }
        }
        for (name, e) in &self.registry {
            let ours = name == &s.name || name.starts_with(&format!("{}[", s.name));
            if ours && !deployments.iter().any(|(n, _)| n == name) {
                deployments.push((name.clone(), e.state.clone()));
            }
        }
        deployments
    }
}

/// Where a deployment's state is, as `stack list` says it: a bucket's
/// place; nothing for the state root's.
fn shown(l: &store::Location) -> String {
    match l {
        store::Location::S3(spec) => spec.to_string(),
        store::Location::Local(_) => String::new(),
    }
}

/// `dform stack rekey STACK K=V...`.
#[derive(Debug, Clone)]
pub(super) struct Rekey {
    pub(super) stack: String,
    pub(super) pairs: Vec<String>,
}

/// `stack rekey`: the deployment whose state moves, and where to.
pub(super) struct Rekeying {
    pub(super) from: crate::stack::Instance,
    pub(super) to: crate::stack::Instance,
}

impl Rekey {
    /// `stack rekey STACK K=V...`: STACK is the program's stack, and keyed; the
    /// pairs are the old key, then the new one, each naming every key input
    /// once (the old one left out: the state from before the stack was keyed).
    /// The run is set to the old values (the new ones for the unkeyed state),
    /// so it evaluates the deployment whose state moves.
    pub(super) fn args(&self, cli: &mut Cli, cfg: &crate::stack::Stack) -> Result<Rekeying> {
        let (files, stack, pairs) = (cli.files.clone(), self.stack.as_str(), &self.pairs);
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
                "stack rekey {stack}: the stack has no key; key it first (`key env: ..` in its file)"
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
        let instance = |key| crate::stack::Instance {
            stack: own.clone(),
            key,
            defaulted: Vec::new(),
        };
        Ok(Rekeying {
            from: instance(from),
            to: instance(to),
        })
    }

    /// The run is of the old deployment (its state, its world): the
    /// provenance of its names is listed, and its state moves.
    pub(super) fn run(&self, run: &Evaluated) -> Result<Outcome> {
        let Some(r) = &run.cx.rekey else {
            unreachable!("Rekey::args ran for rekey");
        };
        let (cli, root) = (&run.cx.cli, &run.cx.root);
        let located = &run.ev.located;
        let (stack, stack_cfg) = (&located.loaded.stack, &located.loaded.cfg);
        let keys: Vec<String> = stack_cfg.keys.iter().map(|(k, _)| k.clone()).collect();
        let named = crate::lint::key_named(&run.ev.res, run.ev.schema(), &keys);
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
        let place = |i: &crate::stack::Instance| {
            let location = deployment::deployment_location(
                root,
                stack,
                stack_cfg.backend.as_ref(),
                i.segment().as_deref(),
            );
            crate::stack::Place {
                world: crate::stack::world_file(&location, &i.dir(&root.join(stack))),
                location,
            }
        };
        let opener = open_s3(root, true);
        let moved = crate::stack::rekey(
            root,
            (&r.from, &place(&r.from)),
            (&r.to, &place(&r.to)),
            &opener,
            located.times,
        )?;
        crate::audit::Log::new(
            moved.open(&opener)?,
            cli.audit_sink.clone().or(stack_cfg.audit_sink.clone()),
        )
        .append(
            "rekey",
            serde_json::json!({
                "from": r.from.name(),
                "to": r.to.name(),
                "who": crate::audit::who(),
            }),
        )?;
        println!(
            "stack {} rekeyed to {}: {moved}",
            r.from.name(),
            r.to.name(),
        );
        Ok(Outcome::Done)
    }
}

/// `dform stack handover STACK --to BACKEND` (R-41, experimental).
#[derive(Debug, Clone)]
pub(super) struct Handover {
    pub(super) stack: String,
    pub(super) to: String,
}

impl Handover {
    pub(super) fn run(&self, cli: &Cli) -> Result<Outcome> {
        let (stack, to) = (&self.stack, &self.to);
        let (from, home, times) = self.place(cli)?;
        let opener = open_s3(&cli.root, true);
        self.seal(cli, &from, &opener, times)?;
        let moved = crate::stack::handover(&cli.root, stack, &from, &home, to, &opener, times)?;
        crate::audit::Log::new(moved.open(&opener)?, cli.audit_sink.clone()).append(
            "handover",
            serde_json::json!({ "stack": stack, "to": to, "who": crate::audit::who() }),
        )?;
        println!("stack {stack} handed over to {to}: {moved}");
        Ok(Outcome::Done)
    }

    /// Before `stack handover NAME --to s3(..)`: a bucket never holds a key
    /// file (R-164), so a deployment whose master is one is sealed first, as
    /// dform.toml's `[secrets]` says (the same master: nothing derived
    /// changes), and the sealed master is what moves; with no `[secrets]` the
    /// handover is refused, naming the setting.
    fn seal(
        &self,
        cli: &Cli,
        from: &crate::stack::Place,
        s3: store::OpenS3,
        times: store::LeaseTimes,
    ) -> Result<()> {
        let (name, to) = (self.stack.as_str(), self.to.as_str());
        if !to.trim_start().starts_with("s3") {
            return Ok(());
        }
        let src = from.location.open(s3)?;
        if crate::zset::file::Key::load(src.as_ref())?.is_none() {
            return Ok(());
        }
        let stack = name.split_once('[').map_or(name, |(s, _)| s);
        let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
        let mixing = crate::custody::Mixing::of(project.as_ref().map(|p| &p.manifest), stack)?;
        if mixing.key_file() {
            bail!(
                "handover {name} to {to}: its master is the key file {}, and a bucket never holds \
                 one (read access to the state would be read access to every derived secret): set \
                 `[secrets] passphrase = \"env:NAME\"` (or `recipients = [\"age1..\"]`) in \
                 dform.toml, and the handover seals it",
                src.locate(store::KEY)
            );
        }
        let dep = store::Deployment::new(src.clone(), name, times);
        let master = dep.master(&mixing, crate::custody::Want::default())?;
        if master.key.is_none() {
            bail!(
                "handover {name} to {to}: sealing its key file needs the master: {}",
                master.without.as_deref().unwrap_or("not held")
            );
        }
        match crate::custody::reseal(src.as_ref(), name, &master, &mixing)? {
            Some(done) if done.key_file => {
                dep.audit(cli.audit_sink.clone(), crate::audit::SINK_TIMEOUT, false)
                    .append(
                        "custody",
                        serde_json::json!({
                            "sealed": store::KEY,
                            "into": store::MASTER,
                            "id": master.id,
                            "who": crate::audit::who(),
                        }),
                    )?;
                eprintln!(
                    "{name}: its key file is sealed into {} for the handover ({})",
                    store::MASTER,
                    mixing.describe()
                );
                Ok(())
            }
            _ => bail!(
                "handover {name} to {to}: its key file could not be sealed ({}): a bucket never holds \
                 one",
                mixing.describe()
            ),
        }
    }

    /// Where the deployment `name` (`app`, `app[env=prod]`) is, for a command
    /// that runs no program: where the registry has it, else where its stack's
    /// program's backend says, else under the state root. With its default
    /// directory (its world's when its state is in a bucket) and the lease
    /// times.
    fn place(&self, cli: &Cli) -> Result<(crate::stack::Place, PathBuf, store::LeaseTimes)> {
        let name = self.stack.as_str();
        let root = &cli.root;
        let home = crate::stack::instance_dir(root, name);
        let project = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?;
        let times = project
            .as_ref()
            .map(|p| p.manifest.lease_times())
            .unwrap_or_default();
        let location = match crate::stack::registry(root)?.remove(name) {
            Some(e) => e.state,
            None => {
                let (stack, seg) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                    Some((stack, seg)) => (stack, Some(seg)),
                    None => (name, None),
                };
                let backend = project.as_ref().and_then(|p| {
                    let d = crate::project::discover(p);
                    match d.named(stack).as_slice() {
                        [one] => stack_backend(one),
                        _ => None,
                    }
                });
                deployment::deployment_location(root, stack, backend.as_ref(), seg)
            }
        };
        let place = crate::stack::Place {
            world: crate::stack::world_file(&location, &home),
            location,
        };
        Ok((place, home, times))
    }
}

/// The audit log `entries` end in a `destroy` that completed: no apply
/// started since (R-149).
pub(super) fn destroyed(entries: &[serde_json::Value]) -> bool {
    entries
        .iter()
        .rev()
        .find_map(|e| match e["kind"].as_str() {
            Some("destroyed") => Some(true),
            Some("apply_start") => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// A deployment's last apply and pending saved plan, from its audit log.
#[derive(Debug, Default)]
struct LastApply {
    applied: String,
    by: String,
    commit: String,
    result: String,
    pending: String,
}

impl LastApply {
    /// A deployment whose state could not be read: the error, as its
    /// result.
    fn failed(e: &anyhow::Error) -> LastApply {
        LastApply {
            result: format!("{e:#}"),
            ..Default::default()
        }
    }
}

impl From<&[serde_json::Value]> for LastApply {
    fn from(entries: &[serde_json::Value]) -> LastApply {
        let field = |e: &serde_json::Value, k: &str| e[k].as_str().unwrap_or("").to_string();
        let start = entries.iter().rposition(|e| e["kind"] == "apply_start");
        let mut out = match start {
            None => LastApply {
                applied: "never".into(),
                ..Default::default()
            },
            Some(i) => {
                let e = &entries[i];
                let end = entries[i..]
                    .iter()
                    .find(|e| e["kind"] == "apply_end")
                    .map(|e| field(e, "result"))
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "running or interrupted".into());
                LastApply {
                    applied: field(e, "time"),
                    by: field(e, "who"),
                    commit: e["commit"]
                        .as_str()
                        .map(|c| crate::report::short_id(c).to_string())
                        .unwrap_or_default(),
                    result: end,
                    pending: String::new(),
                }
            }
        };
        let plan = entries
            .iter()
            .rposition(|e| e["kind"] == "plan" && e["file"].is_string() && e["digest"].is_string());
        if let Some(p) = plan
            && start.is_none_or(|s| p > s)
        {
            out.pending = format!(
                "{} ({})",
                field(&entries[p], "file"),
                field(&entries[p], "digest")
            );
        }
        out
    }
}

/// The backend of the project's stack `found`, as the manifest says;
/// `None` when it says none.
fn stack_backend(found: &crate::project::Found) -> Option<crate::stack::Backend> {
    let program = loader::load_program(std::slice::from_ref(&found.file)).ok()?;
    crate::stack::config(&program).ok()?.backend
}
