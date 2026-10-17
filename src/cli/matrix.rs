//! `plan`, `apply` and `test` on the project module (R-114): each
//! deployment it lists, and those they read, in dependency order; a
//! deployment the project's apply made that it no longer lists is
//! destroyed. `dev effects`: each stack it lists, once.

use super::apply::Apply;
use super::args::Target;
use super::board::{Board, Upstreams, Verdict};
use super::dev::Effects;
use super::plan::Plan;
use super::test::Test;
use super::{Cli, Cmd, Dependency, Held, HeldPlan, Outcome, Refused};
use crate::matrix::{Kept, Made, Matrix};
use crate::provider::ActionKind;
use crate::{report, zset};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        | Cmd::Render(_)
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
    let label = label_of(module, &project);
    let order = listed_order(&matrix, &label, &found)?;
    let removed = removed_order(&cli, &label, &order, &found)?;
    if !cli.input_files.is_empty() {
        bail!("--input-file gives one deployment's inputs: name it as the target");
    }
    check_sets(&cli, &label, &order, &removed)?;
    let of = |d: &Dependency, cmd: Cmd| deployment_run(&cli, d, cmd);
    match &cli.cmd {
        Cmd::Plan(_) => plan(&cli, &label, &matrix, &order, &removed, &of),
        Cmd::Apply(_) => apply(&cli, &label, &order, &removed, &of),
        Cmd::Test(_) => test(&cli, &label, &order, &of),
        Cmd::Effects(_) => effects(&cli, &label, &order, &of),
        Cmd::Status(_) => super::status::matrix(&cli, &order, &of),
        Cmd::Render(_) => super::render::matrix(&cli, &order, &of),
        _ => bail!("internal: a project module runs plan, apply, test and dev effects"),
    }
}

/// The module as the project names it: its path from the root.
fn label_of(module: &Path, project: &crate::project::Project) -> String {
    std::fs::canonicalize(module)
        .ok()
        .and_then(|m| {
            let root = std::fs::canonicalize(&project.root).ok()?;
            Some(m.strip_prefix(root).ok()?.display().to_string())
        })
        .unwrap_or_else(|| module.display().to_string())
}

/// The file of the project's stack `stack`, when exactly one is so named.
fn stack_file(found: &crate::project::Discovered, stack: &str) -> Option<PathBuf> {
    match found.named(stack)[..] {
        [one] => Some(one.file.clone()),
        _ => None,
    }
}

/// The deployments the module lists and those they read, in apply order.
fn listed_order(
    matrix: &Matrix,
    label: &str,
    found: &crate::project::Discovered,
) -> Result<Vec<Dependency>> {
    let mut roots = Vec::new();
    for l in &matrix.listed {
        let Some(file) = stack_file(found, &l.stack) else {
            bail!(
                "{label}: {} lists a deployment of no stack of the project",
                l.target()
            );
        };
        roots.push((file, l.key.clone()));
    }
    super::order::order_of(&roots)
}

/// What an apply of the module made that it lists no more (not in
/// `order`): destroyed, readers first.
fn removed_order(
    cli: &Cli,
    label: &str,
    order: &[Dependency],
    found: &crate::project::Discovered,
) -> Result<Vec<Dependency>> {
    let mut gone = Vec::new();
    for (name, kept) in Made::load(&cli.root)?.of(label) {
        if order.iter().any(|d| d.name == name) {
            continue;
        }
        let Some(file) = stack_file(found, &kept.stack) else {
            bail!(
                "{name}: {label} lists it no more, and the project has no stack {}: its destroy \
                 needs its program; restore the stack's file, destroy {name}, then remove it",
                kept.stack
            );
        };
        gone.push((name, file, kept.key));
    }
    let roots: Vec<_> = gone
        .iter()
        .map(|(_, f, k)| (f.clone(), k.clone()))
        .collect();
    let mut removed = super::order::order_of(&roots)?;
    removed.retain(|d| gone.iter().any(|(n, _, _)| *n == d.name));
    removed.reverse();
    Ok(removed)
}

