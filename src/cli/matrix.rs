//! `plan`, `apply` and `test` on the project module (R-114): each
//! deployment it lists, and those they read, in dependency order; a
//! deployment the project's apply made that it no longer lists is
//! destroyed. `dev effects`: each stack it lists, once.

use super::apply::Apply;
use super::args::Target;
use super::dev::Effects;
use super::plan::Plan;
use super::test::Test;
use super::{Cli, Cmd, Dependency, Held, Outcome, Refused};
use crate::matrix::{Kept, Made, Matrix};
use crate::provider::ActionKind;
use crate::report;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// The project module a command runs on: with no target, the root's
/// project.df for `plan`, `apply`, `test` and `dev effects`; a target
/// that is a project module. `destroy` takes a target always: it removes
/// one deployment.
pub(super) fn target(
    cmd: &Cmd,
    project: Option<&crate::project::Project>,
    t: &Target,
) -> Result<Option<PathBuf>> {
    let module = match (&t.target, t.keys.is_empty()) {
        (None, true) => {
            let module = project.and_then(|p| crate::matrix::module_at(&p.root));
            if cmd.destroys() && matches!(cmd, Cmd::Apply(_)) {
                let listed = match &module {
                    Some(m) => match Matrix::load(m) {
                        Ok(m) if !m.listed.is_empty() => {
                            let names: Vec<String> = m.listed.iter().map(|l| l.target()).collect();
                            format!("; the project lists {}", names.join(", "))
                        }
                        _ => String::new(),
                    },
                    None => String::new(),
                };
                bail!(
                    "destroy needs a target: it removes one deployment, named \
                     (`dform destroy STACK K=V`){listed}"
                );
            }
            match module {
                Some(m) => m,
                None => return Ok(None),
            }
        }
        (Some(f), true)
            if f.ends_with(".df")
                && Path::new(f).is_file()
                && crate::loader::is_project_module(Path::new(f)) =>
        {
            PathBuf::from(f)
        }
        _ => return Ok(None),
    };
    let shown = match &t.target {
        Some(f) => f.clone(),
        None => crate::project::PROJECT_MODULE.to_string(),
    };
    match cmd {
        Cmd::Plan(Plan {
            out: None,
            destroy: false,
            ..
        })
        | Cmd::Apply(Apply {
            plan_file: None,
            destroy: false,
            ..
        })
        | Cmd::Test(_)
        | Cmd::Status(_)
        | Cmd::Effects(Effects { json: false }) => Ok(Some(module)),
        Cmd::Apply(Apply { destroy: true, .. }) => bail!(
            "destroy {shown}: a project module lists deployments; destroy removes one, named \
             (`dform destroy STACK K=V`)"
        ),
        Cmd::Plan(_) => bail!(
            "plan {shown}: --out and --destroy plan one deployment; name it (`dform plan STACK \
             K=V --out plan.json`)"
        ),
        Cmd::Effects(_) => bail!(
            "dev effects {shown}: --json says one stack's effects; name it (`dform dev effects \
             STACK --json`)"
        ),
        // Another command with no target runs on the stack under the
        // working directory, as in a project with no project module.
        _ if t.target.is_none() => Ok(None),
        _ => bail!("{shown} is a project module, no stack: name one deployment"),
    }
}

