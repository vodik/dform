//! The steps of a run of a deployment, each a command's start: the
//! program loaded ([`Cli::load`]), the deployment located ([`Objects`]),
//! its master read and the deployment evaluated ([`Evaluated`]).

use super::cmd::{Cmd, Stage};
use super::outputs::open_sealed;
use super::run_inputs::{check_keys, env_inputs, plan_inputs, secret_inputs_of, split_kv};
use super::secrets::{born, written};
use super::stack::Rekeying;
use super::{Cli, Outcome, Session, launch, open_s3};
use crate::ast::Atom;
use crate::chaos::Chaos;
use crate::plugin::Providers;
use crate::{controller, deployment, report, state, store, watch, zset};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What the command line does at an evaluation's steps
/// (`deployment::Observer`): it prints what the evaluation says, and
/// controller mode's hook stamps the inputs and tables and adds the drift.
pub(super) struct Watch<'a> {
    hook: Option<&'a mut controller::Hook>,
    cmd: &'a Cmd,
}

impl<'a> Watch<'a> {
    pub(super) fn new(hook: Option<&'a mut controller::Hook>, cmd: &'a Cmd) -> Watch<'a> {
        Watch { hook, cmd }
    }
}

impl deployment::Observer for Watch<'_> {
    fn note(&mut self, note: deployment::Note) {
        use deployment::Note;
        let plans = self.cmd.plans();
        match note {
            Note::Warning(w) | Note::Policy(w) => eprintln!("warning: {w}"),
            Note::Collision(w) if plans => eprintln!("warning: {w}"),
            Note::Collision(_) => {}
            Note::Resolved(line) => eprintln!("resolved: {line}"),
            Note::TableMoved(m) if plans => match self.hook {
                Some(_) => controller::log(format_args!("{m}")),
                None => println!("{m}"),
            },
            Note::TableMoved(_) => {}
            Note::Computed(_, n) if plans => eprintln!("note: {n}"),
            Note::Computed(..) => {}
        }
    }

    fn relations(&mut self, relations: &[watch::Relation]) {
        if let Some(h) = self.hook.as_deref_mut() {
            h.inputs(relations);
        }
    }

    fn tables(&mut self, read: &[(watch::Relation, String)]) {
        if let Some(h) = self.hook.as_deref_mut() {
            h.tables(read);
        }
    }

    fn facts(&mut self, backend: &Providers, st: &state::State) -> Result<Vec<Atom>> {
        Ok(match self.hook.as_deref_mut() {
            Some(h) => h.drift_facts(&backend.stored_world(&backend.observe(st)?)),
            None => Vec::new(),
        })
    }
}

impl Cli {
    /// The run's program, as `deployment::load` reads it: the input files'
    /// facts added; `stack` statements and providers' `use`s over the
    /// manifest's defaults, `--provider` over the latter. A key's value is
    /// the target's, else its input's default.
    pub(super) fn load(
        &mut self,
        hook: Option<&mut controller::Hook>,
    ) -> Result<deployment::Loaded> {
        if self.files.is_empty() {
            bail!("internal: a run with no program");
        }
        let target = deployment::Target {
            files: self.files.clone(),
            input_files: self.input_files.clone(),
            providers: self.providers.clone(),
        };
        let loaded = crate::timing::time(
            || "loaded and compiled".into(),
            || {
                deployment::load(
                    &target,
                    env!("CARGO_PKG_VERSION"),
                    &|p: &Path| std::fs::read_to_string(p),
                    &mut Watch::new(hook, &self.cmd),
                )
            },
        )?;
        self.manifest = loaded.manifest.clone();
        check_keys(self, &loaded.cfg, &loaded.stack)?;
        Ok(loaded)
    }
}

/// A run of a deployment, located: its objects (its state, its world)
/// and its audit log, beside its state.
pub(super) struct Objects {
    pub(super) cli: Cli,
    pub(super) located: deployment::Located,
    pub(super) audit: crate::audit::Log,
}

