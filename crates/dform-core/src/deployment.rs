//! One evaluation of one deployment: the program loaded (through a reader,
//! an editor's unsaved buffers), its deployment resolved and located, the
//! providers it names started (through [`Launch`]) and configured, the
//! schema asked for, the program evaluated over the world and state (the
//! extern rounds, `moved/3`), the plan made and the policy pass run over
//! it (E §2.8), the re-evaluation without the replaced objects included.
//! Read only: state and the world are read through the deployment's
//! [`store::Deployment`], and nothing is written or applied.
//!
//! `dform plan`, `apply`, the controller (an apply with a
//! `controller::Hook`), `query` and `why` run this, and so does the
//! language server; an apply does its writes and ticks on top, through
//! the [`Evaluator`] the evaluation hands back. The steps are also public
//! for the commands that stop between them: [`load`] (`dform test`,
//! `strata`), [`Loaded::locate`] (`state show`, `log`, ...).

use crate::ast::{Atom, Lit, Program, Span, Stmt, Term};
use crate::circuit::{Leaf, NodeId, View};
use crate::diag::{Diagnostic, Diagnostics, Fix};
use crate::engine::{self, EvalResult};
use crate::externs::{self, Externs};
use crate::inputs::{self, Declared};
use crate::ir::{self, Address};
use crate::plugin::providers::ProviderWait;
use crate::plugin::{self, Launch, Providers};
use crate::project::{self, Manifest};
use crate::query::Redactor;
use crate::schema::{self, Schema};
use crate::stack::{self, Instance};
use crate::state::{self, State};
use crate::store::{self, Location, OpenS3};
use crate::value::Value;
use crate::watch::Relation;
use crate::{executor, lint, loader, provider, report, stuck, tables, transform, zset};
use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What a program's files are read through: an editor's buffer when one
/// is open, else the disk.
pub type Reader<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;

/// What an evaluation says as it goes, besides its result: the command
/// line prints each where it prints it, the editor publishes some.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A warning: the lint's.
    Warning(String),
    /// A `warn` fact of the program's own evaluation, redacted.
    Policy(String),
    /// The collision lint of a keyed stack.
    Collision(String),
    /// An Apply call whose answer was lost, resolved.
    Resolved(String),
    /// A git table whose ref names another commit than at the last apply.
    TableMoved(String),
    /// A content read of a computed path in a block, at the read: the
    /// block waits a tick (DESIGN.org R-4).
    Computed(crate::ast::Span, String),
}

/// What a caller does at an evaluation's steps. Each defaults to nothing.
pub trait Observer {
    fn note(&mut self, _note: Note) {}
    /// The program's files, the sources the controller watches.
    fn relations(&mut self, _relations: &[Relation]) {}
    /// The tables read, with their sources as read.
    fn tables(&mut self, _read: &[(Relation, String)]) {}
    /// Facts to evaluate with besides the world's (the controller's
    /// drift), once the providers are configured.
    fn facts(&mut self, _backend: &Providers, _st: &State) -> Result<Vec<Atom>> {
        Ok(Vec::new())
    }
}

/// An observer that keeps the notes.
#[derive(Debug, Default)]
pub struct Notes(pub Vec<Note>);

impl Observer for Notes {
    fn note(&mut self, note: Note) {
        self.0.push(note);
    }
}

/// The program to evaluate.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub files: Vec<PathBuf>,
    /// Stack inputs from `.df` files of facts (`--input-file`).
    pub input_files: Vec<PathBuf>,
    /// The providers, over the program's providers' `use`s
    /// (`--provider`); none: the program's.
    pub providers: Vec<String>,
}

/// A program loaded: its statements, its stack, its declared inputs.
pub struct Loaded {
    pub files: Vec<PathBuf>,
    /// The program's project's manifest.
    pub manifest: Option<Manifest>,
    /// The program, the input files' facts added.
    pub program: Program,
    /// The program's files, as sources.
    pub relations: Vec<Relation>,
    /// The stack's settings (dform.toml's) and its providers' `use`s.
    pub cfg: stack::Stack,
    /// The stack's name.
    pub stack: String,
    pub providers: Vec<String>,
    /// The program lowered, when it lowers (when it does not, evaluation
    /// reports why).
    pub lowered: Option<transform::Lowered>,
    /// The stack's and the instances' typed inputs.
    pub declared: Vec<Declared>,
    /// The inputs a fact of the program gives (an input file's).
    pub given: BTreeSet<String>,
    /// The stacks the program's `use`s name: the instances its keyed reads
    /// of deployments are of (R-73).
    pub deployed: Vec<crate::syntax::resolve::Deployed>,
}

/// Load the program of `t`, its files read through `read`; `version` is the
/// running dform's (for dform.toml's requirement).
pub fn load(t: &Target, version: &str, read: Reader, obs: &mut dyn Observer) -> Result<Loaded> {
    let files = t.files.clone();
    let Some(first) = files.first() else {
        bail!("internal: a run with no program");
    };
    let manifest = match project::manifest_root(first) {
        Some(root) => Some(Manifest::load(&root.join(project::MANIFEST), version)?),
        None => None,
    };
    let (mut program, loaded, deployed) = loader::load_program_files_with(&files, read)?;
    // The program's files, each a source the controller watches (R-39).
    let relations = crate::watch::program_sources(&loaded);
    obs.relations(&relations);
    // The stack's settings (the loader's, from the manifest) and its
    // providers' `use`s, their sources the manifest's.
    let mut cfg = stack::config(&program)?;
    if let Some(m) = &manifest {
        with_manifest(&mut cfg, m);
    }
    let stack = cfg.name.clone().unwrap_or_else(|| state::stack_name(first));
    let providers = if t.providers.is_empty() {
        cfg.providers.clone()
    } else {
        t.providers.clone()
    };
    let lowered = transform::lower(&program).ok();
    let declared = lowered
        .as_ref()
        .map(|l| l.inputs.clone())
        .unwrap_or_default();
    let mut given = input_fact_keys(&program);
    for f in &t.input_files {
        let src = std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("read --input-file {}: {e}", f.display()))?;
        let facts = crate::parser::parse_file(&f.display().to_string(), &src)?;
        let stmts = inputs::file_stmts(&facts, &declared)?;
        given.extend(facts.statements.iter().filter_map(|s| match s {
            Stmt::Fact(a) => Some(a.pred.clone()),
            _ => None,
        }));
        program.statements.extend(stmts);
    }
    Ok(Loaded {
        files,
        manifest,
        program,
        relations,
        cfg,
        stack,
        providers,
        lowered,
        declared,
        given,
        deployed,
    })
}

/// What a run that starts providers says of a program that names none
/// (R-26): nothing is started in its place.
pub const NO_PROVIDER: &str = "the program names no provider: add `use NAME` \
     (dform.toml names its source) or run under `dev --provider`";

impl Loaded {
    /// The program names no provider but built-in ones (`file`, `env`,
    /// `time`) and declares no resource type of its own for the mock to
    /// play: its run starts no provider, the mock's `fake` included (R-26).
    pub fn starts_none(&self) -> bool {
        self.providers.is_empty()
            && !self.program.statements.iter().any(|s| {
                matches!(s, Stmt::Pending(p)
                    if matches!(p.kind, crate::ast::PendingKind::TypeDecl { .. }))
            })
    }

    /// A run that starts providers needs one: a program with no
    /// provider's `use`, run with no `--provider`, starts none (R-26).
    pub fn require_provider(&self) -> Result<()> {
        let named = self
            .program
            .statements
            .iter()
            .any(|s| matches!(s, Stmt::Provider(_)));
        if self.providers.is_empty() && !named {
            bail!(NO_PROVIDER);
        }
        Ok(())
    }
}

