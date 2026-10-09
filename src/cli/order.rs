//! The stacks `apply X` applies first: the deployments X reads, each
//! before its readers (R-30, R-73).

use super::{Cli, Cmd, Outcome, run};
use crate::report;
use crate::{deployment, zset};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One deployment `apply` applies, of the stack in `file`, and the
/// stack's inputs (not its key).
pub(super) struct Dependency {
    /// As a reader names it, `platform[env=lab]`.
    pub(super) name: String,
    /// Its full name, `stacks.platform[env=lab]` (R-200).
    pub(super) full: String,
    /// The deployments it reads, by name.
    pub(super) reads: Vec<String>,
    pub(super) file: PathBuf,
    pub(super) keys: Vec<(String, String)>,
    pub(super) inputs: BTreeSet<String>,
    /// Its whole key, a key `keys` leaves out at its default.
    pub(super) key: Vec<(String, String)>,
    /// One of the roots the order was asked for, not a deployment one of
    /// them reads.
    pub(super) root: bool,
}

impl Cli {
    /// `apply X` in a project: the deployments of the project's stacks X
    /// reads (a keyed read of a deployment, R-73), and theirs, each before its
    /// readers, then X; nothing that reads X (R-30: the stack is the unit of
    /// partial work).
    /// Empty when X reads none, and for a plan file, a world fixture or a
    /// program outside a project. A cycle is an error naming it.
    /// `plan X` in a project: the deployments X reads and theirs, each
    /// before its readers, then X (R-200), what `apply X` applies; `--out`
    /// writes them all to the plan file, in that order. Empty when X reads
    /// none, and for the bare diff, a destroy, a world fixture or a program
    /// outside a project.
    pub(super) fn plan_order(&self) -> Result<Vec<Dependency>> {
        // The bare diff (`-q`) is a script's: one deployment's, as it was.
        let Cmd::Plan(super::plan::Plan {
            destroy: false,
            why,
            ..
        }) = &self.cmd
        else {
            return Ok(Vec::new());
        };
        if *why == crate::report::Why::None {
            return Ok(Vec::new());
        }
        if !self.in_project || self.world.is_some() {
            return Ok(Vec::new());
        }
        let [one] = self.files.as_slice() else {
            return Ok(Vec::new());
        };
        let mut order = order_of(&[(one.clone(), self.keys.clone())])?;
        if order.len() == 1 {
            order.clear();
        }
        Ok(order)
    }

    pub(super) fn apply_order(&self) -> Result<Vec<Dependency>> {
        let Cmd::Apply(super::apply::Apply {
            plan_file: None,
            destroy: false,
            ..
        }) = &self.cmd
        else {
            return Ok(Vec::new());
        };
        if !self.in_project || self.world.is_some() {
            return Ok(Vec::new());
        }
        // The project (`apply` with no target): every stack, each with its
        // default key; else the target.
        let roots: Vec<(PathBuf, Vec<(String, String)>)> = match self.files.as_slice() {
            [] => self
                .every_stack
                .iter()
                .map(|f| (f.clone(), Vec::new()))
                .collect(),
            [one] => vec![(one.clone(), self.keys.clone())],
            _ => return Ok(Vec::new()),
        };
        let mut order = order_of(&roots)?;
        if order.len() == 1 && self.every_stack.is_empty() {
            order.clear();
        }
        Ok(order)
    }
}

/// The deployments of `roots`, each a stack's file and the key values a
/// target gives, and those they read (R-30), each before its readers, in
/// the roots' order otherwise. A cycle is an error naming it.
pub(super) fn order_of(roots: &[(PathBuf, Vec<(String, String)>)]) -> Result<Vec<Dependency>> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let Some(project) = crate::project::Project::find(Path::new("."), env!("CARGO_PKG_VERSION"))?
    else {
        return Ok(Vec::new());
    };
    let mut order = Order {
        found: crate::project::discover(&project),
        project,
        order: Vec::new(),
        path: Vec::new(),
    };
    for (file, keys) in roots {
        let (instance, inputs, _) = order.reads(file, keys)?;
        let target = Dependency {
            name: instance.name(),
            full: instance.full_name(),
            reads: Vec::new(),
            file: file.clone(),
            keys: keys.clone(),
            inputs,
            key: instance.key,
            root: true,
        };
        // A root another root read first is a root still.
        if let Some(d) = order.order.iter_mut().find(|d| d.name == target.name) {
            d.root = true;
            continue;
        }
        order.visit(target)?;
    }
    Ok(order.order)
}