/// The input a `--set K=V` names.
fn set_input(kv: &str) -> String {
    kv.split_once('=').map_or(kv, |(k, _)| k).to_string()
}

/// A `--set` goes to each deployment's stack that declares the input;
/// one none of `order` and `removed` declares is an error.
fn check_sets(cli: &Cli, label: &str, order: &[Dependency], removed: &[Dependency]) -> Result<()> {
    let Some(kv) = cli.user_set.iter().find(|kv| {
        !order
            .iter()
            .chain(removed)
            .any(|d| d.inputs.contains(&set_input(kv)))
    }) else {
        return Ok(());
    };
    bail!(
        "--set {kv}: no stack {label} lists declares input {}",
        set_input(kv)
    )
}

/// The run of the deployment `d` the module's `cli` makes, as `cmd`:
/// its file and key, and the `--set`s of the inputs its stack declares.
fn deployment_run(cli: &Cli, d: &Dependency, cmd: Cmd) -> Cli {
    let mut dep = cli.clone();
    dep.cmd = cmd;
    dep.matrix = None;
    dep.user_set = cli
        .user_set
        .iter()
        .filter(|kv| d.inputs.contains(&set_input(kv)))
        .cloned()
        .collect();
    dep.set = dep.user_set.clone();
    dep.set
        .extend(d.key.iter().map(|(k, v)| format!("{k}={v}")));
    dep.keys = d.key.clone();
    dep.files = vec![d.file.clone()];
    dep.world = cli.world.as_deref().map(|w| d.world_beside(w));
    dep
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
    eprint!("{}", Refused::report(e, cli.err_style.color));
}

/// Whether the deployment `d` has been applied and not destroyed since.
fn applied(cli: &Cli, d: &Dependency) -> bool {
    let Ok(registry) = crate::stack::registry(&cli.root) else {
        return false;
    };
    let Some((_, entry)) = crate::stack::registered(&registry, &d.full, &d.name) else {
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

/// A plan of several deployments, printed as one tree (R-200): the
/// project module's (R-114), or a target's and those it reads (R-30).
/// Each is planned on a thread of its own, all at once, and waits only on
/// those it reads (`board`): its evaluation on what their evaluations
/// make known, its report on their plans. A deployment whose plan fails
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
    /// Its part of the tree's plan file (`--out`).
    step: Option<zset::file::Step>,
}

impl Planned {
    /// `node`, not planned: why.
    fn not(mut node: report::deployments::Deployed, why: &str) -> Planned {
        node.state = format!("not planned: {why}");
        Planned {
            node,
            tally: None,
            error: None,
            json: None,
            step: None,
        }
    }
}

/// A deployment of a tree's plan: its run, and whether it is the destroy
/// of one removed from the project module.
struct Member<'d> {
    d: &'d Dependency,
    run: Cli,
    gone: bool,
}

/// A run of a deployment of a tree's plan, as it ended: its outcome, what
/// it held for the tree, and whether the deployment had been applied.
struct Ran {
    result: Result<Outcome>,
    held: HeldPlan,
    applied: bool,
}

/// The stack of a thread a deployment of a tree's plan runs on: the main
/// thread's.
const STACK: usize = 8 << 20;

/// A tree's deployments planned or applied as one run while this lives:
/// each provider started, and its schema asked, once for the tree
/// (`plugin::pool`); each state object read once (`dform_s3::Reads`);
/// each image's tag resolved once (`files::oci::Resolved`).
pub(super) struct OneRun {
    _reads: dform_s3::Remembering,
    _providers: crate::plugin::pool::Shared,
    _digests: crate::files::oci::Resolved,
}

impl OneRun {
    pub(super) fn begin(order: &[Dependency]) -> OneRun {
        OneRun {
            _reads: dform_s3::remember_reads(),
            _providers: crate::plugin::pool::share(super::order::types_of(order)),
            _digests: crate::files::oci::Resolved::for_the_run(),
        }
    }
}