impl Objects {
    /// The deployment `cli` is of: the stack, or one value of its key
    /// (rekey's old one, `rekey`), and where its objects are. A keyed
    /// stack's plan and apply say first which deployment they are of; `-v`
    /// adds which of its key values are defaults.
    pub(super) fn locate(
        cli: Cli,
        loaded: deployment::Loaded,
        rekey: Option<&Rekeying>,
        mut hook: Option<&mut controller::Hook>,
    ) -> Result<Objects> {
        let root = cli.root.clone();
        let set = cli
            .set
            .iter()
            .map(|kv| split_kv(kv).map(|(k, v)| (k.to_string(), v)))
            .collect::<Result<Vec<_>>>()?;
        let writes = hook.is_some() || cli.cmd.writes_objects();
        let locating = crate::timing::span(|| "located the deployment".into());
        let located = loaded.locate(
            &deployment::Selection {
                root: root.clone(),
                set,
                instance: rekey.map(|r| r.from.clone()),
                world: cli.world.clone(),
                inventory: cli.inventory.clone(),
                objects_only: cli.cmd.objects_only(),
            },
            &open_s3(&root, writes),
            &mut Watch::new(hook.as_deref_mut(), &cli.cmd),
        )?;
        drop(locating);
        let text_plan = matches!(&cli.cmd, Cmd::Plan(p) if !p.json);
        if !located.instance.key.is_empty()
            && hook.is_none()
            && (text_plan || matches!(cli.cmd, Cmd::Apply(_)))
        {
            cli.held.print(&match cli.cmd.why() >= report::Why::How {
                true => format!("deployment: {}\n", located.instance.describe()),
                false => format!("deployment: {}\n", located.instance.name()),
            });
        }
        let manifest = located.loaded.manifest.as_ref();
        let audit = located.dep.audit(
            cli.audit_sink
                .clone()
                .or(located.loaded.cfg.audit_sink.clone()),
            manifest.map_or(crate::audit::SINK_TIMEOUT, |m| m.audit_sink_timeout()),
            manifest.is_some_and(|m| m.audit_sink_all()),
        );
        Ok(Objects {
            cli,
            located,
            audit,
        })
    }

    /// The deployment, as the user writes it.
    pub(super) fn deployment(&self) -> &str {
        &self.located.deployment
    }

    /// The deployment's state, lock and audit log.
    pub(super) fn dep(&self) -> &store::Deployment {
        &self.located.dep
    }
}

/// The audit log as the run began, read once: the guardrail and why since
/// the last apply both read it.
pub(super) struct Entries {
    log: crate::audit::Log,
    read: std::cell::OnceCell<Option<Vec<serde_json::Value>>>,
}

impl Entries {
    fn new(log: crate::audit::Log) -> Entries {
        Entries {
            log,
            read: std::cell::OnceCell::new(),
        }
    }

    /// The entries, `None` when the log cannot be read.
    pub(super) fn get(&self) -> Option<&Vec<serde_json::Value>> {
        self.read.get_or_init(|| self.log.entries().ok()).as_ref()
    }
}

/// What a run of a deployment holds once it is located and its master
/// read: what plan, apply and the commands that explain share.
pub(super) struct Context {
    pub(super) cli: Cli,
    /// The state root.
    pub(super) root: PathBuf,
    /// The deployment, as the user writes it.
    pub(super) deployment: String,
    pub(super) dep: store::Deployment,
    pub(super) audit: crate::audit::Log,
    pub(super) entries: Entries,
    /// `apply PLAN`: the plan file, and where it was read.
    pub(super) saved: Option<(PathBuf, zset::file::PlanFile)>,
    /// A plan that writes a file, or an apply: the run digests secrets
    /// with the master.
    pub(super) writes: bool,
    pub(super) mixing: crate::custody::Mixing,
    pub(super) master: crate::custody::Master,
    /// The key the run's digests are keyed with: a plan that writes a
    /// file, and an apply.
    pub(super) key: Option<zset::file::Key>,
    /// What the plan file records, when one is written or read.
    pub(super) inputs: Option<zset::file::Inputs>,
    /// The other stacks' outputs read, by their digests.
    pub(super) outputs_read: Vec<zset::file::OutputsDigest>,
    /// The program's secret inputs.
    pub(super) secret_inputs: BTreeSet<String>,
    /// The secret outputs sealed to this deployment that were opened
    /// (R-166).
    pub(super) opened: Vec<String>,
    pub(super) chaos: Chaos,
    /// What providers and trust roots fetch is cached here.
    pub(super) cache: PathBuf,
    /// What the last apply derived (R-80).
    pub(super) last_derived: Option<zset::Derived>,
    /// `stack rekey`: the deployment whose state moves.
    pub(super) rekey: Option<Rekeying>,
}