/// `plan`, `apply`, `test` or `dev effects` on the project module
/// `module`.
pub(super) fn run(cli: Cli, module: &Path) -> Result<Outcome> {
    let project = crate::project::Project::require(Path::new("."), env!("CARGO_PKG_VERSION"))?;
    let matrix = Matrix::load(module)?;
    let found = crate::project::discover(&project);
    for w in &found.warnings {
        eprintln!("warning: {w}");
    }
    found.check()?;
    // The module as the project names it, its path from the root.
    let label = std::fs::canonicalize(module)
        .ok()
        .and_then(|m| {
            let root = std::fs::canonicalize(&project.root).ok()?;
            Some(m.strip_prefix(root).ok()?.display().to_string())
        })
        .unwrap_or_else(|| module.display().to_string());
    let file_of = |stack: &str| match found.named(stack)[..] {
        [one] => Some(one.file.clone()),
        _ => None,
    };
    let mut roots = Vec::new();
    for l in &matrix.listed {
        let Some(file) = file_of(&l.stack) else {
            bail!(
                "{label}: {} lists a deployment of no stack of the project",
                l.target()
            );
        };
        roots.push((file, l.key.clone()));
    }
    let order = super::order::order_of(&roots)?;
    // What an apply of the module made that it lists no more: destroyed,
    // readers first.
    let mut gone = Vec::new();
    for (name, kept) in Made::load(&cli.root)?.of(&label) {
        if order.iter().any(|d| d.name == name) {
            continue;
        }
        let Some(file) = file_of(&kept.stack) else {
            bail!(
                "{name}: {label} lists it no more, and the project has no stack {}: its destroy \
                 needs its program; restore the stack's file, destroy {name}, then remove it",
                kept.stack
            );
        };
        gone.push((name, file, kept.key));
    }
    let removed = {
        let roots: Vec<_> = gone
            .iter()
            .map(|(_, f, k)| (f.clone(), k.clone()))
            .collect();
        let mut order = super::order::order_of(&roots)?;
        order.retain(|d| gone.iter().any(|(n, _, _)| *n == d.name));
        order.reverse();
        order
    };
    if !cli.input_files.is_empty() {
        bail!("--input-file gives one deployment's inputs: name it as the target");
    }
    // A `--set` goes to each deployment's stack that declares the input;
    // one none declares is an error.
    let input = |kv: &String| {
        kv.split_once('=')
            .map_or(kv.as_str(), |(k, _)| k)
            .to_string()
    };
    if let Some(kv) = cli.user_set.iter().find(|kv| {
        !order
            .iter()
            .chain(&removed)
            .any(|d| d.inputs.contains(&input(kv)))
    }) {
        bail!(
            "--set {kv}: no stack {label} lists declares input {}",
            input(kv)
        );
    }
    let of = |d: &Dependency, cmd: Cmd| {
        let mut dep = cli.clone();
        dep.cmd = cmd;
        dep.matrix = None;
        dep.user_set = cli
            .user_set
            .iter()
            .filter(|kv| d.inputs.contains(&input(kv)))
            .cloned()
            .collect();
        dep.set = dep.user_set.clone();
        dep.set
            .extend(d.key.iter().map(|(k, v)| format!("{k}={v}")));
        dep.keys = d.key.clone();
        dep.files = vec![d.file.clone()];
        dep
    };
    match &cli.cmd {
        Cmd::Plan(_) => plan(&cli, &label, &matrix, &order, &removed, &of),
        Cmd::Apply(_) => apply(&cli, &label, &order, &removed, &of),
        Cmd::Test(_) => test(&cli, &label, &order, &of),
        Cmd::Effects(_) => effects(&cli, &label, &order, &of),
        Cmd::Status(_) => super::status::matrix(&cli, &order, &of),
        _ => bail!("internal: a project module runs plan, apply, test and dev effects"),
    }
}

type For<'a> = dyn Fn(&Dependency, Cmd) -> Cli + 'a;

/// The `== NAME` line a deployment's run is headed by.
pub(super) fn head(cli: &Cli, name: &str, note: &str) {
    let line = match note {
        "" => format!("== {name}"),
        n => format!("== {name}  {n}"),
    };
    println!("{}", cli.style.paint(report::Paint::Bold, &line));
}

/// Say `e`, a deployment's run's error, as `main` would.
pub(super) fn say(cli: &Cli, e: &anyhow::Error) {
    use std::io::IsTerminal;
    match e.downcast_ref::<Refused>().filter(|r| r.footer) {
        Some(r) => eprintln!("{r}"),
        None => eprint!(
            "{}",
            crate::diag::report(e, cli.style.color && std::io::stderr().is_terminal())
        ),
    }
}

/// Whether the deployment `name` has been applied and not destroyed since.
fn applied(cli: &Cli, name: &str) -> bool {
    let Ok(registry) = crate::stack::registry(&cli.root) else {
        return false;
    };
    let Some(entry) = registry.get(name) else {
        return false;
    };
    let opener = super::open_s3(&cli.root, false);
    let Ok(store) = entry.state.open(&opener) else {
        return true;
    };
    let entries = crate::audit::Log::new(store, None)
        .entries()
        .unwrap_or_default();
    !super::stack::destroyed(&entries)
}

/// The project's plan: each deployment it lists and those they read, in
/// apply order, then the destroy of each removed, printed as one tree
/// (R-200).
fn plan(
    cli: &Cli,
    label: &str,
    matrix: &Matrix,
    order: &[Dependency],
    removed: &[Dependency],
    of: &For,
) -> Result<Outcome> {
    let sites = matrix
        .listed
        .iter()
        .filter_map(|l| Some((l.target(), format!("{label}:{}", l.line()?))))
        .collect();
    Tree {
        cli,
        label: Some(label),
        sites,
    }
    .plan(order, removed, of)
}