/// Which deployment of a loaded program, and where its objects are.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// The state root, `dform.state/` at the project root.
    pub root: PathBuf,
    /// Stack inputs: `--set`, and the key values.
    pub set: Vec<(String, Value)>,
    /// The deployment, over the one `set` names (`stack rekey`'s old one).
    pub instance: Option<Instance>,
    /// A world fixture (`dev --world`): state beside it, not the stack's.
    pub world: Option<PathBuf>,
    /// The discovery inventory, over the default.
    pub inventory: Option<PathBuf>,
    /// The run reads or moves the deployment's objects only: the
    /// program's other inputs need not be given.
    pub objects_only: bool,
}

/// A deployment located: which it is and where its state and world are.
pub struct Located {
    pub loaded: Loaded,
    /// The program with the policy rules (`zset::POLICY_RULES`) every
    /// evaluation carries.
    pub program: Program,
    /// The `input` facts of `Selection::set`.
    pub set_facts: Vec<Atom>,
    pub instance: Instance,
    /// `instance`'s name.
    pub deployment: String,
    /// Where the stack's deployments are: its backend's, else the state
    /// root's.
    pub base: Location,
    /// The deployment's directory under the state root.
    pub home: PathBuf,
    /// Where the deployment was handed over to, and whom.
    pub handed: Option<(String, Location)>,
    /// Where the deployment's objects are.
    pub location: Location,
    pub paths: state::StackPaths,
    pub times: store::LeaseTimes,
    pub dep: store::Deployment,
    pub root: PathBuf,
    pub world: Option<PathBuf>,
}

impl Loaded {
    /// The deployment `sel` selects, its store opened through `s3` when
    /// it is in a bucket.
    pub fn locate(self, sel: &Selection, s3: OpenS3, obs: &mut dyn Observer) -> Result<Located> {
        let set_keys: Vec<String> = sel.set.iter().map(|(k, _)| k.clone()).collect();
        for w in lint::lint(&self.program, &set_keys) {
            obs.note(Note::Warning(w));
        }
        // Every evaluation carries the rules that derive the lifecycle
        // denies from the deformation the planner hands back.
        let program = zset::with_policy_rules(self.program.clone())?;
        let mut given = self.given.clone();
        given.extend(set_keys);
        let set_facts = inputs::set_facts(&self.declared, &sel.set)?;
        let instance = match &sel.instance {
            Some(i) => i.clone(),
            None => stack::instance(&self.cfg, &self.stack, &program, &set_facts)?,
        };
        let deployment = instance.name();
        if !sel.objects_only {
            inputs::check_required(&self.declared, &given)?;
        }
        let root = sel.root.clone();
        let base = stack_location(&root, &self.stack, self.cfg.backend.as_ref());
        let keyed = deployment_location(
            &root,
            &self.stack,
            self.cfg.backend.as_ref(),
            instance.segment().as_deref(),
        );
        // The deployment's own directory under the state root: its world's
        // when its state is in a bucket (the world is the provider's).
        let home = instance.dir(&root.join(&self.stack));
        let handed = match &sel.world {
            None => stack::handed_over(&root, &deployment)?,
            Some(_) => None,
        };
        let location = match &handed {
            Some((_, loc)) => loc.clone(),
            None => keyed,
        };
        let mut paths = match &sel.world {
            Some(w) => state::world_paths(&root, w),
            None => state::StackPaths {
                state: match &location {
                    Location::Local(dir) => state::state_path(dir),
                    Location::S3(_) => state::state_path(&home),
                },
                world: stack::world_file(&location, &home),
                inventory: root.join("inventory.json"),
            },
        };
        paths.inventory = inventory(&sel.inventory, &sel.world, &paths.inventory);
        let times = self
            .manifest
            .as_ref()
            .map(|m| m.lease_times())
            .unwrap_or_default();
        let dep = match &sel.world {
            Some(_) => store::Deployment::local(&paths.state, &deployment),
            None => store::Deployment::new(location.open(s3)?, &deployment, times),
        };
        Ok(Located {
            loaded: self,
            program,
            set_facts,
            instance,
            deployment,
            base,
            home,
            handed,
            location,
            paths,
            times,
            dep,
            root,
            world: sel.world.clone(),
        })
    }
}

/// How far an evaluation goes, and what it is given besides its program.
pub struct Options<'a> {
    /// How the providers are reached.
    pub launch: &'a dyn Launch,
    /// Facts given (`--data`).
    pub data: Vec<Atom>,
    /// Failures the fake provider injects (`apply --chaos`).
    pub chaos: Vec<String>,
    /// Where providers and trust roots cache what they fetch.
    pub cache: Option<PathBuf>,
    /// The key a provider digests a sensitive value with.
    pub digest_key: Option<String>,
    /// Extern answers a plan file recorded: asked of nothing again.
    pub recorded: Vec<externs::Answer>,
    /// Every type the program plans is one its providers declare.
    pub check_types: bool,
    /// Every type's objects are discovered, not only those the program
    /// reads (`query`, `why`).
    pub discover_all: bool,
    /// The whole schema and catalog are read (a query of the schema), not
    /// the types the program names.
    pub whole_schema: bool,
    /// The collision lint of a keyed stack.
    pub collisions: bool,
    /// A violation stops the evaluation before its resources are compiled
    /// and planned (`Evaluation::violations` says which).
    pub blocking: bool,
    /// The plan and the policy pass over it.
    pub policy: bool,
    /// What the last apply derived, for the policy pass
    /// (`zset::DERIVED_AT_LAST_APPLY`, R-80).
    pub last_apply: Vec<Atom>,
}

impl<'a> Options<'a> {
    /// Nothing given, nothing checked beyond evaluating.
    pub fn new(launch: &'a dyn Launch) -> Options<'a> {
        Options {
            launch,
            data: Vec::new(),
            chaos: Vec::new(),
            cache: None,
            digest_key: None,
            recorded: Vec::new(),
            check_types: false,
            discover_all: false,
            whole_schema: false,
            collisions: false,
            blocking: false,
            policy: false,
            last_apply: Vec::new(),
        }
    }
}

/// An evaluation's resources, as compiled from its facts.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub resources: Vec<ir::Resource>,
    pub adopts: Vec<ir::Adopt>,
    pub lifecycle: zset::Lifecycle,
}

impl Compiled {
    pub fn of(res: &EvalResult, schema: &Schema) -> Result<Compiled> {
        Ok(Compiled {
            resources: ir::compile_resources(res.facts.iter().cloned(), schema)?,
            adopts: ir::compile_adopts(res.facts.iter())?,
            lifecycle: zset::Lifecycle::from_facts(&res.facts, schema)?,
        })
    }
}

/// A plan and the evaluation that sees it ([`Evaluator::plan`]).
pub struct Planned {
    /// The policy pass: the program with the plan's deformations as facts.
    pub res: EvalResult,
    /// The documents the plan was taken from.
    pub resources: Vec<ir::Resource>,
    pub plan: provider::Plan,
    pub sections: stuck::Sections,
    /// Denies over the plan: what the policy pass derives beyond the plan's
    /// own evaluation (`lifecycle prevent_destroy`, a policy on
    /// `deformation/3`).
    pub denies: Vec<String>,
}

/// What evaluates the program again, over the same providers and extern
/// answers: at a boundary, for the next tick's plan, for the outputs.
pub struct Evaluator {
    pub backend: Rc<Providers>,
    pub externs: Externs<'static>,
    /// `use ssh`'s host keys: what an apply keeps in state
    /// (`plugin::ssh::Ssh::keep`).
    pub ssh: Rc<crate::plugin::ssh::Ssh>,
    pub tables: Rc<tables::Tables>,
    /// The program with the policy rules.
    pub program: Program,
    base_extra: Vec<Atom>,
    declared: Vec<Declared>,
    secret_accounts: BTreeSet<String>,
    deployment: String,
    /// What the last apply derived, given to every policy pass (R-80).
    last_apply: Vec<Atom>,
    /// The last evaluation's facts and where it can be continued from.
    last: RefCell<Option<(Vec<Atom>, engine::Resumable)>>,
    /// The providers configured from the program's settings since the
    /// last [`Evaluator::take_configured`], by the program's name.
    configured: RefCell<Vec<String>>,
    /// The settings a secret reaches, by provider
    /// (`secrets::secret_settings`): printed `(sensitive)`.
    secret_settings: BTreeMap<String, BTreeSet<String>>,
}