impl Context {
    /// The key a plan file's digests are keyed with: none for a plan file
    /// written without the master (R-164), which is checked in its own form.
    pub(super) fn file_key(&self) -> Option<&zset::file::Key> {
        match &self.saved {
            Some((_, f)) if f.unkeyed => None,
            _ => self.key.as_ref(),
        }
    }

    /// The run's inputs as a plan file keyed with `key` records them.
    pub(super) fn plan_inputs(&self, key: Option<&zset::file::Key>) -> Result<zset::file::Inputs> {
        Ok(zset::file::Inputs {
            stack_outputs: self.outputs_read.clone(),
            ..plan_inputs(&self.cli, &self.cli.files, &self.secret_inputs, key)?
        })
    }
}

/// A run of a deployment, evaluated: its context, the evaluation, and
/// controller mode's hook.
pub(super) struct Evaluated<'h> {
    pub(super) cx: Context,
    pub(super) ev: deployment::Evaluation,
    pub(super) hook: Option<&'h mut controller::Hook>,
}

impl<'h> Evaluated<'h> {
    /// The deployment of `objects` evaluated: its master read (R-163,
    /// R-164), the outputs it reads opened, the plan file it applies
    /// checked against this run's inputs, and the program evaluated. A run
    /// blocked by the program's violations stops here.
    pub(super) fn new(
        objects: Objects,
        saved: Option<(PathBuf, zset::file::PlanFile)>,
        rekey: Option<Rekeying>,
        mut hook: Option<&'h mut controller::Hook>,
    ) -> Result<Evaluated<'h>> {
        let Objects {
            cli,
            located,
            audit,
        } = objects;
        let mut cx = Context::new(cli, &located, audit, saved, rekey)?;
        let read_outputs = cx.read_outputs(&located)?;
        if cx.writes {
            cx.inputs = Some(cx.plan_inputs(cx.file_key())?);
        }
        cx.check_stale()?;
        cx.open_hook(&located, hook.as_deref_mut())?;
        if let Cmd::Apply(a) = &cx.cli.cmd {
            cx.chaos = Chaos::parse(&a.chaos)?;
        }
        if cx.cli.cmd.reads_last_apply() {
            cx.last_derived = cx.entries.get().and_then(|es| zset::Derived::last(es));
        }
        let opts = cx.options(hook.is_some())?;
        // A key never rotated is as old as its master (R-161, `secrets/4`).
        crate::functions::random::set_born(born(cx.entries.get()));
        let ev = located.evaluate(
            read_outputs,
            &opts,
            &mut Watch::new(hook.as_deref_mut(), &cx.cli.cmd),
        )?;
        // A destroy is refused by the denies over its plan, not by the
        // program's own: it wants none of the resources they are about.
        if opts.blocking && !cx.cli.cmd.destroys() {
            cx.cli.cmd.blocked(&ev.violations, &ev.redact)?;
        }
        Ok(Evaluated { cx, ev, hook })
    }

    /// The resources the evaluation compiled: an error for a run that
    /// needs them when a violation blocked their compilation.
    fn compiled(&mut self) -> Result<deployment::Compiled> {
        std::mem::replace(
            &mut self.ev.compiled,
            Err(anyhow::anyhow!("internal: the resources are taken once")),
        )
    }

    /// Run the command of [`Stage::Evaluated`].
    pub(super) fn run(mut self, session: &mut Option<Session>) -> Result<Outcome> {
        debug_assert_eq!(self.cx.cli.cmd.stage(), Stage::Evaluated);
        if self.cx.cli.cmd.explains() {
            return super::explain::run(&mut self);
        }
        let compiled = self.compiled()?;
        match self.cx.cli.cmd.clone() {
            Cmd::Eval(c) => c.run(&self, &compiled),
            Cmd::Show(c) => c.run(&self, &compiled),
            Cmd::Graph(c) => c.run(&self, &compiled),
            Cmd::Rekey(c) => c.run(&self),
            Cmd::Plan(c) => c.run(self),
            Cmd::Apply(c) => c.run(self, compiled, session),
            c => unreachable!("{c:?} runs at another stage"),
        }
    }
}