/// A plan of several deployments in apply order, printed as one tree
/// (R-200): the project module's (R-114), or a target's and those it
/// reads (R-30). Each is planned against the outputs of those planned
/// before it, as they will be once applied; a deployment whose plan fails
/// stops the chain there: what reads it is not planned.
pub(super) struct Tree<'a> {
    pub(super) cli: &'a Cli,
    /// The project module that lists them, if one does: what a removed
    /// deployment is removed from.
    pub(super) label: Option<&'a str>,
    /// Where each is listed (`project.df:4`), by name.
    pub(super) sites: std::collections::BTreeMap<String, String>,
}

/// One deployment planned in a tree.
struct Planned {
    node: report::deployments::Deployed,
    tally: Option<report::Tally>,
    error: Option<anyhow::Error>,
    /// Its `plan --json` document.
    json: Option<serde_json::Value>,
}

impl Tree<'_> {
    /// Plan `order`, then the destroy of each of `removed`, each a run of
    /// its own (`of`), and print the tree.
    pub(super) fn plan(
        &self,
        order: &[Dependency],
        removed: &[Dependency],
        of: &For,
    ) -> Result<Outcome> {
        let cli = self.cli;
        let Cmd::Plan(p) = &cli.cmd else {
            bail!("internal: a plan");
        };
        let destroy = Cmd::Plan(Plan {
            destroy: true,
            ..p.clone()
        });
        let mut planned_outputs = std::collections::BTreeMap::new();
        // The deployments not planned, and why: their plan failed, or one
        // they read did.
        let mut stopped: std::collections::BTreeMap<String, String> = Default::default();
        let mut planned = Vec::new();
        let runs = order
            .iter()
            .map(|d| (d, cli.cmd.clone(), false))
            .chain(removed.iter().map(|d| (d, destroy.clone(), true)));
        for (d, cmd, gone) in runs {
            let applied = applied(cli, &d.name);
            let kind = match (gone, applied) {
                (true, _) => ActionKind::Delete,
                (false, true) => ActionKind::Update,
                (false, false) => ActionKind::Create,
            };
            let mut node = report::deployments::Deployed {
                kind,
                name: d.full.clone(),
                site: self.site(d),
                state: String::new(),
                body: String::new(),
            };
            if let Some(why) = d.reads.iter().find_map(|r| stopped.get(r)) {
                node.state = format!("not planned: {why}");
                stopped.insert(d.name.clone(), why.clone());
                planned.push(Planned {
                    node,
                    tally: None,
                    error: None,
                    json: None,
                });
                continue;
            }
            let mut dep = of(d, cmd);
            dep.held = Held::new();
            dep.planned = planned_outputs.clone();
            let result = super::run(dep.clone(), None);
            let held = dep.held.take();
            let (state, error) = match (result, &held.tally) {
                (Err(e), _) => {
                    let word = match Outcome::of_error(&e) {
                        Outcome::Refused { .. } => "refused",
                        _ => "failed",
                    };
                    stopped.insert(d.name.clone(), format!("{} {word}", d.full));
                    (word.to_string(), Some(e))
                }
                (Ok(_), None) => ("planned".to_string(), None),
                (Ok(_), Some(t)) => {
                    // Its changes; the policies are the tree's headline's.
                    let rest = report::Tally {
                        policy: Default::default(),
                        ..t.clone()
                    }
                    .text();
                    let rest = rest.strip_prefix("plan: ").unwrap_or(&rest).to_string();
                    let state = match (gone, t.is_quiet()) {
                        (true, true) => self.removed("nothing is left to destroy"),
                        (true, false) => {
                            self.removed(&format!("the next apply destroys it, {rest}"))
                        }
                        (false, _) if !applied => format!("never applied, {rest}"),
                        (false, true) => {
                            node.kind = ActionKind::Noop;
                            "up to date".to_string()
                        }
                        (false, false) => rest,
                    };
                    (state, None)
                }
            };
            // Its plan feeds the plans that read it (R-200): what it will
            // publish once applied (an apply with no change publishes too).
            if let (None, Some(outputs)) = (&error, held.outputs) {
                planned_outputs.insert(
                    d.name.clone(),
                    crate::stack::Planned {
                        full: d.full.clone(),
                        outputs,
                    },
                );
            }
            let mut state = match d.root || gone || self.label.is_none() {
                true => state,
                false => format!("{state}  (not listed: a listed deployment reads it)"),
            };
            // What it is applied after: what it reads (R-30), less what
            // its state says it already waits on (`.. after X is applied`).
            let after: Vec<&str> = order
                .iter()
                .filter(|o| d.reads.contains(&o.name))
                .map(|o| o.full.as_str())
                .filter(|full| !state.contains(&format!("after {full} is applied")))
                .collect();
            if !after.is_empty() && !gone && !matches!(node.kind, ActionKind::Noop) {
                state.push_str(&format!("  after {}", after.join(", ")));
            }
            node.state = state;
            if !matches!(node.kind, ActionKind::Noop) {
                node.body = held.text;
            }
            planned.push(Planned {
                node,
                tally: held.tally,
                error,
                json: held.json,
            });
        }
        let mut tally = report::Tally::default();
        for p in &planned {
            if let Some(t) = &p.tally {
                tally.add(t);
            }
        }
        let nodes: Vec<_> = planned.iter().map(|p| p.node.clone()).collect();
        if p.json {
            return self.print_json(&tally, planned);
        }
        match nodes.is_empty() {
            true => println!(
                "stacks: {} lists no deployment",
                self.label.unwrap_or(crate::project::PROJECT_MODULE)
            ),
            false => print!("{}", report::deployments::render(&tally, &nodes, cli.style)),
        }
        let mut outcome = Outcome::Done;
        for e in planned.into_iter().filter_map(|p| p.error) {
            say(cli, &e);
            // The worst of them: a failure, else the first refusal.
            outcome = match (outcome, Outcome::of_error(&e)) {
                (Outcome::Failed, _) | (_, Outcome::Failed) => Outcome::Failed,
                (Outcome::Done, o) => o,
                (o, _) => o,
            };
        }
        Ok(outcome)
    }

    /// `plan --json` of the tree: its summary, then each deployment's
    /// line and its own document under `plan` (R-200); the outcome the
    /// worst of theirs, each error said on stderr.
    fn print_json(&self, tally: &report::Tally, planned: Vec<Planned>) -> Result<Outcome> {
        let mut deployments = Vec::new();
        let mut outcome = Outcome::Done;
        for p in planned {
            let mut j = serde_json::json!({
                "deployment": p.node.name,
                "mark": report::marker_of(&p.node.kind),
                "site": p.node.site,
                "state": p.node.state,
                "plan": p.json,
            });
            if let Some(e) = &p.error {
                j["error"] = format!("{e:#}").into();
                say(self.cli, e);
                outcome = match (outcome, Outcome::of_error(e)) {
                    (Outcome::Failed, _) | (_, Outcome::Failed) => Outcome::Failed,
                    (Outcome::Done, o) => o,
                    (o, _) => o,
                };
            }
            deployments.push(j);
        }
        let j = serde_json::json!({
            "summary": tally.text(),
            "outcome": outcome.word(),
            "deployments": deployments,
        });
        println!("{}", serde_json::to_string_pretty(&j)?);
        Ok(outcome)
    }

    /// Where `d` is listed, else its stack's file from the project root.
    fn site(&self, d: &Dependency) -> String {
        if let Some(s) = self.sites.get(&d.name) {
            return s.clone();
        }
        let file = std::fs::canonicalize(&d.file).unwrap_or_else(|_| d.file.clone());
        crate::project::manifest_root(&file)
            .and_then(|root| Some(file.strip_prefix(root).ok()?.display().to_string()))
            .unwrap_or_else(|| d.file.display().to_string())
    }

    /// The state of a deployment removed from the project module.
    fn removed(&self, what: &str) -> String {
        let label = self.label.unwrap_or(crate::project::PROJECT_MODULE);
        format!("removed from {label}: {what}")
    }
}