impl Evaluator {
    pub fn schema(&self) -> &Schema {
        self.backend.schema()
    }

    /// The program over the world as `st` has it, as facts: round 0
    /// resolves every null the world can answer, except those of
    /// `withheld` addresses (being replaced). `more`: the deformation
    /// facts of a policy pass, which continues the last evaluation over
    /// the same facts from the first stratum that reads them; `why` labels
    /// them as the plan's, of apply tick `tick` (`None`: of `plan`).
    /// Returns the evaluation and its violations.
    pub fn evaluate_with(
        &self,
        st: &State,
        withheld: &BTreeSet<Address>,
        more: &[Atom],
        tick: Option<usize>,
    ) -> Result<(EvalResult, Vec<String>)> {
        let backend = &self.backend;
        let externs = &self.externs;
        let program = &self.program;
        let mut extra = self.base_extra.clone();
        // The types a provider configured in this run serves beyond the
        // schema the run loaded (a cluster's CRDs): their catalog too.
        extra.extend(backend.learned());
        extra.extend(executor::withhold(backend.world_facts(st)?, withheld));
        let (res, mut violations) = if more.is_empty() {
            let (mut res, mut violations, mut resumable) =
                externs.eval_resumable(program, &extra, zset::POLICY_INPUTS)?;
            // A provider the program configures, its settings now known,
            // is configured, and what it serves read again.
            let configured = backend.configure_from(&res.facts)?;
            // A kind a CRD the program made defines, served now (R-126).
            let learned = self.learn_made_kinds(&res, st)?;
            if !configured.is_empty() || learned {
                self.configured.borrow_mut().extend(configured);
                // An extern of a provider that was waiting on its settings
                // answered "not yet": ask it again.
                externs.forget_not_yet();
                extra = self.base_extra.clone();
                extra.extend(backend.learned());
                extra.extend(executor::withhold(backend.world_facts(st)?, withheld));
                (res, violations, resumable) =
                    externs.eval_resumable(program, &extra, zset::POLICY_INPUTS)?;
            }
            // Each provider reaches the account the program expects of it
            // (`expect_account`), or nothing is planned.
            backend
                .check_accounts(&res.facts, &self.secret_accounts, &self.secret_settings)
                .with_context(|| format!("deployment {}", self.deployment))?;
            *self.last.borrow_mut() = Some((extra, resumable));
            (res, violations)
        } else {
            let resumed = match &*self.last.borrow() {
                Some((seen, resumable)) if *seen == extra => Some(resumable.with_at(more, tick)?),
                _ => None,
            };
            let (res, violations) = match resumed {
                Some((res, violations)) if externs.settle(&res.facts)? => (res, violations),
                _ => {
                    // A new extern call: the answers the resumable was
                    // taken with are not all of them any more.
                    *self.last.borrow_mut() = None;
                    extra.extend(more.iter().cloned());
                    externs.eval_at(program, &extra, tick)?
                }
            };
            // At a boundary too: a provider whose settings the last tick
            // made known (a kubeconfig read from the server it created) is
            // configured now, and the program evaluated again over what it
            // serves (R-45).
            let configured = backend.configure_from(&res.facts)?;
            // So is a kind the CRD the last tick made defines (R-126).
            let learned = self.learn_made_kinds(&res, st)?;
            if !configured.is_empty() || learned {
                self.configured.borrow_mut().extend(configured);
                externs.forget_not_yet();
                *self.last.borrow_mut() = None;
                let (res, violations) = self.evaluate_with(st, withheld, more, tick)?;
                backend
                    .check_accounts(&res.facts, &self.secret_accounts, &self.secret_settings)
                    .with_context(|| format!("deployment {}", self.deployment))?;
                return Ok((res, violations));
            }
            (res, violations)
        };
        violations.extend(inputs::violations(&res.facts, &self.declared));
        Ok((res, violations))
    }

    /// A resource whose provider cannot plan it yet waits on that provider
    /// (R-110), under `later` until a run that has it: one whose settings
    /// the program gives and this evaluation does not know (a kubeconfig
    /// read from a server still booting), as `provider k8s (kubeconfig
    /// from k3s.kubeconfig)`; one of a kind no schema has yet, a cluster's
    /// CRD, as `provider k8s  schema`, created as written, untyped.
    /// A resource already waiting on a null keeps what it waits on.
    ///
    /// A kind no schema has that a CRD the program makes defines waits on
    /// that CRD instead (R-126), under `later` as `waits on
    /// k8s.custom_resource_definition "middlewares.traefik.io"`: apply makes
    /// it at the tick after the CRD's, once its provider serves the kind
    /// ([`Evaluator::learn_made_kinds`]). One nothing makes, of a provider
    /// whose cluster was reached, is an error naming the CRD it lacks.
    fn wait_on_providers(
        &self,
        plan: &mut provider::Plan,
        resources: &[ir::Resource],
        sections: &mut stuck::Sections,
        st: &State,
    ) -> Result<()> {
        let crds = crds_made(resources);
        for r in resources {
            let typ = &r.addr.typ;
            // A kind no schema has: its provider's cluster serves it once
            // reached (R-110), or does not, reached (R-126).
            let schema_wait = matches!(self.backend.waits(typ), Some(ProviderWait::Schema(_)))
                || self.backend.unserved(typ);
            let label = match (self.provider_wait(typ), crd_of(&crds, typ)) {
                (_, Some(crd)) if schema_wait => report::address(crd),
                (Some(label), _) => label,
                (None, _) if schema_wait => bail!(self.no_crd(&r.addr)),
                (None, _) => continue,
            };
            if schema_wait {
                let doc = engine::value_to_json(&r.attrs);
                let (kind, changes) = match st.get(&r.addr) {
                    Some(_) => (provider::ActionKind::Pending, Vec::new()),
                    None => (
                        provider::ActionKind::Create,
                        provider::diff(self.schema(), &r.addr.typ, None, Some(&doc)),
                    ),
                };
                plan.actions.push(provider::Action {
                    kind,
                    addr: r.addr.clone(),
                    changes,
                    on: BTreeSet::from([label.clone()]),
                });
            }
            // One waiting on what dform's own extern has not answered (the
            // kubeconfig `ssh.read` reads from a host still booting) waits
            // on it through the provider: said as both.
            let waits = sections
                .pending
                .entry((r.addr.typ.clone(), r.addr.name.clone()))
                .or_default();
            let read = |l: &String| {
                crate::value::null_owner(l).is_some_and(|(t, _)| crate::externs::in_process(&t))
            };
            if waits.is_empty() || waits.iter().any(read) {
                waits.insert(label);
            }
        }
        Ok(())
    }

    /// The error of a resource whose kind its provider's cluster does not
    /// serve and no CRD the program makes defines (R-126), at its
    /// statement.
    fn no_crd(&self, addr: &Address) -> String {
        let (kind, crd) = crate::crd::expected(&addr.typ)
            .unwrap_or_else(|| (addr.typ.clone(), "a CustomResourceDefinition".into()));
        let at = self
            .program
            .statements
            .iter()
            .find_map(|s| {
                let (typ, span) = match s {
                    Stmt::Resource(r) => (&r.typ, r.span),
                    Stmt::Rule(r) if r.head.pred == "want" => (r.head.args.first()?, r.head.span),
                    Stmt::Fact(a) if a.pred == "want" => (a.args.first()?, a.span),
                    _ => return None,
                };
                (typ.as_str() == Some(&addr.typ))
                    .then(|| crate::diag::place(span))
                    .flatten()
            })
            .map(|p| format!("{p}: "))
            .unwrap_or_default();
        format!(
            "{at}{}: the cluster has no kind {kind} and nothing in the program makes its CRD \
             ({crd})",
            report::address(addr)
        )
    }