/// The deployment's master (R-163): its key file read, one made only for a
/// deployment with no state, by a run that writes (a plan file, an apply)
/// or derives (`random.*`); a plan that writes a file, and an apply, digest
/// secrets with it (the plan file's, the audit log's).
fn master(
    cli: &Cli,
    located: &deployment::Located,
    mixing: &crate::custody::Mixing,
    writes: bool,
) -> Result<crate::custody::Master> {
    if !cli.cmd.holds_master() {
        return Ok(crate::custody::Master::none());
    }
    let derives = located
        .loaded
        .lowered
        .as_ref()
        .is_some_and(|l| crate::functions::random::called(&l.program));
    let new_master = cli.cmd.new_master();
    // A given secret sealed to the master's own key makes one (R-108).
    let gives = matches!(
        &cli.cmd,
        Cmd::Secrets(super::secrets::Secrets::Set { remove: false, .. })
    ) && crate::custody::given::to_master(mixing);
    // The key may be made now: a bucket is checked first, as for any run
    // that writes.
    if let (true, None, store::Location::S3(spec)) = (
        writes || derives || gives || new_master,
        &cli.world,
        &located.location,
    ) {
        open_s3(&cli.root, true)(spec)?;
    }
    let mut m = located.dep.master(
        mixing,
        crate::custody::Want {
            make: writes || derives || gives,
            new_master,
        },
    )?;
    // A query, a why or `secrets` only reads: it says what this master
    // derives, whatever state was applied with.
    m.accept |= cli.cmd.only_reads_secrets();
    Ok(m)
}

/// What a run says of its master: a new one taken (`--new-master`);
/// dform.toml's `[secrets]` saying otherwise than `state.master`, so the
/// next apply that holds the master seals it again (After R-164); a run
/// without it (R-164), which plans in full, each change that needs it
/// marked, and an apply makes what needs it not.
fn say_master(
    cli: &Cli,
    located: &deployment::Located,
    mixing: &crate::custody::Mixing,
    master: &crate::custody::Master,
) {
    let deployment = &located.deployment;
    if cli.cmd.new_master()
        && let Some(id) = &master.id
    {
        eprintln!(
            "new master (--new-master): random.* derive from {} (id {}); every value derived \
             from another master changes",
            master.source,
            crate::report::short_id(id)
        );
    }
    if let (Some(r), Some(_), true) = (&master.reseal, &master.key, cli.cmd.plans())
        && (!r.added.is_empty() || !r.removed.is_empty() || r.passphrase.is_some())
    {
        let when = match cli.cmd {
            Cmd::Apply(_) => "this apply",
            _ => "the next apply",
        };
        let place = match &located.location {
            store::Location::S3(_) => "in the bucket",
            store::Location::Local(_) => "beside its state",
        };
        // Once per run, however often the deployment is opened in it.
        static SAID: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
        if SAID
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(deployment.to_string())
        {
            eprintln!(
                "{deployment}: {}{}  (dform.toml [secrets])",
                r.describe(mixing, when, place),
                match r.removed.is_empty() {
                    true => "",
                    false => {
                        "; sealing to a recipient no longer revokes what it opened before: \
                         `dform secrets cycle` makes a master it never held, and each secret \
                         moves to it as it is rotated"
                    }
                }
            );
        }
    }
    if let Some(why) = &master.without {
        eprintln!(
            "{deployment}: planned without its master ({why}): a secret it derives is a \
             stand-in, proven unchanged or marked `needs the key`"
        );
    }
}