/// The project's apply: each deployment in apply order, planned,
/// confirmed and applied in turn, then the destroy of each removed; the
/// first that does not end done ends the run there.
fn apply(
    cli: &Cli,
    label: &str,
    order: &[Dependency],
    removed: &[Dependency],
    of: &For,
) -> Result<Outcome> {
    let Cmd::Apply(a) = &cli.cmd else {
        bail!("internal: an apply");
    };
    let destroy = Cmd::Apply(Apply {
        plan_file: None,
        allow_empty: Vec::new(),
        destroy: true,
        new_master: false,
        ..a.clone()
    });
    let named: Vec<String> = order
        .iter()
        .map(|d| match d.root {
            true => d.name.clone(),
            false => format!("{} (not listed: a listed deployment reads it)", d.name),
        })
        .collect();
    let mut line = match named.is_empty() {
        true => format!("stacks: {label} lists no deployment"),
        false => format!(
            "stacks: {label}'s deployments, in apply order: {}",
            named.join(", then ")
        ),
    };
    if !removed.is_empty() {
        let gone: Vec<&str> = removed.iter().map(|d| d.name.as_str()).collect();
        line.push_str(&format!(
            "; then, removed from it, destroyed: {}",
            gone.join(", then ")
        ));
    }
    println!("{line}; each is planned, confirmed and applied in turn");
    for d in order {
        head(cli, &d.full, "");
        match super::run(of(d, cli.cmd.clone()), None)? {
            Outcome::Done => {}
            o => return Ok(o),
        }
        if d.root {
            let stack = crate::state::stack_name(&d.file);
            let kept = Kept {
                stack,
                key: d.key.clone(),
            };
            Made::record(&cli.root, label, &d.name, Some(kept))?;
        }
    }
    for d in removed {
        head(cli, &d.full, &format!("removed from {label}"));
        match super::run(of(d, destroy.clone()), None)? {
            Outcome::Done => Made::record(&cli.root, label, &d.name, None)?,
            o => return Ok(o),
        }
    }
    Ok(Outcome::Done)
}