    /// Ask the providers again for the kinds no schema has that a CRD the
    /// program made defines, the CRD in state (R-126): the cluster serves
    /// them once it has the CRD. Whether the schema learned any, so the
    /// program is evaluated again over it.
    fn learn_made_kinds(&self, res: &EvalResult, st: &State) -> Result<bool> {
        let typ = |a: &Atom| a.args.first().and_then(Term::as_str).map(str::to_string);
        let waiting: BTreeSet<String> = res
            .facts
            .iter()
            .filter(|a| a.pred == "want")
            .filter_map(typ)
            .filter(|t| self.backend.unserved(t))
            .collect();
        if waiting.is_empty() {
            return Ok(false);
        }
        let facts = res
            .facts
            .iter()
            .filter(|a| typ(a).is_some_and(|t| crate::crd::TYPES.contains(&t.as_str())))
            .cloned();
        let made = ir::compile_resources(facts, self.schema())?;
        let types: BTreeSet<String> = crds_made(&made)
            .into_iter()
            .filter(|(crd, _)| st.get(crd).is_some())
            .flat_map(|(_, ts)| ts)
            .filter(|t| waiting.contains(t))
            .collect();
        if types.is_empty() {
            return Ok(false);
        }
        self.backend.relearn(&types)
    }

    /// What a resource of `typ` waits on before its provider plans it, as
    /// `later` prints it ([`Providers::waits`]).
    pub fn provider_wait(&self, typ: &str) -> Option<String> {
        Some(match self.backend.waits(typ)? {
            ProviderWait::Settings(p) => self.settings_label(&p),
            ProviderWait::Schema(p) => format!("provider {p}  schema"),
        })
    }

    /// The providers configured from the program's settings since the
    /// last call (at a tick boundary: a kubeconfig the tick made known).
    pub fn take_configured(&self) -> Vec<String> {
        self.configured.take()
    }

    /// What a run says of provider `name` configured from the program's
    /// settings: each setting's key and `KEY = VALUE`, a secret one's
    /// value `(sensitive)`, a public one's itself; with `how`, the
    /// expression it is written as too (`from k3s.kubeconfig`). Never a
    /// secret's value (R-45).
    pub fn settings_shown(
        &self,
        name: &str,
        facts: &BTreeSet<Atom>,
        how: bool,
    ) -> Vec<(String, String)> {
        let secret = self.secret_settings.get(name);
        let mut out = Vec::new();
        for a in facts.iter().filter(|a| a.pred == "provider_config") {
            let [Term::Val(Value::Str(n)), Term::Val(Value::Obj(settings))] = a.args.as_slice()
            else {
                continue;
            };
            if n != name {
                continue;
            }
            for (k, v) in settings {
                let shown = match secret.is_some_and(|s| s.contains(k)) {
                    true => "(sensitive)".to_string(),
                    false => crate::partition::fmt_bare(v),
                };
                let from = match how {
                    true => self.setting_source(name, k),
                    false => None,
                };
                let text = match from {
                    Some(t) => format!("{k} = {shown} from {t}"),
                    None => format!("{k} = {shown}"),
                };
                out.push((k.clone(), text));
            }
        }
        out.dedup();
        out
    }

    /// The expression setting `key` of provider `name` is written as.
    fn setting_source(&self, name: &str, key: &str) -> Option<String> {
        self.program.statements.iter().find_map(|s| {
            let head = match s {
                Stmt::Rule(r) => &r.head,
                Stmt::Fact(a) => a,
                _ => return None,
            };
            match head.args.as_slice() {
                [Term::Val(Value::Str(n)), Term::Obj(_)]
                    if head.pred == "provider_config" && n == name =>
                {
                    setting_text(head.span, key)
                }
                _ => None,
            }
        })
    }

    /// `provider NAME  KEY = EXPR, ..`: a provider block's settings (its
    /// `provider_config` row) as the program writes them.
    fn settings_label(&self, name: &str) -> String {
        let mut from: Vec<String> = Vec::new();
        for s in &self.program.statements {
            let head = match s {
                Stmt::Rule(r) => &r.head,
                Stmt::Fact(a) => a,
                _ => continue,
            };
            let [Term::Val(Value::Str(n)), Term::Obj(settings)] = head.args.as_slice() else {
                continue;
            };
            if head.pred != "provider_config" || n != name {
                continue;
            }
            for k in settings.keys() {
                from.push(match setting_text(head.span, k) {
                    Some(t) => format!("{k} = {t}"),
                    None => k.clone(),
                });
            }
        }
        from.dedup();
        match from.as_slice() {
            [] => format!("provider {name}"),
            from => format!("provider {name}  {}", from.join(", ")),
        }
    }

    /// The program over the world as `st` has it.
    pub fn evaluate(&self, st: &State) -> Result<(EvalResult, Vec<String>)> {
        self.evaluate_with(st, &BTreeSet::new(), &[], None)
    }

    /// The provider's plan for evaluation `res` (whose violations are
    /// `violations`, its resources `resources`, `adopts` and `lifecycle`),
    /// and the policy over it. A replace
    /// makes a new object, so the nulls that named the old one are
    /// retracted (`executor`): the program is evaluated again without the
    /// replaced identities, and what reads them is held until the
    /// replacement exists. Then the policy pass (E §2.8): the plan's
    /// deformations go back to the evaluator as facts and the program is
    /// evaluated once more; the denies it derives beyond the plan's own
    /// evaluation are the denies over the plan.
    pub fn plan(
        &self,
        res: EvalResult,
        violations: &[String],
        resources: Vec<ir::Resource>,
        adopts: &[ir::Adopt],
        lifecycle: &zset::Lifecycle,
        st: &State,
    ) -> Result<Planned> {
        let backend = &self.backend;
        let schema = self.schema();
        // A resource with a conflicting attribute is not planned: the
        // report shows the conflict, and the deny blocks an apply. Nor is
        // one whose provider has no schema of its type yet (R-110).
        // Nor one of a kind its provider's cluster does not serve (R-126):
        // it waits on the CRD the program makes, or is an error.
        let crds = crds_made(&resources);
        if let Some(r) = resources
            .iter()
            .find(|r| backend.unserved(&r.addr.typ) && crd_of(&crds, &r.addr.typ).is_none())
        {
            bail!(self.no_crd(&r.addr));
        }
        let asked = |res: &EvalResult, docs: &[ir::Resource]| -> Vec<ir::Resource> {
            let conflicted = report::conflicted(res);
            docs.iter()
                .filter(|r| !conflicted.contains(&r.addr))
                .filter(|r| !matches!(backend.waits(&r.addr.typ), Some(ProviderWait::Schema(_))))
                .filter(|r| !backend.unserved(&r.addr.typ))
                .cloned()
                .collect()
        };
        let mut plan = backend.plan(&asked(&res, &resources), adopts, lifecycle, st)?;
        let replaced = executor::replaced(&plan);
        let (res, violations, resources) = if replaced.is_empty() {
            (res, violations.to_vec(), resources)
        } else {
            let (again, violations) = self.evaluate_with(st, &replaced, &[], None)?;
            let docs = ir::compile_resources(again.facts.iter().cloned(), schema)?;
            plan =
                backend.plan_retracting(&asked(&again, &docs), adopts, lifecycle, st, &replaced)?;
            executor::hold_dependents(&mut plan, &docs, &replaced);
            (again, violations, docs)
        };
        let mut sections = sections(&res, &resources, schema);
        self.wait_on_providers(&mut plan, &resources, &mut sections, st)?;
        executor::hold_deposed(&mut plan, &resources, &sections);
        // The resource rules that may derive after a boundary (pending
        // groups), for the plan's policy pass.
        let may_derive: Vec<Atom> = res
            .may_derive
            .iter()
            .filter(|m| m.head.pred == "want")
            .map(|m| m.fact())
            .collect();
        let instances = zset::Instances::from_facts(&res.facts).with(&st.instances);
        drop(res);
        let observed = backend.observe(st)?;
        let before = observed
            .iter()
            .map(|(a, d)| (a.clone(), Some(d.clone())))
            .collect();
        let mut facts = zset::deformation_facts(
            plan.actions.iter().filter_map(|a| {
                let held = report::waits_on(a, &sections).is_some();
                Some((zset::deformation_kind(&a.kind, held)?, &a.addr))
            }),
            &before,
            &observed,
        );
        facts.extend(zset::instance_facts(
            plan.actions.iter().filter_map(|a| {
                let held = report::waits_on(a, &sections).is_some();
                Some((zset::deformation_kind(&a.kind, held)?, &a.addr))
            }),
            &instances,
        ));
        facts.extend(may_derive);
        facts.extend(self.last_apply.iter().cloned());
        let (res, all) = self.evaluate_with(st, &replaced, &facts, None)?;
        let again = ir::compile_resources(res.facts.iter().cloned(), schema)?;
        if again.len() != resources.len()
            || again
                .iter()
                .zip(&resources)
                .any(|(a, b)| a.addr != b.addr || a.attrs != b.attrs)
        {
            bail!(
                "a resource rule reads deformation/3 or world_digest/2: the plan would \
                 depend on itself (only policy may read the deformation)"
            );
        }
        let denies = all
            .into_iter()
            .filter(|v| !violations.contains(v))
            .collect();
        Ok(Planned {
            res,
            resources,
            plan,
            sections,
            denies,
        })
    }
}