/// A secret input's value inline in argv (R-108): said, not refused, as CI
/// passes a masked variable so.
fn warn_secret_set(cli: &Cli, located: &deployment::Located) {
    let secret_named = secret_inputs_of(&located.loaded.declared);
    for (k, v) in cli.user_set.iter().filter_map(|kv| kv.split_once('=')) {
        if !v.starts_with('@') && secret_named.iter().any(|(a, _)| a == k) {
            eprintln!(
                "warning: --set {k}: argv is readable by every user on this host through /proc \
                 and lands in shell history; use --set {k}=@FILE or `dform secrets set {} {k}`",
                written(&located.instance)
            );
        }
    }
}

impl Context {
    /// What the run of the deployment `located` holds before it reads
    /// anything else: its master (R-163, R-164) and the key its digests are
    /// keyed with, checked against the plan file it applies. A plan file is
    /// checked in its own form: one written without the master by digests
    /// anyone can compute (R-164). After a cycle the key is the first
    /// epoch's master, carried (R-165).
    fn new(
        cli: Cli,
        located: &deployment::Located,
        audit: crate::audit::Log,
        saved: Option<(PathBuf, zset::file::PlanFile)>,
        rekey: Option<Rekeying>,
    ) -> Result<Context> {
        let deployment = located.deployment.clone();
        let writes = saved.is_some()
            || matches!(&cli.cmd, Cmd::Plan(p) if p.out.is_some())
            || matches!(cli.cmd, Cmd::Apply(_));
        // Who holds it: dform.toml's `[secrets]` (R-164).
        let mixing =
            crate::custody::Mixing::of(located.loaded.manifest.as_ref(), &located.instance.stack)?;
        let master = master(&cli, located, &mixing, writes)?;
        say_master(&cli, located, &mixing, &master);
        let key = match writes {
            true => master.digest.clone(),
            false => None,
        };
        if let (Some((path, f)), None) = (&saved, &key)
            && !f.unkeyed
        {
            bail!(
                "plan file {}: its digests are keyed with {deployment}'s master, which this run \
                 does not hold ({}): apply it with the passphrase, or plan again without it",
                path.display(),
                master.without.as_deref().unwrap_or("no master")
            );
        }
        let secret_inputs = located
            .loaded
            .declared
            .iter()
            .filter(|d| d.scope.is_empty())
            .filter(|d| matches!(&d.decl.ty, crate::ast::TypeExpr::Apply(n, _) if n == "secret"))
            .map(|d| d.decl.name.clone())
            .collect();
        warn_secret_set(&cli, located);
        let root = cli.root.clone();
        // What providers and trust roots fetch is cached in the state root's
        // cache/, a world fixture's beside it.
        let cache = match &cli.world {
            Some(w) => w.parent().unwrap_or(Path::new("")).to_path_buf(),
            None => root.join("cache"),
        };
        Ok(Context {
            dep: located.dep.clone(),
            entries: Entries::new(audit.clone()),
            audit,
            deployment,
            saved,
            writes,
            mixing,
            master,
            key,
            inputs: None,
            outputs_read: Vec::new(),
            secret_inputs,
            opened: Vec::new(),
            chaos: Chaos::default(),
            cache,
            last_derived: None,
            rekey,
            root,
            cli,
        })
    }