/// The deployments of a project in apply order, as they are found: each
/// visited after those it reads.
struct Order {
    project: crate::project::Project,
    found: crate::project::Discovered,
    /// The deployments in apply order so far.
    order: Vec<Dependency>,
    /// The reads being followed, for a cycle's message.
    path: Vec<String>,
}

impl Order {
    /// The deployment `d`, after the deployments it reads; a cycle is an
    /// error naming it.
    fn visit(&mut self, mut d: Dependency) -> Result<()> {
        if self.order.iter().any(|o| o.name == d.name) {
            return Ok(());
        }
        if let Some(i) = self.path.iter().position(|p| *p == d.name) {
            bail!(
                "apply {}: the stacks read each other's outputs in a cycle: {} -> {}",
                self.path[0],
                self.path[i..].join(" -> "),
                d.name
            );
        }
        let (_, inputs, deps) = self.reads(&d.file, &d.keys)?;
        d.inputs = inputs;
        d.reads = deps.iter().map(|x| x.name.clone()).collect();
        self.path.push(d.name.clone());
        for dep in deps {
            self.visit(dep)?;
        }
        self.path.pop();
        self.order.push(d);
        Ok(())
    }

    /// The stack in `file` keyed by `keys`: its name, its inputs (not its
    /// key), and the deployments it reads that are this project's.
    fn reads(
        &self,
        file: &Path,
        keys: &[(String, String)],
    ) -> Result<(crate::stack::Instance, BTreeSet<String>, Vec<Dependency>)> {
        let t = deployment::Target {
            files: vec![file.to_path_buf()],
            input_files: Vec::new(),
            providers: Vec::new(),
        };
        let loaded = deployment::load(
            &t,
            env!("CARGO_PKG_VERSION"),
            &|p: &Path| std::fs::read_to_string(p),
            &mut deployment::Notes::default(),
        )?;
        // The key the target gives, a key it does not at its default.
        let given: Vec<crate::ast::Atom> = keys
            .iter()
            .map(|(k, v)| {
                crate::ast::atom(
                    "input",
                    vec![crate::ast::str_term(k), crate::ast::str_term(v)],
                    Default::default(),
                )
            })
            .collect();
        let named = (loaded.stack.as_str(), loaded.path.as_str());
        let instance = crate::stack::instance(&loaded.cfg, named, &loaded.program, &given)
            .unwrap_or_else(|_| crate::stack::Instance {
                stack: loaded.stack.clone(),
                path: loaded.path.clone(),
                key: keys.to_vec(),
                defaulted: Vec::new(),
            });
        let (mut names, any) =
            crate::stack::reads(&loaded.program, &loaded.deployed, &instance.key);
        // A deployment named by what the program computes: any of the
        // stack's may be read, so every one there is goes first.
        if !any.is_empty() {
            for name in crate::stack::registry(&self.project.state_root())?.into_keys() {
                // As the program reads it (`platform[env=lab]`).
                let name = crate::stack::short_of(&name);
                let stack = name.split_once('[').map_or(name.as_str(), |(s, _)| s);
                if any.contains(stack) {
                    names.insert(name);
                }
            }
        }
        let mut deps = Vec::new();
        for name in names {
            if let Some(d) = self.dependency(name)? {
                deps.push(d);
            }
        }
        let inputs = loaded
            .program
            .statements
            .iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Input(i) if !i.key => Some(i.name.clone()),
                _ => None,
            })
            .collect();
        Ok((instance, inputs, deps))
    }

    /// The deployment `name` (`app[env=prod]`) read, when it is of a stack
    /// of this project; another project's (`acme.platform`) is that
    /// project's to apply.
    fn dependency(&self, name: String) -> Result<Option<Dependency>> {
        let (stack, key) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
            Some((s, k)) => (s.to_string(), k),
            None => (name.clone(), ""),
        };
        let [one] = self.found.named(&stack)[..] else {
            return Ok(None);
        };
        let keys: Vec<(String, String)> = key
            .split(',')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .ok_or_else(|| anyhow::anyhow!("a read of {name}: expected K=V in the key"))
            })
            .collect::<Result<_>>()?;
        Ok(Some(Dependency {
            full: format!("{}{}", one.path, &name[stack.len()..]),
            reads: Vec::new(),
            file: one.file.clone(),
            key: keys.clone(),
            keys,
            name,
            inputs: BTreeSet::new(),
            root: false,
        }))
    }
}