/// The CRD among `crds` ([`crds_made`]) that defines `typ`.
fn crd_of<'a>(crds: &'a [(Address, Vec<String>)], typ: &str) -> Option<&'a Address> {
    crds.iter()
        .find(|(_, ts)| ts.iter().any(|t| t == typ))
        .map(|(crd, _)| crd)
}

/// The CRDs among `resources` and the types each defines
/// (`crd::defines`).
fn crds_made(resources: &[ir::Resource]) -> Vec<(Address, Vec<String>)> {
    resources
        .iter()
        .filter(|r| crate::crd::TYPES.contains(&r.addr.typ.as_str()))
        .filter_map(|r| Some((r.addr.clone(), crate::crd::defines(&r.attrs)?)))
        .collect()
}

/// The text of the entry `key = EXPR` in the block at `span`: `EXPR`, to
/// the end of its line or the next `,` or `}`.
fn setting_text(span: Span, key: &str) -> Option<String> {
    let (_, text) = crate::diag::source_of(span)?;
    let block = text.get(span.start as usize..span.end as usize)?;
    let body = &block[block.find('{')? + 1..];
    body.split(['\n', ','])
        .filter_map(|e| e.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_end_matches('}').trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `l` with the data sources `schema` declares that it reads and does
/// not declare itself (R-106): each an `extern` declaration, as if the
/// program had written it.
fn with_schema_externs(l: &transform::Lowered, schema: &Schema) -> transform::Lowered {
    if schema.externs.is_empty() {
        return l.clone();
    }
    let own: BTreeSet<&str> = l.extern_fns.iter().map(|f| f.name.as_str()).collect();
    let read: BTreeSet<&str> = l
        .program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Rule(r) => Some(&r.body),
            _ => None,
        })
        .flatten()
        .filter_map(|l| match l {
            Lit::Pos(a) | Lit::Not(a) => Some(a.pred.as_str()),
            _ => None,
        })
        .collect();
    let mut out = l.clone();
    for (p, f) in &schema.externs {
        if read.contains(p.as_str()) && !own.contains(p.as_str()) {
            out.extern_fns.push(f.clone());
            out.externs.insert(crate::ast::Extern {
                pred: p.clone(),
                arity: f.args.len(),
                span: f.span,
            });
        }
    }
    out
}

/// E §2.7's sections for an evaluation: what waits on a boundary.
pub fn sections(res: &EvalResult, resources: &[ir::Resource], schema: &Schema) -> stuck::Sections {
    let docs = resources
        .iter()
        .map(|r| ((r.addr.typ.clone(), r.addr.name.clone()), r.attrs.clone()))
        .collect();
    stuck::sections(&res.stuck, &res.may_derive, &res.facts, &docs, schema)
}

/// The nulls of `on` (labels) an apply can wait on (R-81), in order:
/// what the world has not reached yet, a computed value of an object that
/// exists (a Job's `status.succeeded`, a cluster's endpoint), and what an
/// extern answered "not yet" (`not_yet`, `Externs::not_yet`). Waiting
/// changes nothing for the rest: another stack's output, an input, a value
/// of an object no tick makes.
pub fn waitable(on: &BTreeSet<String>, state: &State, not_yet: &BTreeSet<String>) -> Vec<String> {
    on.iter()
        .filter(|l| {
            not_yet.contains(*l)
                || crate::value::null_owner(l)
                    .is_some_and(|(typ, name)| state.get(&Address { typ, name }).is_some())
        })
        .cloned()
        .collect()
}

/// An evaluation of a deployment.
pub struct Evaluation {
    pub located: Located,
    /// Other stacks' published outputs, as read.
    pub outputs: Vec<stack::Read>,
    /// The deployment's state, `moved/3` applied, uncertain calls resolved.
    pub st: State,
    /// The moves `moved/3` made.
    pub moves: Vec<(Address, Address)>,
    /// The program's own evaluation, and its violations.
    pub res: EvalResult,
    pub violations: Vec<String>,
    /// The collision lint's findings.
    pub collisions: Vec<lint::Collision>,
    /// Prints `res`'s values redacted.
    pub redact: Redactor,
    /// The resources of `res`; not compiled when a violation blocks
    /// (`Options::blocking`).
    pub compiled: Result<Compiled>,
    /// The plan and its policy pass, when asked for.
    pub policy: Option<Result<Planned>>,
    pub evaluator: Evaluator,
}

impl Located {
    /// Other stacks' published outputs: each deployment the program names,
    /// of the project (every registered one, when it names one by a
    /// variable) or of a remote, through its backend (`s3`).
    pub fn read_outputs(&self, s3: OpenS3) -> Result<Vec<stack::Read>> {
        let (named, any_name) = self
            .loaded
            .lowered
            .as_ref()
            .map(|l| stack::named_outputs(&l.program, &self.loaded.deployed))
            .unwrap_or_default();
        let remotes = self
            .loaded
            .manifest
            .as_ref()
            .map(|m| m.remotes())
            .unwrap_or_default();
        stack::stack_outputs(
            &self.root,
            &self.deployment,
            (!any_name).then_some(&named),
            &remotes,
            s3,
        )
    }