    /// Other stacks' published outputs, read once, as facts; a plan file
    /// records their digests, and an apply of it refuses when one moved. A
    /// secret output sealed to this deployment (R-166) is opened with its
    /// master, its value read as a secret input's is; the apply's log says
    /// which. A reader the producer does not seal to yet is one from its
    /// apply: registered, its master's public key published (made above),
    /// so the producer's next apply seals to it.
    fn read_outputs(&mut self, located: &deployment::Located) -> Result<Vec<crate::stack::Read>> {
        let mut read = located.read_outputs(&open_s3(&self.root, false))?;
        let (opened, unsealed) = open_sealed(&mut read, &self.deployment, &self.master);
        if let (Cmd::Apply(_), false, None) = (&self.cli.cmd, unsealed.is_empty(), &self.cli.world)
        {
            crate::stack::register(
                &self.root,
                &self.deployment,
                &located.location,
                located.loaded.cfg.bootstrap,
            )?;
        }
        self.opened = opened;
        self.outputs_read = read
            .iter()
            .map(|r| zset::file::OutputsDigest {
                deployment: r.name.clone(),
                digest: r.digest.clone(),
            })
            .collect();
        Ok(read)
    }

    /// `apply PLAN`: the plan file's inputs are this run's, the environment
    /// variables it read as they are now; else it is stale.
    fn check_stale(&self) -> Result<()> {
        let (Some((path, saved)), Some(inputs)) = (&self.saved, &self.inputs) else {
            return Ok(());
        };
        let labels = saved
            .inputs
            .env
            .iter()
            .filter_map(|e| e.get("sensitive")?.as_str().map(str::to_string));
        let now = zset::file::Inputs {
            env: env_inputs(labels, self.file_key()),
            ..inputs.clone()
        };
        let diff = saved.input_differences(&now);
        if diff.is_empty() {
            return Ok(());
        }
        eprintln!(
            "plan file {} is stale: its inputs are not this run's:",
            path.display()
        );
        for d in &diff {
            eprintln!("- {d}");
        }
        bail!("stale plan: run plan again");
    }

    /// Controller mode's run opens its hook on the deployment; a batch
    /// apply of a deployment handed over to the controller is refused.
    fn open_hook(
        &self,
        located: &deployment::Located,
        hook: Option<&mut controller::Hook>,
    ) -> Result<()> {
        let deployment = &self.deployment;
        if let Some(h) = hook {
            if located.loaded.cfg.bootstrap {
                bail!(
                    "stack {deployment} is role = bootstrap: it stays batch, and the controller never runs it"
                );
            }
            h.audit = Some(self.audit.clone());
            h.open(
                &self.dep,
                &located.paths.world,
                self.root.parent().unwrap_or(Path::new("")),
            )?;
        } else if let (Cmd::Apply(_), Some((to, _))) = (&self.cli.cmd, &located.handed) {
            bail!(
                "stack {deployment} was handed over to {to}: the controller runs it \
                 (`dform controller run {deployment}`), not a batch apply"
            );
        }
        Ok(())
    }

    /// How the deployment is evaluated for this run's command.
    fn options(&self, controlled: bool) -> Result<deployment::Options<'static>> {
        let cmd = &self.cli.cmd;
        Ok(deployment::Options {
            launch: launch(),
            data: super::run_inputs::build_extra_facts(&self.cli.data)?,
            chaos: match cmd {
                Cmd::Apply(a) => a.chaos.clone(),
                _ => Vec::new(),
            },
            cache: self.cli.world.is_none().then(|| self.cache.clone()),
            // A plan that makes no key digests with the one there is.
            digest_key: self
                .master
                .digest
                .as_ref()
                .map(|k| k.derive("provider digest").to_hex()),
            master: self.master.clone(),
            recorded: self
                .saved
                .as_ref()
                .map(|(_, f)| f.externs.clone())
                .unwrap_or_default(),
            // What a run plans, its providers declare.
            check_types: cmd.plans() || controlled,
            discover_all: cmd.explains(),
            whole_schema: cmd.pattern().is_some_and(super::explain::reads_schema),
            collisions: matches!(
                cmd,
                Cmd::Plan(_) | Cmd::Apply(_) | Cmd::Query(_) | Cmd::Why(_)
            ),
            blocking: cmd.blocking(),
            policy: cmd.policy(),
            last_apply: self
                .last_derived
                .as_ref()
                .map(zset::Derived::facts)
                .unwrap_or_default(),
            destroy: cmd.destroys(),
        })
    }
}