/// `apply X` in a project: the deployments X reads, then X, each planned,
/// confirmed and applied in turn, a run of its own; one declined or
/// stopped ends the command there, before the stacks that read it.
pub(super) struct InOrder {
    cli: Cli,
    order: Vec<Dependency>,
}

impl InOrder {
    pub(super) fn new(cli: Cli, order: Vec<Dependency>) -> InOrder {
        InOrder { cli, order }
    }

    pub(super) fn run(self) -> Result<Outcome> {
        self.say();
        self.check_sets()?;
        let (last, deps) = self
            .order
            .split_last()
            .ok_or_else(|| anyhow::anyhow!("internal: an apply order with no deployment"))?;
        for d in deps {
            self.header(&d.full);
            let mut cli = self.of(d);
            cli.input_files = Vec::new();
            match run(cli, None)? {
                Outcome::Done => {}
                o => return Ok(o),
            }
        }
        self.header(&last.full);
        if !self.cli.every_stack.is_empty() {
            // The project's last stack: run as a dependency is, by its file.
            return run(self.of(last), None);
        }
        run(self.target(last), None)
    }

    /// The target's plan of what it reads, then its own (R-200): one
    /// tree, each planned against the outputs of those before it.
    pub(super) fn plan(self) -> Result<Outcome> {
        let of = |d: &Dependency, cmd: Cmd| {
            let mut cli = match d.root {
                true => self.target(d),
                false => {
                    let mut cli = self.of(d);
                    cli.input_files = Vec::new();
                    cli
                }
            };
            cli.cmd = cmd;
            cli
        };
        super::matrix::Tree {
            cli: &self.cli,
            label: None,
            sites: Default::default(),
        }
        .plan(&self.order, &[], &of)
    }

    /// The run of the target `last`: a `--set` of an input it declares,
    /// or one no stack of the run declares, which names the error, is its.
    fn target(&self, last: &Dependency) -> Cli {
        let mut cli = self.cli.clone();
        cli.user_set.retain(|kv| {
            let k = named_input(kv);
            last.inputs.contains(&k) || !self.order.iter().any(|d| d.inputs.contains(&k))
        });
        cli.set = cli.user_set.clone();
        cli.set
            .extend(cli.keys.iter().map(|(k, v)| format!("{k}={v}")));
        cli
    }

    /// The `stacks:` line (R-79): the deployments in apply order. Each is
    /// planned, confirmed and applied in turn, so its ticks are its own
    /// plan's, printed under its name.
    fn say(&self) {
        let named: Vec<String> = self.order.iter().map(|d| d.full.clone()).collect();
        let target = named.last().cloned().unwrap_or_default();
        let deps = &named[..named.len() - 1];
        if self.cli.every_stack.is_empty() {
            println!(
                "stacks: {}, then {target} below, in apply order: {target} reads {}; each is \
                 planned, confirmed and applied in turn",
                deps.join(", then "),
                if deps.len() == 1 {
                    "its outputs"
                } else {
                    "their outputs"
                }
            );
        } else {
            println!(
                "stacks: the project's {}, in apply order: {}; each is planned, confirmed and \
                 applied in turn",
                named.len(),
                named.join(", then ")
            );
        }
    }

    /// The project has no target to name the error: a `--set` no stack
    /// declares is one now.
    fn check_sets(&self) -> Result<()> {
        if self.cli.every_stack.is_empty() {
            return Ok(());
        }
        if let Some(kv) = self.cli.user_set.iter().find(|kv| {
            !self
                .order
                .iter()
                .any(|d| d.inputs.contains(&named_input(kv)))
        }) {
            bail!(
                "--set {kv}: no stack of the project declares input {}",
                named_input(kv)
            );
        }
        Ok(())
    }