    /// Evaluate the deployment, `outputs` the other stacks' it reads.
    pub fn evaluate(
        self,
        outputs: Vec<stack::Read>,
        opts: &Options,
        obs: &mut dyn Observer,
    ) -> Result<Evaluation> {
        let l = &self.loaded;
        let lowered = l.lowered.as_ref();
        let secret_outputs = stack::secret_outputs(&outputs);
        let held = stack::held(&outputs);
        // What dform.toml grants each provider, for the launcher (R-13b).
        plugin::host::register(l.manifest.iter().flat_map(|m| m.grants()));
        let backend = Rc::new(if l.starts_none() {
            Providers::none()
        } else {
            Providers::start_deferred(
                opts.launch,
                &l.providers,
                &plugin::Config {
                    world: self.paths.world.clone(),
                    inventory: self.paths.inventory.clone(),
                    chaos: opts.chaos.clone(),
                    cache: opts.cache.clone(),
                    configured: provider_configs(&self.program),
                    stack: self.deployment.clone(),
                    blocks: l.cfg.provider_blocks.clone(),
                    digest_key: opts.digest_key.clone(),
                    policies: l
                        .manifest
                        .as_ref()
                        .map(|m| m.policies())
                        .unwrap_or_default(),
                    held: held.clone(),
                    worlds: outputs
                        .iter()
                        .filter(|r| held.values().any(|h| h.deployment == r.name))
                        .filter_map(|r| Some((r.name.clone(), r.world.clone()?)))
                        .collect(),
                },
            )?
        });
        // Externs are asked on demand: a table's of its file, else of the
        // file provider, else of the providers.
        let tables = Rc::new(tables::Tables::default());
        let mut st = self.dep.load_state()?;
        // `random.*` derive from the deployment's master (R-60): made on
        // first use when it is the stack's key.
        if lowered.is_some_and(|l| crate::functions::random::called(&l.program)) {
            let ikm = crate::functions::random::master(|| self.dep.plan_key())?;
            crate::functions::random::set_master(ikm, &self.deployment);
        }
        // `memo.first`: what state keeps, a sealed one opened with the
        // stack's key.
        let memos = Rc::new(crate::memo::Memos::new(
            &st,
            match st.memo.values().any(|m| m.value.is_none()) {
                true => self.dep.existing_plan_key()?,
                false => None,
            },
        ));
        // `use ssh`: the host keys state knows, and those met.
        let ssh = Rc::new(crate::plugin::ssh::Ssh::new(st.known_hosts.clone()));
        let mut base_extra = self.set_facts.clone();
        base_extra.extend(opts.data.iter().cloned());
        base_extra.extend(stack::output_facts(&outputs, &l.deployed));
        // A deployment it reads that has not been applied: what reads it
        // waits on it (R-121).
        if let Some(lw) = lowered {
            base_extra.extend(stack::unapplied_facts(
                &lw.program,
                &l.deployed,
                &self.instance.key,
                &outputs,
            ));
        }
        // The manifest, as facts policy may read.
        if let Some(m) = &l.manifest {
            base_extra.extend(m.facts());
        }
        // The schema is asked for once the run knows the types it names.
        let types = match opts.discover_all {
            true => None,
            false => world_types(lowered),
        };
        let discovered = backend.discover(types.as_ref())?;
        let scope = match opts.whole_schema {
            true => None,
            false => catalog_scope(&self.program, &base_extra, &discovered, &st),
        };
        backend.load_schema(scope.as_ref())?;
        // The data sources the providers' schemas declare that the program
        // reads with no `extern` line of its own (R-106): declared as if it
        // had one.
        let with_externs = lowered.map(|l| with_schema_externs(l, backend.schema()));
        let lowered = with_externs.as_ref();
        let externs = {
            let (no_program, no_fns) = (Program::default(), vec![]);
            let program_dir = project::base_of(&l.files[0]);
            let (tables, backend, ssh) = (tables.clone(), backend.clone(), ssh.clone());
            Externs::new(
                lowered.map_or(&no_program, |l| &l.program),
                lowered.map_or(&no_fns, |l| &l.extern_fns),
                move |f, inputs| {
                    if let Some(r) = tables.answer(f, inputs) {
                        return r;
                    }
                    if let Some(r) = externs::file(f, inputs, &program_dir) {
                        return r;
                    }
                    if let Some(r) = externs::env(f, inputs) {
                        return r;
                    }
                    if let Some(r) = externs::time(f) {
                        return r;
                    }
                    if let Some(r) = ssh.answer(f, inputs) {
                        return r;
                    }
                    if let Some(r) = memos.answer(f, inputs) {
                        return r;
                    }
                    backend.query_extern(f, inputs)
                },
            )
        };
        // What the plan file read, before asking.
        externs.preload(opts.recorded.clone());
        // What a run plans, its providers declare: a type none does would
        // be handed to one that knows nothing of it.
        if opts.check_types {
            backend.check_types(&self.program, &l.providers)?;
        }
        // A provider configured from what it serves itself is a cycle.
        if let Some(l) = lowered {
            backend.check_configuration(&l.program, &l.extern_fns, |p| {
                externs::in_process(p) || tables::describe(p).is_some()
            })?;
        }
        // Apply calls whose answer was lost, resolved before anything is
        // planned: a plan sees what they did (`apply` writes it down).
        for line in executor::resolve_uncertain(&backend, &mut st)? {
            obs.note(Note::Resolved(line));
        }
        // The static secret pass and the refinement checks (a literal that
        // violates one, E0306), against the provider's schema.
        if let Some(l) = lowered {
            crate::secrets::check(l, backend.schema(), &secret_outputs)?;
            // Where the pass found secrets, for the redactor (R-128).
            base_extra.extend(crate::secrets::taint(l, backend.schema(), &secret_outputs));
            // A memo of a secret is kept sealed and recorded nowhere.
            externs.mark_secret(&crate::secrets::secret_memos(
                l,
                backend.schema(),
                &secret_outputs,
            ));
            crate::refine::check(&l.program, backend.schema())?;
            crate::types::check(&l.program, backend.schema())?;
            // Column types again, the attributes read into a column typed
            // by the schema (R-34).
            crate::infer::infer(
                &l.program,
                &l.extern_fns,
                &l.inputs,
                &l.declared,
                Some(backend.schema()),
            )?;
            for (at, n) in transform::computed_reads(&l.program.statements, backend.schema()) {
                obs.note(Note::Computed(at, n));
            }
        }
        // An `expect_account` a secret reaches is named by its label.
        let secret_accounts = lowered
            .map(|l| crate::secrets::secret_expected_accounts(l, backend.schema(), &secret_outputs))
            .unwrap_or_default();
        let secret_settings = lowered
            .map(|l| crate::secrets::secret_settings(l, backend.schema(), &secret_outputs))
            .unwrap_or_default();
        base_extra.extend(backend.catalog(scope.as_ref())?);
        base_extra.extend(discovered);
        base_extra.extend(obs.facts(&backend, &st)?);
        // Quantity and time literals read as their attributes' types
        // (R-66, R-62), now the schema is known.
        let mut program = self.program.clone();
        if let Some(l) = lowered {
            let own: BTreeSet<&str> = program
                .statements
                .iter()
                .filter_map(|s| match s {
                    Stmt::ExternFn(f) => Some(f.name.as_str()),
                    _ => None,
                })
                .collect();
            let more: Vec<Stmt> = l
                .extern_fns
                .iter()
                .filter(|f| !own.contains(f.name.as_str()))
                .map(|f| Stmt::ExternFn(f.clone()))
                .collect();
            program.statements.extend(more);
        }
        crate::types::read(&mut program, backend.schema())?;
        let evaluator = Evaluator {
            backend: backend.clone(),
            externs,
            ssh,
            tables: tables.clone(),
            program,
            base_extra,
            declared: l.declared.clone(),
            secret_accounts,
            deployment: self.deployment.clone(),
            last_apply: opts.last_apply.clone(),
            last: RefCell::new(None),
            configured: RefCell::new(Vec::new()),
            secret_settings,
        };
        let (mut res, mut violations) = evaluator.evaluate(&st)?;
        // moved/3 rewrites state's identity before the diff (E §3.4); round
        // 0 must see the new addresses, so the program is evaluated again.
        let moves =
            st.apply_moves(&zset::Lifecycle::from_facts(&res.facts, backend.schema())?.moved);
        if !moves.is_empty() {
            (res, violations) = evaluator.evaluate(&st)?;
        }
        // The tables' sources; a git table whose ref has moved since the
        // deployment was last applied says so.
        obs.tables(&tables.sources());
        for m in tables::moved(&st.externs, &evaluator.externs.recorded()) {
            obs.note(Note::TableMoved(m));
        }
        // The collision lint of a keyed stack: a name every deployment
        // writes the same.
        let collisions = if opts.collisions && !l.cfg.keys.is_empty() && !l.cfg.isolated {
            let keys: Vec<String> = l.cfg.keys.iter().map(|(k, _)| k.clone()).collect();
            // A type's provider, by the name a provider's `use` block that
            // configures it gives it (its namespace's, R-36).
            let configured = provider_configs(&self.program);
            let provider = |t: &str| {
                configured
                    .iter()
                    .find(|n| backend.serves(n, t) || t.starts_with(&format!("{n}.")))
                    .cloned()
                    .unwrap_or_else(|| backend.provider_of(t).to_string())
            };
            lint::key_collisions(&res, backend.schema(), &keys, &self.deployment, provider)
        } else {
            Vec::new()
        };
        for c in &collisions {
            obs.note(Note::Collision(c.text.clone()));
        }
        // Policy messages quote values and rule text: redacted.
        let redact = Redactor::new(&res.facts, backend.schema());
        for w in &res.warnings {
            obs.note(Note::Policy(redact.text(w)));
        }
        let (compiled, policy) = if opts.blocking && !violations.is_empty() {
            (Err(anyhow::anyhow!("blocked by constraints")), None)
        } else {
            let compiled = Compiled::of(&res, backend.schema());
            // No plan when the resources do not compile: that is why.
            let plan = |c: &Compiled| {
                evaluator.plan(
                    res.clone(),
                    &violations,
                    c.resources.clone(),
                    &c.adopts,
                    &c.lifecycle,
                    &st,
                )
            };
            let policy = match (&compiled, opts.policy) {
                (Ok(c), true) => Some(plan(c)),
                (Err(_), true) => Some(Compiled::of(&res, backend.schema()).and_then(|c| plan(&c))),
                (_, false) => None,
            };
            (compiled, policy)
        };
        Ok(Evaluation {
            located: self,
            outputs,
            st,
            moves,
            res,
            violations,
            collisions,
            redact,
            compiled,
            policy,
            evaluator,
        })
    }
}