/// The deployments of `members` that member `i` reads: those before it it
/// names.
fn upstreams_of(members: &[Member], i: usize) -> Vec<usize> {
    let reads = &members[i].d.reads;
    (0..i)
        .filter(|&j| reads.contains(&members[j].d.name))
        .collect()
}

/// The full names of the deployments of `members` that `d` reads.
fn reads_of(members: &[Member], d: &Dependency) -> Vec<String> {
    members
        .iter()
        .filter(|m| d.reads.contains(&m.d.name))
        .map(|m| m.d.full.clone())
        .collect()
}

/// The worse of a tree's outcome so far and that of `e`, a deployment's
/// error: a failure, else the first refusal.
fn worst(outcome: Outcome, e: &anyhow::Error) -> Outcome {
    match (outcome, Outcome::of_error(e)) {
        (Outcome::Failed, _) | (_, Outcome::Failed) => Outcome::Failed,
        (Outcome::Done, o) => o,
        (o, _) => o,
    }
}

/// The tree's plan file at `out`: every deployment's plan, once each
/// planned.
fn save_file(out: &Path, steps: Vec<zset::file::Step>) -> Result<()> {
    let digests: Vec<String> = steps
        .iter()
        .map(|s| {
            format!(
                "{} {}",
                s.deployment,
                s.plan.digest.as_deref().unwrap_or_default()
            )
        })
        .collect();
    zset::file::Sequence {
        version: zset::file::VERSION,
        deployments: steps,
    }
    .save(out)?;
    eprintln!(
        "plan file: {} (plan digests: {})",
        out.display(),
        digests.join(", ")
    );
    Ok(())
}