    /// `== NAME` above a deployment's run.
    fn header(&self, name: &str) {
        println!(
            "{}",
            self.cli
                .style
                .paint(report::Paint::Bold, &format!("== {name}"))
        );
    }

    /// The run of the deployment `d`, by its file and key: a `--set` goes
    /// to each stack of the run that declares the input; one none declares
    /// stays the target's, which names the error.
    fn of(&self, d: &Dependency) -> Cli {
        let mut cli = self.cli.clone();
        cli.user_set = self
            .cli
            .user_set
            .iter()
            .filter(|kv| d.inputs.contains(&named_input(kv)))
            .cloned()
            .collect();
        cli.set = cli.user_set.clone();
        cli.set
            .extend(d.keys.iter().map(|(k, v)| format!("{k}={v}")));
        cli.keys = d.keys.clone();
        cli.files = vec![d.file.clone()];
        cli
    }
}

/// The input a `--set K=V` names.
fn named_input(kv: &str) -> String {
    kv.split_once('=').map_or(kv, |(k, _)| k).to_string()
}

/// `apply PLAN` of a plan file of several deployments (R-200): which one
/// this run is, and the digest of what each earlier one published once
/// applied, by the name a reader gives it: what a reader was planned
/// against is valid once its dependency's apply produced it.
#[derive(Debug, Clone, Default)]
pub(super) struct InSequence {
    pub(super) step: usize,
    pub(super) applied: BTreeMap<String, String>,
}

/// `apply PLAN` of a plan file of several deployments: each applied in
/// the file's order, a run of its own asking before it (or `--yes`) and
/// checked against its plan at its turn; the first that does not end
/// done ends the command there, with its status.
pub(super) struct Sequenced {
    cli: Cli,
    path: PathBuf,
    file: zset::file::Sequence,
}

impl Sequenced {
    pub(super) fn new(cli: Cli, path: PathBuf, file: zset::file::Sequence) -> Sequenced {
        Sequenced { cli, path, file }
    }

    pub(super) fn run(self) -> Result<Outcome> {
        let named: Vec<&str> = self
            .file
            .deployments
            .iter()
            .map(|s| s.deployment.as_str())
            .collect();
        println!(
            "stacks: {}, in apply order as {} planned them; each is confirmed and applied in turn",
            named.join(", then "),
            self.path.display()
        );
        let mut applied = BTreeMap::new();
        for (i, step) in self.file.deployments.iter().enumerate() {
            println!(
                "{}",
                self.cli
                    .style
                    .paint(report::Paint::Bold, &format!("== {}", step.deployment))
            );
            match run(self.of(i, step, &applied), None)? {
                Outcome::Done => {}
                o => return Ok(o),
            }
            // What it published, for those after it that read it.
            let reader = crate::stack::short_of(&step.deployment);
            let s3 = super::open_s3(&self.cli.root, false);
            if let Some(d) = crate::stack::published_digest(&self.cli.root, &step.deployment, &s3)?
            {
                applied.insert(reader, d);
            }
        }
        Ok(Outcome::Done)
    }

    /// The run of the file's deployment `i`: its program and inputs the
    /// file's, a `--set` it records; an approval only where it needs one.
    fn of(&self, i: usize, step: &zset::file::Step, applied: &BTreeMap<String, String>) -> Cli {
        let mut cli = self.cli.clone();
        let recorded: BTreeSet<String> = step
            .plan
            .inputs
            .set
            .iter()
            .filter_map(|s| match s {
                serde_json::Value::String(kv) => Some(named_input(kv)),
                s => {
                    let label = s["sensitive"].as_str()?;
                    Some(label.rsplit_once('#').map_or(label, |(_, k)| k).to_string())
                }
            })
            .collect();
        cli.user_set
            .retain(|kv| recorded.contains(&named_input(kv)));
        cli.set = cli.user_set.clone();
        cli.files = Vec::new();
        cli.keys = Vec::new();
        if step.plan.needs_approval.is_empty()
            && let Cmd::Apply(a) = &mut cli.cmd
        {
            a.approval = None;
        }
        cli.sequence = Some(InSequence {
            step: i,
            applied: applied.clone(),
        });
        cli
    }
}