/// Load, locate and evaluate the deployment `sel` selects of the program
/// of `t`: read only.
pub fn evaluate(
    t: &Target,
    sel: &Selection,
    version: &str,
    read: Reader,
    s3: OpenS3,
    opts: &Options,
    obs: &mut dyn Observer,
) -> Result<Evaluation> {
    let located = load(t, version, read, obs)?.locate(sel, s3, obs)?;
    let outputs = located.read_outputs(s3)?;
    located.evaluate(outputs, opts, obs)
}

/// What `why` and `query` read, and an editor shows: the policy pass's
/// evaluation, else (no plan could be made) the program's own.
pub struct Explained {
    pub res: EvalResult,
    /// The program's violations and the denies over the plan.
    pub violations: Vec<String>,
    /// Why no plan could be made.
    pub error: Option<anyhow::Error>,
    pub redact: Redactor,
}

impl Evaluation {
    pub fn schema(&self) -> &Schema {
        self.evaluator.schema()
    }

    /// The evaluation to explain: the policy pass's when there is one.
    pub fn explained(&mut self) -> Explained {
        let mut violations = self.violations.clone();
        let (res, error) = match self.policy.take() {
            Some(Ok(p)) => {
                violations.extend(p.denies);
                (p.res, None)
            }
            Some(Err(e)) => (self.res.clone(), Some(e)),
            None => (self.res.clone(), None),
        };
        let redact = Redactor::new(&res.facts, self.schema());
        Explained {
            res,
            violations,
            error,
            redact,
        }
    }
}

/// How bad a problem is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// Where a problem, or what it relates to, is.
#[derive(Debug, Clone)]
pub enum At {
    /// A span of a source the loader registered.
    Span(Span),
    /// `file:line:col ...`, as a stated fact's provenance names it.
    Place(String),
    /// Nowhere more precise: the stack's own file, at its start.
    Top,
}

/// By place: a span by where it is (`Span`'s own equality holds of any two).
impl PartialEq for At {
    fn eq(&self, other: &At) -> bool {
        match (self, other) {
            (At::Span(a), At::Span(b)) => a.same_place(b),
            (At::Place(a), At::Place(b)) => a == b,
            (At::Top, At::Top) => true,
            _ => false,
        }
    }
}

/// An error or warning of an evaluation, where it is, what it relates to
/// (the contributions a policy read), and the fixes it carries.
#[derive(Debug, Clone)]
pub struct Problem {
    pub severity: Severity,
    pub message: String,
    pub at: At,
    pub related: Vec<(At, String)>,
    pub fixes: Vec<Fix>,
}

impl Problem {
    pub fn top(severity: Severity, message: impl Into<String>) -> Problem {
        Problem {
            severity,
            message: message.into(),
            at: At::Top,
            related: Vec::new(),
            fixes: Vec::new(),
        }
    }

    fn of_diagnostic(d: &Diagnostic) -> Problem {
        let mut message = d.message.clone();
        for n in &d.notes {
            message.push_str(&format!("\nnote: {n}"));
        }
        if let Some(h) = &d.help {
            message.push_str(&format!("\nhelp: {h}"));
        }
        Problem {
            severity: Severity::Error,
            message,
            at: At::Span(d.span),
            related: d
                .labels
                .iter()
                .map(|(s, m)| (At::Span(*s), m.clone()))
                .collect(),
            fixes: d.fixes.clone(),
        }
    }
}

/// An error as problems: its diagnostics at their spans, with their
/// fixes, else its text at the top of the stack's file.
pub fn of_error(e: &anyhow::Error) -> Vec<Problem> {
    match e.chain().find_map(|x| x.downcast_ref::<Diagnostics>()) {
        Some(Diagnostics(ds)) => ds.iter().map(Problem::of_diagnostic).collect(),
        None => vec![Problem::top(Severity::Error, format!("{e:#}"))],
    }
}

impl Explained {
    /// What `dform plan` refuses and warns of, where it is: every `deny`
    /// and `warn` fact at the rule that derived it (the contributions it
    /// reads as related), a violation nothing accounts for (an input of the
    /// wrong type) at the top, and why no plan could be made.
    pub fn problems(&self, _program: &Program) -> Vec<Problem> {
        let mut out: Vec<Problem> = self.error.iter().flat_map(of_error).collect();
        let denied: BTreeSet<String> = self
            .res
            .facts
            .iter()
            .filter(|a| a.pred == "deny")
            .filter_map(policy_text)
            .collect();
        for v in &self.violations {
            if !denied.contains(v) {
                out.push(Problem::top(Severity::Error, self.redact.text(v)));
            }
        }
        for a in &self.res.facts {
            let severity = match a.pred.as_str() {
                "deny" => Severity::Error,
                "warn" => Severity::Warning,
                _ => continue,
            };
            let Some(text) = policy_text(a) else { continue };
            let Some(id) = self.res.circuit.fact_id(&engine::circuit_fact(a)) else {
                continue;
            };
            let (at, related) = provenance(&self.res, id);
            out.push(Problem {
                severity,
                message: self.redact.text(&text),
                at: at.unwrap_or(At::Top),
                related,
                fixes: Vec::new(),
            });
        }
        out
    }
}