impl Tree<'_> {
    /// Plan `order`, and the destroy of each of `removed`, each a run of
    /// its own (`of`), and print the tree.
    pub(super) fn plan(
        &self,
        order: &[Dependency],
        removed: &[Dependency],
        of: &For,
    ) -> Result<Outcome> {
        let Cmd::Plan(p) = &self.cli.cmd else {
            bail!("internal: a plan");
        };
        let destroy = Cmd::Plan(Plan {
            destroy: true,
            ..p.clone()
        });
        let _run = OneRun::begin(order);
        let members: Vec<Member> = order
            .iter()
            .map(|d| (d, self.cli.cmd.clone(), false))
            .chain(removed.iter().map(|d| (d, destroy.clone(), true)))
            .map(|(d, cmd, gone)| Member {
                d,
                run: of(d, cmd),
                gone,
            })
            .collect();
        let mut planned = self.plan_all(&members).0;
        let mut tally = report::Tally::default();
        for p in &planned {
            if let Some(t) = &p.tally {
                tally.add(t);
            }
        }
        if let Some(out) = &p.out
            && planned.iter().all(|p| p.error.is_none())
        {
            let steps: Vec<_> = planned.iter_mut().filter_map(|p| p.step.take()).collect();
            if steps.len() == order.len() {
                save_file(out, steps)?;
            }
        }
        if p.json {
            return self.print_json(&tally, planned);
        }
        self.print_text(&tally, planned)
    }

    /// What the deployments of `order` will publish as their plans say,
    /// each planned (unprinted) over those it reads: what a run of a
    /// deployment that reads them reads (`why`, `query`). One whose plan
    /// fails is left out: its reader reads what it published.
    pub(super) fn outputs(
        &self,
        order: &[Dependency],
        of: &For,
    ) -> std::collections::BTreeMap<String, crate::stack::Planned> {
        let plan = Cmd::Plan(Plan {
            out: None,
            json: false,
            destroy: false,
            why: report::Why::Line,
            new_master: false,
        });
        let members: Vec<Member> = order
            .iter()
            .map(|d| Member {
                d,
                run: of(d, plan.clone()),
                gone: false,
            })
            .collect();
        self.plan_all(&members).1.outputs()
    }

    /// Each of `members` planned on a thread of its own, all at once, in
    /// the tree's run (its providers, `plugin::pool`): what each planned,
    /// in their order, and the board they read each other on.
    fn plan_all(&self, members: &[Member]) -> (Vec<Planned>, Arc<Board>) {
        let board = Board::new(
            members.iter().map(|m| m.d.name.clone()).collect(),
            members.iter().map(|m| m.d.full.clone()).collect(),
        );
        let pool = crate::plugin::pool::current();
        let within = tracing::Span::current();
        let planned = std::thread::scope(|s| {
            let threads: Vec<_> = (0..members.len())
                .map(|i| {
                    let (board, pool, within) = (&board, pool.clone(), within.clone());
                    std::thread::Builder::new()
                        .name(format!("plan {}", members[i].d.full))
                        // As deep as the main thread's: an evaluation
                        // recurses there as here.
                        .stack_size(STACK)
                        .spawn_scoped(s, move || {
                            let _unwinding = board.unwinding(i);
                            let _pool = pool.map(crate::plugin::pool::join);
                            within.in_scope(|| self.plan_one(board, members, i))
                        })
                        .expect("a thread for a deployment's plan")
                })
                .collect();
            threads
                .into_iter()
                .map(|t| t.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
                .collect()
        });
        (planned, board)
    }

    /// Member `i` of `members` planned, and posted on `board` for those
    /// that read it: again while what its evaluation read of those it
    /// reads is not what their plans make known; not when one of them is
    /// not planned.
    fn plan_one(&self, board: &Arc<Board>, members: &[Member], i: usize) -> Planned {
        let m = &members[i];
        let applied = applied(self.cli, m.d);
        let of = upstreams_of(members, i);
        loop {
            let up = Upstreams::new(board.clone(), i, of.clone());
            let mut dep = m.run.clone();
            dep.held = Held::new();
            dep.upstreams = Some(up.clone());
            let result = run_deployment(dep.clone(), &m.d.full);
            match up.verdict() {
                Verdict::Stands => {
                    let ran = Ran {
                        result,
                        held: dep.held.take(),
                        applied,
                    };
                    return self.gathered(board, members, i, ran);
                }
                Verdict::Again => continue,
                Verdict::NotPlanned(why) => {
                    board.stopped(i, why.clone());
                    return Planned::not(self.node(m.d, m.gone, applied), &why);
                }
            }
        }
    }

    /// What the run of member `i` of `members` planned (`ran`), its
    /// outputs posted on `board` for the plans that read it (R-200): what
    /// it will publish once applied (an apply with no change publishes
    /// too); else why it was not.
    fn gathered(&self, board: &Board, members: &[Member], i: usize, ran: Ran) -> Planned {
        let Member { d, gone, .. } = &members[i];
        let Ran {
            result,
            mut held,
            applied,
        } = ran;
        let mut node = self.node(d, *gone, applied);
        // Its plan file's part (`--out`): after what it reads.
        let step = held.file.take().map(|plan| zset::file::Step {
            deployment: d.full.clone(),
            after: reads_of(members, d),
            plan,
        });
        let (state, error) = match result {
            Err(e) => {
                let word = match Outcome::of_error(&e) {
                    Outcome::Refused { .. } => "refused",
                    _ => "failed",
                };
                board.stopped(i, format!("{} {word}", d.full));
                (word.to_string(), Some(e))
            }
            Ok(_) => {
                board.planned(i, held.outputs.take());
                let state = self.state_of(&mut node, held.tally.as_ref(), *gone, applied);
                (state, None)
            }
        };
        node.state = self.listed(state, d, *gone);
        if !matches!(node.kind, ActionKind::Noop) {
            node.body = held.text;
        }
        Planned {
            node,
            tally: held.tally,
            error,
            json: held.json,
            step,
        }
    }

    /// The tree's node of `d`, its state not said yet: its destroy
    /// (`gone`), else its first apply or another.
    fn node(&self, d: &Dependency, gone: bool, applied: bool) -> report::deployments::Deployed {
        let kind = match (gone, applied) {
            (true, _) => ActionKind::Delete,
            (false, true) => ActionKind::Update,
            (false, false) => ActionKind::Create,
        };
        report::deployments::Deployed {
            kind,
            name: d.full.clone(),
            site: self.site(d),
            state: String::new(),
            body: String::new(),
        }
    }

    /// The state line of a deployment planned (`tally`: its headline's
    /// counts, `None` when it made no plan): its changes, `up to date`
    /// (`node` then marked unchanged), or its destroy's (`gone`).
    fn state_of(
        &self,
        node: &mut report::deployments::Deployed,
        tally: Option<&report::Tally>,
        gone: bool,
        applied: bool,
    ) -> String {
        let Some(t) = tally else {
            return "planned".to_string();
        };
        // Its changes; the policies are the tree's headline's.
        let rest = report::Tally {
            policy: Default::default(),
            ..t.clone()
        }
        .text();
        let rest = rest.strip_prefix("plan: ").unwrap_or(&rest).to_string();
        match (gone, t.is_quiet()) {
            (true, true) => self.removed("nothing is left to destroy"),
            (true, false) => self.removed(&format!("the next apply destroys it, {rest}")),
            (false, _) if !applied => format!("never applied, {rest}"),
            (false, true) => {
                node.kind = ActionKind::Noop;
                "up to date".to_string()
            }
            (false, false) => rest,
        }
    }

    /// `state`, with, in a project module's tree, why a deployment it
    /// does not list is in it. What it reads orders nothing of it: its
    /// changes are in the tree's ticks.
    fn listed(&self, state: String, d: &Dependency, gone: bool) -> String {
        match d.root || gone || self.label.is_none() {
            true => state,
            false => format!("{state}  (not listed: a listed deployment reads it)"),
        }
    }

    /// The tree as text: its headline and each deployment's line and
    /// plan; the outcome the worst of theirs, each error said on stderr.
    fn print_text(&self, tally: &report::Tally, planned: Vec<Planned>) -> Result<Outcome> {
        let cli = self.cli;
        let nodes: Vec<_> = planned.iter().map(|p| p.node.clone()).collect();
        match nodes.is_empty() {
            true => println!(
                "stacks: {} lists no deployment",
                self.label.unwrap_or(crate::project::PROJECT_MODULE)
            ),
            false => print!("{}", report::deployments::render(tally, &nodes, cli.style)),
        }
        let mut outcome = Outcome::Done;
        for e in planned.into_iter().filter_map(|p| p.error) {
            say(cli, &e);
            outcome = worst(outcome, &e);
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
                outcome = worst(outcome, e);
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
            "stacks: {label}'s deployments, {}: one apply, its ticks the tree's",
            named.join(", ")
        ),
    };
    if !removed.is_empty() {
        let gone: Vec<&str> = removed.iter().map(|d| d.name.as_str()).collect();
        line.push_str(&format!(
            "; then, removed from it, destroyed: {}",
            gone.join(", then ")
        ));
    }
    println!("{line}");
    if !order.is_empty() {
        match super::apply::tree(cli, order, &|d| of(d, cli.cmd.clone()))? {
            Outcome::Done => {}
            o => return Ok(o),
        }
    }
    for d in order.iter().filter(|d| d.root) {
        let stack = crate::state::stack_name(&d.file);
        let kept = Kept {
            stack,
            key: d.key.clone(),
        };
        Made::record(&cli.root, label, &d.name, Some(kept))?;
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

/// The run `dep` of the tree's deployment `full`: a step of the tree's
/// run.
#[tracing::instrument(
    target = "dform",
    level = "trace",
    name = "plan.phase",
    skip_all,
    fields(otel.name = %format_args!("deployment {full}"))
)]
fn run_deployment(dep: Cli, full: &str) -> Result<Outcome> {
    super::run(dep, None)
}