/// The project's test: each deployment it lists, its key pinned.
fn test(cli: &Cli, label: &str, order: &[Dependency], of: &For) -> Result<Outcome> {
    let listed: Vec<&Dependency> = order.iter().filter(|d| d.root).collect();
    match listed.is_empty() {
        true => println!("stacks: {label} lists no deployment"),
        false => println!("stacks: {label}'s deployments, each tested with its key"),
    }
    let mut failed = Vec::new();
    for d in &listed {
        head(cli, &d.full, "");
        if let Err(e) = super::run(of(d, Cmd::Test(Test)), None) {
            say(cli, &e);
            failed.push(d.name.as_str());
        }
    }
    if !failed.is_empty() {
        bail!(
            "test: {} of {label}'s {} deployments failed: {}",
            failed.len(),
            listed.len(),
            failed.join(", ")
        );
    }
    Ok(Outcome::Done)
}

/// The project's `dev effects`: each stack it lists, once, as what a
/// stack reads, writes and offers is its program's, whatever its key.
fn effects(cli: &Cli, label: &str, order: &[Dependency], of: &For) -> Result<Outcome> {
    let mut seen = std::collections::BTreeSet::new();
    let stacks: Vec<&Dependency> = order
        .iter()
        .filter(|d| d.root && seen.insert(d.file.clone()))
        .collect();
    match stacks.is_empty() {
        true => println!("stacks: {label} lists no deployment"),
        false => println!("stacks: the stacks {label} lists, each once"),
    }
    for d in stacks {
        let stack = d.name.split_once('[').map_or(d.name.as_str(), |(s, _)| s);
        head(cli, stack, "");
        super::run(of(d, cli.cmd.clone()), None)?;
    }
    Ok(Outcome::Done)
}

/// The project module's deployments for `stack list`, by name: `true`
/// for one it lists, `false` for one an apply of it made that it lists
/// no more; and the module's name. Empty with no project module (a
/// module that does not load is warned of).
pub(super) fn listed(
    project: &crate::project::Project,
    state_root: &Path,
) -> (std::collections::BTreeMap<String, bool>, String) {
    let mut out = std::collections::BTreeMap::new();
    let label = crate::project::PROJECT_MODULE.to_string();
    let Some(module) = crate::matrix::module_at(&project.root) else {
        return (out, label);
    };
    let found = crate::project::discover(project);
    let listed = Matrix::load(&module).and_then(|m| {
        let roots: Vec<_> = m
            .listed
            .iter()
            .filter_map(|l| match found.named(&l.stack)[..] {
                [one] => Some((one.file.clone(), l.key.clone())),
                _ => None,
            })
            .collect();
        super::order::order_of(&roots)
    });
    match listed {
        Ok(order) => out.extend(order.into_iter().filter(|d| d.root).map(|d| (d.name, true))),
        Err(e) => eprintln!("warning: {label}: {e:#}"),
    }
    if let Ok(made) = Made::load(state_root) {
        for name in made.of(&label).into_keys() {
            out.entry(name).or_insert(false);
        }
    }
    (out, label)
}