/// A `deny` or `warn` fact as the evaluator words its violation (`engine`'s
/// `format_policy_fact`).
pub fn policy_text(a: &Atom) -> Option<String> {
    let Some(Term::Val(Value::Str(msg))) = a.args.first() else {
        return None;
    };
    match a.args.get(1) {
        None => Some(msg.clone()),
        Some(Term::Val(ctx)) => Some(format!(
            "{msg} ctx={}",
            serde_json::to_string(&engine::value_to_json(ctx)).ok()?
        )),
        Some(_) => None,
    }
}

/// Where rule `id` (`r12`) of `res` is written, when it is.
pub fn rule_span(res: &EvalResult, id: &str) -> Option<Span> {
    let i: usize = id.strip_prefix('r')?.parse().ok()?;
    let span = res.rules.get(i)?.head.span;
    (!span.is_none()).then_some(span)
}

/// Where fact `id` comes from: the nearest rule (or stated fact) that is
/// written somewhere, and the contributions of the attributes below it,
/// each with its rank.
pub fn provenance(res: &EvalResult, id: NodeId) -> (Option<At>, Vec<(At, String)>) {
    let c = &res.circuit;
    let mut at = None;
    let mut related = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = std::collections::VecDeque::from([(id, 0usize)]);
    while let Some((n, depth)) = queue.pop_front() {
        if depth > 8 || !seen.insert(n) || seen.len() > 400 {
            continue;
        }
        let View::Fact { fact, alts, .. } = c.view(n) else {
            continue;
        };
        let Some(&alt) = alts.first() else { continue };
        let View::Times { children, .. } = c.view(alt) else {
            continue;
        };
        let mut sigma = false;
        for ch in children {
            match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => {
                    sigma = id.starts_with('Σ');
                    if at.is_none()
                        && let Some(s) = rule_span(res, id)
                    {
                        at = Some(At::Span(s));
                    }
                }
                View::Leaf(Leaf::Base { span }) if at.is_none() => {
                    at = Some(At::Place(span.clone()));
                }
                _ => {}
            }
        }
        for ch in children {
            if let View::Fact { fact: f, .. } = c.view(*ch) {
                if sigma && let Some(w) = written(res, *ch) {
                    let rank = match f.args.get(4) {
                        Some(Value::Str(r)) => r.clone(),
                        _ => "?".into(),
                    };
                    let at = crate::ir::Address {
                        typ: text_of(fact.args.first()),
                        name: text_of(fact.args.get(1)),
                    };
                    let what = format!(
                        "contribution to {} at rank {rank}",
                        at.attr(&text_of(fact.args.get(2)))
                    );
                    if !related.iter().any(|(x, _)| *x == w) {
                        related.push((w, what));
                    }
                }
                queue.push_back((*ch, depth + 1));
            }
        }
    }
    (at, related)
}

/// Where the firings of fact `id` are written: its first rule or stated
/// fact that has a place.
pub fn written(res: &EvalResult, id: NodeId) -> Option<At> {
    let c = &res.circuit;
    let View::Fact { alts, .. } = c.view(id) else {
        return None;
    };
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        for ch in children {
            match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => {
                    if let Some(s) = rule_span(res, id) {
                        return Some(At::Span(s));
                    }
                }
                View::Leaf(Leaf::Base { span }) => return Some(At::Place(span.clone())),
                _ => {}
            }
        }
    }
    None
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => crate::partition::fmt_value(v),
        None => String::new(),
    }
}

/// A value as `--set` and a key value read it: a bool, an integer, else a
/// string.
pub fn value_of(raw: &str) -> Value {
    if raw == "true" {
        Value::Bool(true)
    } else if raw == "false" {
        Value::Bool(false)
    } else if let Ok(i) = raw.parse::<i64>() {
        Value::Int(i)
    } else {
        Value::Str(raw.to_string())
    }
}

/// The manifest under the program's own statements: a provider named
/// without a `source` takes the manifest's entry of that name.
pub fn with_manifest(cfg: &mut stack::Stack, m: &Manifest) {
    for p in &mut cfg.providers {
        if !p.contains('/')
            && let Some(src) = m.provider_source(p)
        {
            *p = src;
        }
    }
}

/// The keys of the program's own `input("k", v)` facts.
pub fn input_fact_keys(program: &Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Fact(a) if a.pred == "input" => match a.args.first() {
                Some(Term::Val(Value::Str(k))) => Some(k.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `inventory`, else `<world dir>/inventory.json` when a world fixture is
/// given and that file exists, else `default`.
fn inventory(explicit: &Option<PathBuf>, world: &Option<PathBuf>, default: &Path) -> PathBuf {
    if let Some(p) = explicit {
        return p.clone();
    }
    if let Some(w) = world {
        let dir = w.parent().unwrap_or_else(|| Path::new("."));
        let candidate = dir.join("inventory.json");
        if candidate.exists() {
            return candidate;
        }
    }
    default.to_path_buf()
}

/// The providers the program configures itself: the constant names of
/// its `provider_config(Name, Settings)` facts and rules.
fn provider_configs(program: &Program) -> BTreeSet<String> {
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Fact(a) => Some(a),
            Stmt::Rule(r) => Some(&r.head),
            _ => None,
        })
        .filter(|a| a.pred == "provider_config")
        .filter_map(|a| match a.args.first() {
            Some(Term::Val(Value::Str(n))) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// The types the program reads of the discovery inventory, when every read
/// names its type; `None`: all of them.
fn world_types(lowered: Option<&transform::Lowered>) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for st in &lowered?.program.statements {
        let body = match st {
            Stmt::Rule(r) => &r.body,
            _ => continue,
        };
        for l in body {
            let (Lit::Pos(a) | Lit::Not(a)) = l else {
                continue;
            };
            if !plugin::providers::INVENTORY
                .iter()
                .any(|(p, _)| *p == a.pred)
            {
                continue;
            }
            match a.args.first() {
                Some(Term::Val(Value::Str(t))) => {
                    out.insert(t.clone());
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

/// The types whose schema and catalog the run asks for: those the program
/// names, given its facts, and those state has objects of; `None`: all.
fn catalog_scope(
    program: &Program,
    given: &[Atom],
    discovered: &[Atom],
    st: &State,
) -> Option<BTreeSet<String>> {
    let lowered = transform::lower(program).ok()?;
    let facts: Vec<Atom> = given.iter().chain(discovered).cloned().collect();
    let mut named = schema::named_types(&lowered.program, &facts)?;
    named.extend(
        st.resources
            .keys()
            .chain(st.deposed.keys())
            .chain(st.uncertain.keys())
            .filter_map(|k| state::parse_key(k).map(|a| a.typ)),
    );
    Some(named)
}

/// Where the deployment of `stack` whose key is `seg` (`env=prod`, as
/// [`Instance::segment`] prints it) is: under its stack's location, or,
/// when the backend names the key (`s3("acme", "shop/{env}")`), where it
/// says.
pub fn deployment_location(
    root: &Path,
    stack: &str,
    backend: Option<&stack::Backend>,
    seg: Option<&str>,
) -> Location {
    let key: Vec<(String, String)> = seg
        .into_iter()
        .flat_map(|s| s.split(','))
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    match backend.map(|b| stack::keyed_backend(b, &key)) {
        Some((b, true)) => stack_location(root, stack, Some(&b)),
        Some((b, false)) => stack_location(root, stack, Some(&b)).child(seg),
        None => stack_location(root, stack, None).child(seg),
    }
}

/// Where a stack's deployments are: its backend's location, else its
/// directory under the state root.
pub fn stack_location(root: &Path, stack: &str, backend: Option<&stack::Backend>) -> Location {
    match backend {
        Some(stack::Backend::Local(dir)) => Location::Local(state::local_dir(root, dir)),
        Some(stack::Backend::S3(spec)) => Location::S3(spec.clone()),
        None => Location::Local(root.join(stack)),
    }
}
