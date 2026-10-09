//! The engine's side of the provider protocol: the stack's providers, and
//! the plan and the tick built from their per-resource calls.
//!
//! The engine owns everything but the cloud: it refreshes by Reading each
//! object state maps (`refresh`), resolves each desired document's refs
//! and nulls as far as the world allows, takes the Z-set `desired − world`
//! (`zset::deformation`), and asks the owning provider to Plan each
//! resource (the diff, and whether it replaces). An apply is a tick of
//! per-resource Apply calls in the order the executor picks (`Tick`); the
//! executor persists state after each.
//!
//! Computed values come only from Apply (proposal E §2.2, DR-11 revised).
//! At plan time a `ref(T, N, Attr)` to a schema-computed attribute is the
//! labeled null `?T/N#Attr` unless N already exists in the world, in which
//! case its computed value is resolved at refresh (round 0), so a
//! steady-state stack carries no nulls. At Apply the executor fills the
//! nulls in dependency order from what earlier Apply calls returned. A
//! sensitive computed value never leaves its provider: Read and Apply
//! return its label, `{"$secret": "T/N#Attr"}`, and output prints it
//! redacted.
//!
//! Which provider serves a type: the one whose schema declares it, else the
//! mock (which plays the types a program declares itself), else the first.
//! A plan whose resource has a type none of them declares is refused before
//! anything is planned ([`Providers::check_types`]).

use super::backend::{Call, CallError, Reply, Ticket};
use super::link::{Link, Retry};
use super::pb;
use super::policy::{self, Class};
use super::source::{self, Source};
use super::timed::timed_out;
use super::wire;
use crate::ast::{Atom, Term};
use crate::ir::{Address, Adopt, Resource};
use crate::provider::{self, Action, ActionKind, Change, Plan, get_path, remove_path, set_path};
use crate::schema::Schema;
use crate::secrets::held;
use crate::spell;
use crate::state::{self, State, StateEntry};
use crate::value::{NullClass, Value};
use crate::zset::{self, Lifecycle};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as Json, json};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `provider_expect_account(Name, Account)`: a provider's `use` block's
/// `expect_account`, what [`Providers::check_accounts`] reads.
pub const EXPECT_ACCOUNT: &str = "provider_expect_account";

/// What the engine hands every provider at Configure.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The world file (the mock's cloud).
    pub world: PathBuf,
    /// The discovery inventory file.
    pub inventory: PathBuf,
    /// `dform dev --chaos` specs, for the mock.
    pub chaos: Vec<String>,
    /// Where a provider may cache what it fetched (the k8s provider's
    /// OpenAPI document): `dform.state/cache/`. None: beside the world.
    pub cache: Option<PathBuf>,
    /// The providers, by name, the program configures itself
    /// (`provider_config(Name, Settings)`): each is told at Configure that
    /// its settings come later, and serves nothing until they do
    /// ([`Providers::configure_from`]).
    pub configured: BTreeSet<String>,
    /// The deployment the run is of (`app`, `app[env=prod]`): a provider
    /// may mark what it creates with it, to find an object whose Create
    /// answer was lost (`provider.created`).
    pub stack: String,
    /// The program's providers' `use`s, by the name each binds, so
    /// `provider_config(NAME, ..)` (what a block's settings lower to)
    /// finds its link by the block's name as well as by the provider's
    /// own; a name a `use .. as` gives is a link of its own (R-115).
    pub blocks: Vec<Block>,
    /// The secrets of other stacks the run reads (their outputs) that a
    /// provider holds, by the label of the run's null: an Apply document
    /// carries each as its label and where it is held (`provider::Held`),
    /// and the provider reads it there.
    pub held: BTreeMap<String, provider::Held>,
    /// The mock's world of each deployment a held secret names: its
    /// objects are there, not in this run's world.
    pub worlds: BTreeMap<String, PathBuf>,
    /// A key derived from the deployment's plan key (`Key::derive`, hex),
    /// for a provider to digest a secret it holds (`provider::Held`'s
    /// digest); `None` when the run has no plan key (a plain plan).
    pub digest_key: Option<String>,
    /// Each provider's call policy by its spec (`[providers.NAME]`'s
    /// `timeout`, `retries`, `backoff`, R-81); a provider not named has the
    /// default. The mock's one link takes the first of its schemas' that
    /// has one.
    pub policies: BTreeMap<String, super::policy::Policy>,
    /// What dform.toml grants each provider (`[providers.NAME] allow` and
    /// `credentials`, R-13b), by its spec; a provider not named has none.
    /// Passed to the launcher as each starts ([`Launch::plugin`]).
    pub grants: BTreeMap<String, super::host::Grants>,
    /// The run's reader of locations (R-153): a scheme a provider's
    /// manifest declares is read through it, by that provider.
    pub files: std::sync::Arc<crate::files::Files>,
    /// No provider reaches its credentials (`dform test`, R-188): every
    /// one started from an executable is told its settings come later,
    /// as a provider the program configures is, and none come.
    pub no_credentials: bool,
}

/// A provider's `use`: the name it binds, the provider it starts and that
/// provider's spec (as `specs` names it). `use ovh as ca` is `{ name: ca,
/// provider: ovh }`, a link of its own that serves `ovh`'s types as `ca.*`
/// (R-115); `use ovh` is `{ name: ovh, provider: ovh }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub spec: String,
    pub name: String,
    pub provider: String,
}

impl Block {
    /// Whether the `use` renames the provider: its link renames each call.
    fn renames(&self) -> bool {
        self.name != self.provider
    }
}

/// How a run reaches its providers: a backend (`plugin::backend`). The
/// CLI's starts each as a process and speaks gRPC to it (`dform-grpc`);
/// tests and benches link the mock in (`dform-mock`).
pub trait Launch {
    /// The mock provider, which plays every mock schema.
    fn mock(&self) -> Result<Link>;
    /// The provider executable at `exe`, with what dform.toml grants it.
    fn plugin(&self, exe: &Path, grants: &super::host::Grants) -> Result<Link>;
}

/// An object as Read, Apply or Import returns it: its configured
/// attributes and its computed values (a secret one as its label).
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub attrs: Json,
    pub computed: Json,
}

/// Objects by `key(type, remote id)`.
type World = BTreeMap<String, Object>;

/// A provider's Plan of one resource: its changes, and whether they
/// replace it.
type Planned = (Vec<Change>, bool);
/// One Plan call: the address, its remote id (empty for a create), the
/// world's document and the desired one.
type Ask = (Address, String, Option<Json>, Option<Json>);

fn key(typ: &str, remote: &str) -> String {
    format!("{typ}::{remote}")
}

/// The Query predicate a provider with the `managed` capability answers
/// with Type, Name and Key bound: `(Type, Name, Key, RemoteId)` for the
/// object an Apply Create or Replace of address Name with idempotency key
/// Key made, if it made one.
pub const CREATED: &str = "provider.created";
/// The inventory relations a provider with the `inventory` capability
/// answers with every argument free, and their arities.
pub const INVENTORY: [(&str, usize); 3] = [
    ("cloud_exists", 2),
    ("cloud_attr", 4),
    ("cloud_computed", 4),
];

pub struct Providers {
    links: Vec<RefCell<Link>>,
    /// Each provider's name, as its handshake gave it.
    names: Vec<String>,
    /// The providers' Schema answers: loaded at start, or, for a run that
    /// scopes them to the types it names, by [`Providers::load_schema`].
    loaded: OnceCell<Loaded>,
    /// The provider for a type nobody declares: the mock, else the first.
    fallback: usize,
    /// The last refresh and the objects state mapped for it: one run reads
    /// the world once between two writes, so every consumer of a refresh
    /// (round 0, the plan, the executor's comparison) sees the same Reads.
    refreshed: RefCell<Option<(BTreeSet<String>, World)>>,
    /// Import answers since the last write.
    imported: RefCell<BTreeMap<String, Option<Object>>>,
    /// What the providers said during the last apply, for the CLI to print.
    notes: RefCell<Vec<String>>,
    /// Each link's Configure document, as the engine first sent it.
    bases: Vec<Json>,
    /// Each link's provider and the namespace of its types, when the
    /// program names it: what shares a Schema answer between the names a
    /// provider has (R-115).
    shares: Vec<Option<Share>>,
    /// The links whose settings the program gives and has not yet (a
    /// null, or not evaluated): they serve no state entry.
    awaiting: RefCell<BTreeSet<usize>>,
    /// The links the program configures itself: `awaiting` as it started.
    by_program: BTreeSet<usize>,
    /// The settings each link was last configured with from the program.
    settings: RefCell<BTreeMap<usize, Json>>,
    /// The kinds a link was asked to serve again (a CRD made at the last
    /// tick defines them, R-126): each once a run.
    relearned: RefCell<BTreeSet<String>>,
    /// A program's provider block name -> its link ([`Config::blocks`]).
    blocks: BTreeMap<String, usize>,
    /// The mock schemas' blocks: name -> the schema (the mock's one link
    /// plays every one).
    mock_blocks: BTreeMap<String, String>,
    /// The account each link's last Configure reported, if it tells.
    accounts: RefCell<BTreeMap<usize, String>>,
    /// A link whose reported account is a secret revealed into its
    /// settings: that secret's label, which a message prints instead.
    secret_accounts: RefCell<BTreeMap<usize, String>>,
    /// Where each secret the run knows of is held, by its label:
    /// [`Config::held`], an extern's secret column as its provider answered
    /// it ([`Providers::query_extern`]).
    held: RefCell<BTreeMap<String, provider::Held>>,
    /// [`Config::digest_key`]: what a sensitive leaf of a world document
    /// dform keeps is digested with ([`Providers::stored`]).
    digest_key: Option<crate::zset::file::Key>,
    /// What the last plan proved unchanged without the master
    /// ([`Providers::proven`]).
    proven: RefCell<BTreeMap<Address, Vec<String>>>,
}

/// What a resource waits on before its provider plans it
/// ([`Providers::waits`]), by the provider's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderWait {
    /// The provider's settings, which the program gives and has not yet.
    Settings(String),
    /// The provider's schema of the type: a kind its cluster serves.
    Schema(String),
}

/// What one link starts ([`Providers::start_deferred`]).
struct Start {
    spec: String,
    /// The names the program's `use`s give it.
    names: Vec<String>,
    /// The `use .. as` it serves under its name (R-115).
    rename: Option<Block>,
}

impl Start {
    /// What each link starts: a spec, the names its `use`s bind, and how it
    /// renames the provider's types (`use ovh as ca`: a link of its own,
    /// R-115). Every mock schema no `use` renames is played by one mock
    /// link, the first.
    fn of(specs: &[String], cfg: &Config) -> Vec<Start> {
        let mut starts: Vec<Start> = Vec::new();
        for s in specs {
            let named: Vec<&Block> = cfg.blocks.iter().filter(|b| b.spec == *s).collect();
            let plain: Vec<String> = named
                .iter()
                .filter(|b| !b.renames())
                .map(|b| b.name.clone())
                .collect();
            if !plain.is_empty() || named.iter().all(|b| !b.renames()) {
                starts.push(Start {
                    spec: s.clone(),
                    names: plain,
                    rename: None,
                });
            }
            for b in named.into_iter().filter(|b| b.renames()) {
                starts.push(Start {
                    spec: s.clone(),
                    names: vec![b.name.clone()],
                    rename: Some(b.clone()),
                });
            }
        }
        starts
    }

    /// Played by the one mock link: a mock schema no `use` renames.
    fn shared(&self) -> bool {
        self.rename.is_none() && matches!(source::resolve(&self.spec), Source::Mock(_))
    }
}

/// Providers being started: their links, each link's base configuration,
/// the links awaiting the program's settings, the accounts they answered,
/// each block's link, and how a link's schema is shared.
struct Starting<'a> {
    launch: &'a dyn Launch,
    cfg: &'a Config,
    starts: &'a [Start],
    /// The mock schemas the mock link plays.
    mocks: Vec<String>,
    /// The configuration every link starts from.
    base: Json,
    links: Vec<Link>,
    bases: Vec<Json>,
    awaiting: BTreeSet<usize>,
    accounts: BTreeMap<usize, String>,
    blocks: BTreeMap<String, usize>,
    mock_blocks: BTreeMap<String, String>,
    shares: Vec<Option<Share>>,
}

impl<'a> Starting<'a> {
    fn new(launch: &'a dyn Launch, cfg: &'a Config, starts: &'a [Start]) -> Starting<'a> {
        let mocks: Vec<String> = starts
            .iter()
            .filter(|st| st.shared())
            .filter_map(|st| match source::resolve(&st.spec) {
                Source::Mock(m) => Some(m),
                Source::Plugin(_) => None,
            })
            .collect();
        Starting {
            launch,
            cfg,
            starts,
            mocks,
            base: base_config(cfg),
            links: Vec::new(),
            bases: Vec::new(),
            awaiting: BTreeSet::new(),
            accounts: BTreeMap::new(),
            blocks: BTreeMap::new(),
            mock_blocks: BTreeMap::new(),
            shares: Vec::new(),
        }
    }

    /// Each block's link: the mock is the first when there is one, the
    /// others follow in order. The starts that are links of their own.
    fn blocks(&mut self) -> Vec<&'a Start> {
        let mut next = usize::from(!self.mocks.is_empty());
        let mut own = Vec::new();
        for st in self.starts {
            if st.shared() {
                let Source::Mock(m) = source::resolve(&st.spec) else {
                    unreachable!("a mock");
                };
                self.blocks.extend(st.names.iter().map(|b| (b.clone(), 0)));
                self.mock_blocks
                    .extend(st.names.iter().map(|b| (b.clone(), m.clone())));
            } else {
                self.blocks
                    .extend(st.names.iter().map(|b| (b.clone(), next)));
                own.push(st);
                next += 1;
            }
        }
        own
    }

    /// Whether the program configures link `i` (named `name`) by a block.
    fn by_block(&self, i: usize, name: &str) -> bool {
        let cfg = self.cfg;
        cfg.configured.contains(name)
            || self
                .blocks
                .iter()
                .any(|(b, &j)| j == i && cfg.configured.contains(b))
    }

    fn policy_of(&self, s: &String) -> Option<super::policy::Policy> {
        self.cfg.policies.get(s).copied()
    }

    /// The mock link, playing every shared mock schema. A mock schema the
    /// program configures by its block serves nothing until the settings
    /// arrive, as a plugin would; a mock playing several providers is
    /// configured by none of them ([`Providers::link_for`]).
    fn start_mock(&mut self) -> Result<()> {
        let launch = self.launch;
        let mut link = crate::timing::time(|| "provider mock started".into(), || launch.mock())?;
        if let Some(p) = self
            .starts
            .iter()
            .filter(|st| st.shared())
            .find_map(|st| self.policy_of(&st.spec))
        {
            link.set_policy(p);
        }
        let mut config = self.base.clone();
        config["schemas"] = json!(self.mocks);
        if self.mocks.len() == 1
            && self
                .blocks
                .iter()
                .any(|(b, &j)| j == 0 && self.cfg.configured.contains(b))
        {
            config["deferred"] = json!(true);
            self.awaiting.insert(0);
        }
        self.accounts
            .extend(configure(&mut link, config.clone())?.map(|a| (0, a)));
        self.links.push(link);
        self.bases.push(config);
        self.shares.push(None);
        Ok(())
    }

    /// A link of its own: a plugin, or a mock under a `use .. as`.
    fn start_own(&mut self, st: &Start) -> Result<()> {
        let (launch, cfg) = (self.launch, self.cfg);
        let path = |p: &PathBuf| json!(p.display().to_string());
        let mock = match source::resolve(&st.spec) {
            Source::Mock(m) => Some(m),
            Source::Plugin(_) => None,
        };
        let shown = match source::resolve(&st.spec) {
            Source::Plugin(p) => p.display().to_string(),
            Source::Mock(m) => m,
        };
        let mut link = match (&mock, source::resolve(&st.spec)) {
            (Some(_), _) => {
                crate::timing::time(|| "provider mock started".into(), || launch.mock())?
            }
            (None, Source::Plugin(p)) => crate::timing::time(
                || format!("provider {} started (spawn and handshake)", p.display()),
                || {
                    let mut none = super::host::Grants::none_for(&p);
                    none.files = crate::files::Shared(Some(cfg.files.clone()));
                    launch.plugin(&p, cfg.grants.get(&st.spec).unwrap_or(&none))
                },
            )?,
            (None, Source::Mock(_)) => unreachable!("a plugin"),
        };
        if let Some(policy) = self.policy_of(&st.spec) {
            link.set_policy(policy);
        }
        // The provider's types under the `use`'s name, renamed at the link
        // only: the provider never learns of it.
        if let Some(b) = &st.rename {
            link.rename(wire::Rename::new(b.name.clone(), b.provider.clone()));
        }
        let mut base = self.base.clone();
        let i = self.links.len();
        // A world file is one process's: the mock keeps its world in memory
        // and writes it whole. A second process (the mock as a plugin
        // beside it, a provider under a second name) keeps its own, as a
        // second cloud would.
        if i > 0 || st.rename.is_some() {
            let name = st.rename.as_ref().map_or(&link.name, |b| &b.name);
            base["world"] = path(&cfg.world.with_extension(format!("{name}.json")));
        }
        let mut config = base.clone();
        if let Some(m) = &mock {
            config["schemas"] = json!([m]);
        }
        if self.by_block(i, &link.name) || (cfg.no_credentials && mock.is_none()) {
            config["deferred"] = json!(true);
            self.awaiting.insert(i);
        }
        let account = configure(&mut link, config.clone())
            .with_context(|| format!("configure provider {shown}"))?;
        self.accounts.extend(account.map(|a| (i, a)));
        self.links.push(link);
        self.bases
            .push(if mock.is_some() { config } else { base.clone() });
        // Its schema is the provider's, under the name: asked once of the
        // provider and shared by each name it has.
        self.shares.push(st.names.first().map(|n| Share {
            spec: st.spec.clone(),
            namespace: n.clone(),
            of: st.rename.as_ref().map(|b| b.provider.clone()),
        }));
        Ok(())
    }

    /// The providers, started: the location schemes a provider's manifest
    /// declares are read by it, through the run's reader (R-153).
    fn into_providers(self) -> Result<Providers> {
        let cfg = self.cfg;
        if self.links.is_empty() {
            bail!("no providers");
        }
        for link in &self.links {
            if let Some(r) = &link.reader {
                for s in &link.schemes {
                    cfg.files.declare(s, &link.name, r.clone());
                }
            }
        }
        let p = Providers::deferred(self.links);
        Ok(Providers {
            bases: self.bases,
            shares: self.shares,
            by_program: self.awaiting.clone(),
            awaiting: RefCell::new(self.awaiting),
            blocks: self.blocks,
            mock_blocks: self.mock_blocks,
            accounts: RefCell::new(self.accounts),
            held: RefCell::new(cfg.held.clone()),
            relearned: RefCell::new(BTreeSet::new()),
            digest_key: cfg
                .digest_key
                .as_deref()
                .and_then(crate::zset::file::Key::from_hex),
            ..p
        })
    }
}

/// The configuration every link starts from: its world, inventory, chaos,
/// stack, cache, digest key and the worlds of other deployments it holds.
fn base_config(cfg: &Config) -> Json {
    let path = |p: &PathBuf| json!(p.display().to_string());
    let base = json!({
        "world": path(&cfg.world),
        "inventory": path(&cfg.inventory),
        "chaos": cfg.chaos,
        "stack": cfg.stack,
    });
    let mut base = base;
    if let Some(c) = &cfg.cache {
        base["cache"] = path(c);
    }
    if let Some(k) = &cfg.digest_key {
        base["digest_key"] = json!(k);
    }
    if !cfg.worlds.is_empty() {
        base["worlds"] = cfg
            .worlds
            .iter()
            .map(|(d, w)| (d.clone(), path(w)))
            .collect();
    }
    base
}

/// A link's schema is its provider's, the types in `namespace`: one
/// Schema answer of a spec serves each link of it ([`Providers::load_schema`]).
struct Share {
    spec: String,
    namespace: String,
    /// The provider a `use .. as` renames (R-115).
    of: Option<String>,
}

/// What the providers' Schema calls answered.
struct Loaded {
    /// The schema as loaded, then each extension of it in the run.
    schema: Generation,
    /// The catalog facts of the types learned so, for the evaluation.
    learned: RefCell<Vec<Atom>>,
    /// Type -> the provider serving it.
    owner: BTreeMap<String, usize>,
    /// Extern predicate -> the provider answering it.
    externs: BTreeMap<String, usize>,
    /// The types asked for; `None`: every one.
    scope: Option<BTreeSet<String>>,
}

/// The providers' schema, and what a provider configured from the
/// program's settings later serves beyond it ([`Providers::learn`]): each
/// extension a new generation after the last, none changed once set, so a
/// schema read earlier in the run stays as it was read.
struct Generation {
    schema: Schema,
    next: OnceCell<Box<Generation>>,
}

impl Generation {
    fn new(schema: Schema) -> Generation {
        Generation {
            schema,
            next: OnceCell::new(),
        }
    }

    fn latest(&self) -> &Generation {
        let mut g = self;
        while let Some(n) = g.next.get() {
            g = n;
        }
        g
    }
}

/// Configure a link: the account its credentials reach, if it tells;
/// what it notes of its configuration (a credential about to expire) on
/// stderr.
fn configure(link: &mut Link, config: Json) -> Result<Option<String>> {
    let config = Some(wire::doc(&config));
    let r: pb::ConfigureResponse = link.call(pb::ConfigureRequest { config })?;
    for n in &r.notes {
        eprintln!("{n}");
    }
    Ok(r.account)
}

impl Providers {
    /// Start the providers `specs` name (`--provider` or the providers'
    /// `use`s; none is the mock's `fake` schema): every mock schema in
    /// one mock provider, every plugin in its own process.
    pub fn start(launch: &dyn Launch, specs: &[String], cfg: &Config) -> Result<Providers> {
        let p = Self::start_deferred(launch, specs, cfg)?;
        p.load_schema(None)?;
        Ok(p)
    }

    /// No provider: what a program naming only built-in providers runs
    /// over (R-26), its schema not loaded yet ([`Providers::load_schema`]:
    /// empty). Asking it anything else is the no-provider error.
    pub fn none() -> Providers {
        Self::deferred(Vec::new())
    }

    /// `start`, without asking for the schema yet: the caller asks with
    /// [`Providers::load_schema`] once it knows the types it names, before
    /// anything reads the schema.
    pub fn start_deferred(
        launch: &dyn Launch,
        specs: &[String],
        cfg: &Config,
    ) -> Result<Providers> {
        let specs: Vec<String> = if specs.is_empty() {
            vec!["fake".to_string()]
        } else {
            specs.to_vec()
        };
        let starts = Start::of(&specs, cfg);
        let mut s = Starting::new(launch, cfg, &starts);
        let own = s.blocks();
        if !s.mocks.is_empty() {
            s.start_mock()?;
        }
        for st in own {
            s.start_own(st)?;
        }
        s.into_providers()
    }

    /// Configure each provider the program configures
    /// (`provider_config(Name, Settings)` in `facts`) whose settings are
    /// known now and changed: Configure again with them as `settings`.
    /// Settings holding a null (a cluster not created yet) wait. A secret
    /// another provider holds (a resource's sensitive computed attribute,
    /// another stack's held output, an extern's secret column) is revealed
    /// by that provider into this call (R-45, the `Reveal` call): once the
    /// object that holds it exists. The providers configured, by the name
    /// the program gives them: then what was read is read again. The
    /// settings go to the provider in that call and are kept here only to
    /// tell a change, a revealed secret as its label and where it is held
    /// (never written, printed or digested: a kubeconfig may be among
    /// them).
    pub fn configure_from<'a>(
        &self,
        facts: impl IntoIterator<Item = &'a Atom>,
    ) -> Result<Vec<String>> {
        let facts: Vec<&Atom> = facts.into_iter().collect();
        // The object each address maps to, for a secret its attribute holds.
        let identity: BTreeMap<(String, String), String> = facts
            .iter()
            .filter(|a| a.pred == "identity")
            .filter_map(|a| match a.args.as_slice() {
                [
                    Term::Val(Value::Str(t)),
                    Term::Val(Value::Str(n)),
                    Term::Val(Value::Str(r)),
                ] => Some(((t.clone(), n.clone()), r.clone())),
                _ => None,
            })
            .collect();
        let mut changed = Vec::new();
        for a in facts.iter().filter(|a| a.pred == "provider_config") {
            let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() else {
                continue;
            };
            let Some(i) = self.link_for(name) else {
                continue;
            };
            // What the settings are kept as, and what goes to the provider.
            let Some(kept) = self.settings_of(v, &identity) else {
                continue;
            };
            // A stand-in is no setting (R-164): the provider waits for a
            // run that holds the master.
            if crate::secrets::standin::carries(&kept) {
                continue;
            }
            if self.settings.borrow().get(&i) == Some(&kept) {
                continue;
            }
            let mut shown = Vec::new();
            let settings = self
                .revealed(v, &identity, &mut shown)
                .with_context(|| format!("configure provider {name}"))?;
            let mut config = self.bases.get(i).cloned().unwrap_or_else(|| json!({}));
            config["settings"] = settings;
            let account = configure(&mut self.links[i].borrow_mut(), config)
                .with_context(|| format!("configure provider {name} from provider_config"))?;
            // An account that is a revealed secret prints as its label.
            let secret = account.as_ref().and_then(|a| {
                shown
                    .iter()
                    .find(|(_, s)| s.expose() == a.as_bytes())
                    .map(|(l, _)| l.clone())
            });
            match secret {
                Some(l) => self.secret_accounts.borrow_mut().insert(i, l),
                None => self.secret_accounts.borrow_mut().remove(&i),
            };
            let mut accounts = self.accounts.borrow_mut();
            match account {
                Some(a) => accounts.insert(i, a),
                None => accounts.remove(&i),
            };
            drop(accounts);
            self.settings.borrow_mut().insert(i, kept);
            self.awaiting.borrow_mut().remove(&i);
            if self.loaded.get().is_some() {
                self.learn(i)
                    .with_context(|| format!("the schema of provider {name}, configured"))?;
            }
            changed.push(name.clone());
        }
        if !changed.is_empty() {
            self.invalidate();
        }
        Ok(changed)
    }

    /// Where the secret labeled `label` is held, and the provider that
    /// holds it: an extern's or another stack's, as it was answered; a
    /// resource's sensitive attribute (`T/N#P`), by the provider serving
    /// `T`, once the object exists (`identity`).
    fn holder(
        &self,
        label: &str,
        identity: &BTreeMap<(String, String), String>,
    ) -> Option<(usize, provider::Held)> {
        if let Some(h) = self.held.borrow().get(label) {
            return Some((self.link_named(&h.provider)?, h.clone()));
        }
        let (typ, name, path) = crate::value::null_parts(label)?;
        let remote = identity.get(&(typ.clone(), name))?;
        let i = self.route(&typ);
        let deployment = self.bases.get(i)?.get("stack")?.as_str()?.to_string();
        Some((
            i,
            provider::Held {
                provider: self.links[i].borrow().name.clone(),
                deployment,
                typ,
                remote: remote.clone(),
                path,
                digest: String::new(),
            },
        ))
    }

    /// Settings as they are kept to tell a change: every value as it is,
    /// a secret as its label and where it is held. `None` while one is not
    /// known yet: an open null, a secret no provider holds yet.
    fn settings_of(
        &self,
        v: &Value,
        identity: &BTreeMap<(String, String), String>,
    ) -> Option<Json> {
        Some(match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } => provider::held_json(label, &self.holder(label, identity)?.1),
            // A template over held secrets as it is, once each has its
            // holder (R-218).
            Value::Str(t) if held::carries(t) => {
                for l in held::labels(t) {
                    self.holder(&l, identity)?;
                }
                Json::String(t.clone())
            }
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.settings_of(x, identity))
                    .collect::<Option<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Some((k.clone(), self.settings_of(x, identity)?)))
                    .collect::<Option<_>>()?,
            ),
            other => known_json(other)?,
        })
    }

    /// Settings as Configure takes them: each secret revealed by the
    /// provider that holds it, its bytes in this document only (and in
    /// `shown`, by its label, zeroed when dropped). Only called with
    /// settings [`Providers::settings_of`] knows.
    fn revealed(
        &self,
        v: &Value,
        identity: &BTreeMap<(String, String), String>,
        shown: &mut Vec<(String, super::credentials::Secret)>,
    ) -> Result<Json> {
        Ok(match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } => json!(self.revealed_text(label, identity, shown)?),
            Value::Str(t) if held::carries(t) => {
                json!(held::fill(t, |l| self.revealed_text(l, identity, shown))?)
            }
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.revealed(x, identity, shown))
                    .collect::<Result<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Ok((k.clone(), self.revealed(x, identity, shown)?)))
                    .collect::<Result<_>>()?,
            ),
            other => known_json(other).ok_or_else(|| anyhow!("a setting is not known yet"))?,
        })
    }

    /// The secret labeled `label` as text, revealed by the provider that
    /// holds it (and kept in `shown`, by its label, zeroed when dropped).
    fn revealed_text(
        &self,
        label: &str,
        identity: &BTreeMap<(String, String), String>,
        shown: &mut Vec<(String, super::credentials::Secret)>,
    ) -> Result<String> {
        let (i, held) = self.holder(label, identity).ok_or_else(|| {
            anyhow!(
                "no object of this deployment holds the secret {} (it is not made, or it is \
                 another deployment's)",
                crate::ir::label(label)
            )
        })?;
        let bytes = self.reveal(i, &held, label)?;
        let text = std::str::from_utf8(bytes.expose())
            .map_err(|_| {
                anyhow!(
                    "the secret {} that provider {} revealed is not text",
                    crate::ir::label(label),
                    held.provider
                )
            })?
            .to_string();
        shown.push((label.to_string(), bytes));
        Ok(text)
    }

    /// The object each address of `state` maps to, for a secret its
    /// attribute holds ([`Providers::holder`]).
    fn identities(&self, state: &State) -> BTreeMap<(String, String), String> {
        self.entries(&state.resources)
            .map(|(a, e)| ((a.typ, a.name), e.remote.clone()))
            .collect()
    }

    /// `doc`, an Apply call's document for `addr`, with each secret another
    /// provider holds revealed into it (R-218, as into a Configure): at a
    /// path the schema marks sensitive, one written whole that the writing
    /// provider does not hold itself (one it holds it materializes, as it
    /// always has), and every one inside a template. Each is revealed by
    /// the provider that holds it under this run's lease; the bytes are in
    /// this call only. A reveal that does not happen is the call's error,
    /// naming the attribute.
    fn reveal_into(&self, addr: &Address, doc: &mut Json, state: &State) -> Result<()> {
        if self.revealed_attrs(&addr.typ, doc).is_empty() {
            return Ok(());
        }
        let identity = self.identities(state);
        let writer = self.route(&addr.typ);
        self.reveal_at(addr, writer, &identity, doc, "", "")
    }

    fn reveal_at(
        &self,
        addr: &Address,
        writer: usize,
        identity: &BTreeMap<(String, String), String>,
        v: &mut Json,
        path: &str,
        norm: &str,
    ) -> Result<()> {
        let sensitive = !norm.is_empty() && self.schema().is_sensitive(&addr.typ, norm);
        let text = |label: &str| {
            self.revealed_text(label, identity, &mut Vec::new())
                .map_err(|e| unrevealed(addr, path, label, &e))
        };
        if let Some((key, label)) = provider::marker(v) {
            if sensitive && key == provider::SECRET_KEY && self.holder_link(label) != Some(writer) {
                *v = Json::String(text(label)?);
            }
            return Ok(());
        }
        match v {
            Json::String(t) if sensitive && held::carries(t) => *t = held::fill(t, text)?,
            Json::Object(m) => {
                for (k, x) in m.iter_mut() {
                    let (p, n) = (crate::ir::path_join(path, k), crate::ir::path_join(norm, k));
                    self.reveal_at(addr, writer, identity, x, &p, &n)?;
                }
            }
            Json::Array(xs) => {
                for (i, x) in xs.iter_mut().enumerate() {
                    self.reveal_at(addr, writer, identity, x, &format!("{path}[{i}]"), norm)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The link of the provider that holds the secret labeled `label`:
    /// an extern's or another stack's, as it was answered; a resource's
    /// sensitive attribute, the one serving its type.
    fn holder_link(&self, label: &str) -> Option<usize> {
        if let Some(h) = self.held.borrow().get(label) {
            return self.link_named(&h.provider);
        }
        let (typ, _, _) = crate::value::null_parts(label)?;
        Some(self.route(&typ))
    }

    /// The attributes of a document for `typ` that [`Providers::reveal_into`]
    /// reveals a secret into, by path: what its provider is sent is not
    /// what dform has, so each is compared as a write-only one is, by the
    /// digest state keeps of what was sent (R-106).
    fn revealed_attrs(&self, typ: &str, doc: &Json) -> Vec<String> {
        let writer = self.route(typ);
        let reveals = |v: &Json, norm: &str| {
            let mut found = false;
            self.reveals(typ, writer, v, norm, &mut found);
            found
        };
        doc.as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| reveals(v, &crate::ir::path_join("", k)))
            .map(|(k, _)| crate::ir::path_join("", k))
            .collect()
    }

    fn reveals(&self, typ: &str, writer: usize, v: &Json, norm: &str, found: &mut bool) {
        let sensitive = || self.schema().is_sensitive(typ, norm);
        match v {
            _ if *found => {}
            v if let Some((key, label)) = provider::marker(v) => {
                *found = key == provider::SECRET_KEY
                    && self.holder_link(label) != Some(writer)
                    && sensitive();
            }
            Json::String(t) => *found = held::carries(t) && sensitive(),
            Json::Object(m) => {
                for (k, x) in m {
                    self.reveals(typ, writer, x, &crate::ir::path_join(norm, k), found);
                }
            }
            Json::Array(xs) => {
                for x in xs {
                    self.reveals(typ, writer, x, norm, found);
                }
            }
            _ => {}
        }
    }

    /// The secret `held` names, from provider `i`, which holds it: under
    /// this run's holder identity (`WHO pid PID`, as the deployment's lease
    /// names its holder), which a provider refuses a reveal without.
    fn reveal(
        &self,
        i: usize,
        held: &provider::Held,
        label: &str,
    ) -> Result<super::credentials::Secret> {
        let req = pb::RevealRequest {
            held: Some(pb::Held {
                provider: held.provider.clone(),
                deployment: held.deployment.clone(),
                r#type: held.typ.clone(),
                remote: held.remote.clone(),
                path: held.path.clone(),
                digest: held.digest.clone(),
            }),
            lease: format!("{} pid {}", crate::audit::who(), std::process::id()),
        };
        let r: pb::RevealResponse = self.links[i].borrow_mut().call(req).with_context(|| {
            format!(
                "provider {} reveals the secret {}",
                held.provider,
                crate::ir::label(label)
            )
        })?;
        Ok(super::credentials::Secret::new(r.value))
    }

    /// The kinds of `types` (none in any schema, their namespace's
    /// provider one the program configures or a cluster's, [`Providers::reached`]) a
    /// CRD the program made defines (R-126): each such provider is
    /// configured again with the settings it has (none, one the environment
    /// configures), told the kinds (`kinds`, which it may wait a moment for the
    /// cluster to serve), and what it serves now learned. Each kind is
    /// asked for once a run. Whether the schema learned any.
    pub fn relearn(&self, types: &BTreeSet<String>) -> Result<bool> {
        let mut by_link: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for t in types {
            if self.relearned.borrow().contains(t) || self.schema().knows_type(t) {
                continue;
            }
            // A provider the program configures, with the settings it
            // has; one the environment configures, as it was.
            let Some(i) = self.reached(t) else {
                continue;
            };
            if self.settings.borrow().contains_key(&i) || !self.by_program.contains(&i) {
                by_link.entry(i).or_default().push(t.clone());
            }
        }
        let mut grew = false;
        for (i, kinds) in by_link {
            self.relearned.borrow_mut().extend(kinds.iter().cloned());
            let mut config = self.bases.get(i).cloned().unwrap_or_else(|| json!({}));
            if let Some(settings) = self.settings.borrow().get(&i) {
                config["settings"] = settings.clone();
            }
            config["kinds"] = json!(kinds);
            configure(&mut self.links[i].borrow_mut(), config)
                .with_context(|| format!("configure provider {} again", self.names[i]))?;
            if self.loaded.get().is_some() {
                self.learn(i)
                    .with_context(|| format!("the schema of provider {}", self.names[i]))?;
            }
            grew |= kinds.iter().any(|t| self.schema().knows_type(t));
        }
        if grew {
            self.invalidate();
        }
        Ok(grew)
    }

    /// Whether `typ` is a kind no schema has of a provider the program
    /// configured, its settings arrived (R-126): the provider learned
    /// what its cluster serves then ([`Providers::learn`]), so this is a
    /// kind the cluster does not serve, which a plan would send it anyway
    /// ([`Providers::route`]).
    pub fn unserved(&self, typ: &str) -> bool {
        !self.loaded().owner.contains_key(typ)
            && !self.schema().knows_type(typ)
            && self.reached(typ).is_some()
    }

    fn deferred(links: Vec<Link>) -> Providers {
        Providers {
            names: links.iter().map(|l| l.name.clone()).collect(),
            links: links.into_iter().map(RefCell::new).collect(),
            loaded: OnceCell::new(),
            fallback: 0,
            refreshed: RefCell::new(None),
            imported: RefCell::new(BTreeMap::new()),
            notes: RefCell::new(Vec::new()),
            bases: Vec::new(),
            shares: Vec::new(),
            awaiting: RefCell::new(BTreeSet::new()),
            by_program: BTreeSet::new(),
            settings: RefCell::new(BTreeMap::new()),
            blocks: BTreeMap::new(),
            mock_blocks: BTreeMap::new(),
            accounts: RefCell::new(BTreeMap::new()),
            secret_accounts: RefCell::new(BTreeMap::new()),
            held: RefCell::new(BTreeMap::new()),
            relearned: RefCell::new(BTreeSet::new()),
            digest_key: None,
            proven: RefCell::new(BTreeMap::new()),
        }
    }

    /// The types and the externs the provider `name` serves: a mock
    /// schema's own (the mock plays several on one link), else its link's.
    fn served(&self, name: &str, i: usize) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        if let Some(spec) = self.mock_blocks.get(name) {
            let s = crate::schema::load_provider(spec)?;
            let types = s
                .provider_of
                .keys()
                .chain(s.attrs.keys().map(|(t, _)| t))
                .cloned()
                .collect();
            let externs = crate::externs::load_answers(std::slice::from_ref(spec))?
                .into_iter()
                .map(|a| a.pred)
                .collect();
            return Ok((types, externs));
        }
        let l = self.loaded();
        let of = |m: &BTreeMap<String, usize>| {
            m.iter()
                .filter(|&(_, &j)| j == i)
                .map(|(k, _)| k.clone())
                .collect()
        };
        Ok((of(&l.owner), of(&l.externs)))
    }

    /// The link a program names: by the name of the `use` that selects
    /// it, else by the provider's own name. A mock playing several
    /// providers on one link (`use google` and `use k8s` on the
    /// built-in schemas) is none of theirs: one's settings would
    /// configure the others' types too, so it plays them unconfigured.
    fn link_for(&self, name: &str) -> Option<usize> {
        let played = self
            .bases
            .first()
            .and_then(|b| b.get("schemas"))
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
        if self.mock_blocks.contains_key(name) && played > 1 {
            return self.link_named(name);
        }
        // A `use`'s name first: two names of one provider (R-115) are two
        // links its handshake names alike.
        self.blocks
            .get(name)
            .copied()
            .or_else(|| self.link_named(name))
    }

    /// A provider's configuration is known before it answers anything: a
    /// `provider_config(N, ..)` rule that reads, through any chain of
    /// rules, an extern N answers or an attribute of a type N serves (its
    /// computed values among them) is a compile error naming the cycle.
    /// Reading another provider's is the lazy configuration: its settings
    /// wait on that provider's nulls. `local`: the externs the engine
    /// answers itself (tables, `file.*`, `env_var`).
    pub fn check_configuration(
        &self,
        program: &crate::ast::Program,
        externs: &[crate::ast::ExternFn],
        local: impl Fn(&str) -> bool,
    ) -> Result<()> {
        use crate::ast::{Lit, Stmt};
        let rules: Vec<(&Atom, &[Lit])> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Rule(r) => Some((&r.head, r.body.as_slice())),
                Stmt::Fact(a) => Some((a, &[][..])),
                _ => None,
            })
            .collect();
        let declared: BTreeSet<&str> = externs.iter().map(|f| f.name.as_str()).collect();
        let text = |t: &Term| match t {
            Term::Val(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let mut diags = Vec::new();
        for (head, body) in rules.iter().filter(|(h, _)| h.pred == "provider_config") {
            let Some(name) = head.args.first().and_then(text) else {
                continue;
            };
            let Some(i) = self.link_for(&name) else {
                continue;
            };
            let (types, answers) = self.served(&name, i)?;
            // What a body atom reads that the provider serves, if it does.
            let served = |a: &Atom| -> Option<String> {
                if declared.contains(a.pred.as_str()) && !local(&a.pred) {
                    return answers
                        .contains(&a.pred)
                        .then(|| format!("extern {}", a.pred));
                }
                let typed = matches!(
                    a.pred.as_str(),
                    "attr" | "cloud_attr" | "cloud_computed" | "cloud_exists"
                );
                let t = a.args.first().and_then(text).filter(|_| typed)?;
                let what = match (a.args.get(1).and_then(text), a.args.get(2).and_then(text)) {
                    (Some(n), Some(p)) if a.pred != "cloud_exists" => Address {
                        typ: t.clone(),
                        name: n,
                    }
                    .attr(p.trim_start_matches('.')),
                    (Some(n), _) => Address {
                        typ: t.clone(),
                        name: n,
                    }
                    .to_string(),
                    _ => t.clone(),
                };
                types.contains(&t).then_some(what)
            };
            // A reference in a head: `ref(T, N, P)` of a type it serves.
            fn reference(t: &Term, types: &BTreeSet<String>) -> Option<String> {
                match t {
                    Term::Val(Value::Ref { typ, name, attr }) if types.contains(typ) => Some(
                        Address {
                            typ: typ.clone(),
                            name: name.clone(),
                        }
                        .attr(attr),
                    ),
                    Term::Func { name, args } if name == crate::ir::REF => match args.as_slice() {
                        [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), p]
                            if types.contains(t) =>
                        {
                            let a = Address {
                                typ: t.clone(),
                                name: n.clone(),
                            };
                            Some(match p {
                                Term::Val(Value::Str(p)) => a.attr(p.trim_start_matches('.')),
                                _ => format!("{a}.."),
                            })
                        }
                        _ => None,
                    },
                    Term::Func { args, .. } | Term::List(args) => {
                        args.iter().find_map(|a| reference(a, types))
                    }
                    Term::Obj(m) => m.values().find_map(|a| reference(a, types)),
                    _ => None,
                }
            }
            // Depth first from the rule: the chain of predicates to the
            // first thing it reads that the provider serves.
            let mut seen = BTreeSet::new();
            let mut cells = BTreeSet::new();
            let mut stack: Vec<(&Atom, &[Lit], Vec<String>)> = vec![(head, body, Vec::new())];
            let mut found = None;
            'search: while let Some((h, b, path)) = stack.pop() {
                if let Some(what) = h.args.iter().find_map(|t| reference(t, &types)) {
                    found = Some((path.clone(), what));
                    break 'search;
                }
                for l in b {
                    let (Lit::Pos(a) | Lit::Not(a)) = l else {
                        continue;
                    };
                    if let Some(what) = served(a) {
                        found = Some((path.clone(), what));
                        break 'search;
                    }
                    // A cell that is not a resource's (a `let`, an input):
                    // what contributes to it.
                    if a.pred == "attr"
                        && let [t, scope, k, _] = a.args.as_slice()
                        && text(t).is_some_and(|t| crate::transform::is_pseudo_type(&t))
                        && cells.insert((t, scope, k))
                    {
                        // The cell is named by the reader that reads it.
                        for (h2, b2) in &rules {
                            if h2.pred == "arg"
                                && h2.args.len() == 5
                                && (&h2.args[0], &h2.args[1], &h2.args[2]) == (t, scope, k)
                            {
                                stack.push((h2, b2, path.clone()));
                            }
                        }
                        continue;
                    }
                    if seen.insert(a.pred.as_str()) {
                        let mut p = path.clone();
                        p.push(a.pred.clone());
                        for (h2, b2) in &rules {
                            if h2.pred == a.pred {
                                stack.push((h2, b2, p.clone()));
                            }
                        }
                    }
                }
            }
            let Some((path, what)) = found else { continue };
            let mut cycle = vec![format!("provider {name}'s configuration")];
            cycle.extend(path.iter().map(|p| format!("reads {p}")));
            cycle.push(format!("reads {what}"));
            cycle.push(format!("which provider {name} serves"));
            diags.push(
                crate::diag::Diagnostic::error(
                    head.span,
                    format!(
                        "provider {name} is configured from {what}, which it serves itself: a cycle"
                    ),
                )
                .with_note(format!("the cycle: {}", cycle.join(" -> ")))
                .with_help(
                    "a provider's configuration is evaluated before anything it serves: read \
                     inputs, settings, tables, env_var, or another provider's values",
                ),
            );
        }
        if diags.is_empty() {
            Ok(())
        } else {
            Err(crate::diag::Diagnostics(diags).into())
        }
    }

    /// Every resource's type is declared by the schema of the provider that
    /// will apply it: a type no provider of the stack declares would go to
    /// the fallback, which knows nothing of it (no computed attribute, no
    /// id it mints), so it is a plan error before anything is planned; a
    /// type the program declares itself (a `type` block) is the mock's. The
    /// error names the resource, the stack's providers' `use`s, and the
    /// known schemas that do declare the type. `specs`: the providers this
    /// run started, as `--provider` or the providers' `use`s give them.
    pub fn check_types(&self, program: &crate::ast::Program, specs: &[String]) -> Result<()> {
        use crate::ast::Stmt;
        let owner = &self.loaded().owner;
        let blocks: Vec<&crate::ast::Config> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Provider(c) if self.blocks.contains_key(&c.name) => Some(c),
                _ => None,
            })
            .collect();
        let names: Vec<String> = if !blocks.is_empty() {
            blocks.iter().map(|c| c.name.clone()).collect()
        } else if specs.is_empty() {
            self.names.clone()
        } else {
            specs.to_vec()
        };
        let who = match names.as_slice() {
            [one] => format!("provider {one} does not declare"),
            many => format!("none of the providers {} declares", many.join(", ")),
        };
        // The types the program declares itself (a `type` block, a
        // `type_*` row), in any module: the mock plays them with the
        // program's shape.
        let own: BTreeSet<&str> = crate::modules::nested(&program.statements)
            .into_iter()
            .filter_map(|s| match s {
                Stmt::Pending(p) => {
                    let crate::ast::PendingKind::TypeDecl { name, .. } = &p.kind;
                    Some(name.as_str())
                }
                Stmt::Fact(a) if a.pred.starts_with("type_") => match a.args.first() {
                    Some(Term::Val(Value::Str(t))) => Some(t.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let mut diags = Vec::new();
        // The program's resources and those of each module it uses and
        // each component it makes a resource of (R-113), once each.
        let reached = crate::modules::reached(program);
        for r in reached.into_iter().filter_map(|s| match s {
            Stmt::Resource(r) => Some(r),
            _ => None,
        }) {
            let Term::Val(Value::Str(typ)) = &r.typ else {
                continue;
            };
            // A kind of a provider the program configures is known once
            // the provider is (R-110): the plan lists it under `later`.
            // Nor is a kind its namespace's cluster may serve once a CRD
            // is made, however the provider is configured (R-126): the
            // plan waits on the CRD the program makes, or names the one it
            // lacks.
            if owner.contains_key(typ)
                || own.contains(typ.as_str())
                || self.waits(typ).is_some()
                || self.reached(typ).is_some()
            {
                continue;
            }
            let by = crate::schema::declaring(typ);
            let known = match by.as_slice() {
                [] => "no known provider schema declares it".to_string(),
                by => format!("declared by: {}", by.join(", ")),
            };
            // A relationship that is an attribute of one side (R-158).
            if let Some((t, p)) = crate::schema::instead(typ) {
                diags.push(
                    crate::diag::Diagnostic::error(
                        r.span,
                        format!("{typ} is no resource: it is {t}'s attribute {p}"),
                    )
                    .with_help(format!(
                        "write `{p} = [..]` in the {t}'s block, or add to it from \
                         anywhere with `set R.{p} = [..] where ..`: each write adds its \
                         elements"
                    )),
                );
                continue;
            }
            let mut d = crate::diag::Diagnostic::error(r.span, format!("{who} {typ}; {known}"));
            for c in &blocks {
                d = d.with_label(c.span, format!("provider {}", c.name));
            }
            // A provider's types are named under it (R-36): `aws.vpc` is
            // provider aws's. The fake cloud's namespaces (`net`, `iam`)
            // name no provider, so where a known schema gives the type to
            // another provider, that schema is the one to configure.
            let ns = typ.split_once('.').map(|(ns, _)| ns).filter(|ns| {
                by.is_empty()
                    || by.iter().any(|n| {
                        crate::schema::load_provider(n)
                            .is_ok_and(|s| s.provider_of.get(typ).is_some_and(|p| p == ns))
                    })
            });
            match (ns, by.first()) {
                (Some(ns), _) if !names.iter().any(|n| n == ns) => {
                    d = d.with_help(format!(
                        "{typ} is provider {ns}'s type: add `use {ns}` to the stack"
                    ));
                }
                (None, Some(first)) => {
                    d = d.with_help(format!("configure the provider that does: `use {first}`"));
                }
                _ => {}
            }
            diags.push(d);
        }
        if diags.is_empty() {
            Ok(())
        } else {
            Err(crate::diag::Diagnostics(diags).into())
        }
    }

    /// `expect_account` (`provider_expect_account(Name, Account)` in
    /// `facts`): each configured provider it names reports that account at
    /// Configure, or the run is refused before anything is planned. A
    /// provider still waiting on the program's settings is checked once
    /// they arrive. `secret`: the providers whose expected account a
    /// secret reaches (`secrets::secret_expected_accounts`), named by its
    /// label, `provider/NAME#expect_account`, never its value. `settings`:
    /// the settings a secret reaches, by provider
    /// (`secrets::secret_settings`); an account a provider reports that is
    /// one of their values (a provider that echoes its token) is named by
    /// the setting's label, `provider/NAME#KEY`, never its value.
    pub fn check_accounts<'a>(
        &self,
        facts: impl IntoIterator<Item = &'a Atom> + Clone,
        secret: &BTreeSet<String>,
        settings: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<()> {
        // The values of the secret settings, by their label.
        let mut secrets: Vec<(String, String)> = Vec::new();
        for a in facts
            .clone()
            .into_iter()
            .filter(|a| a.pred == "provider_config")
        {
            let [Term::Val(Value::Str(name)), Term::Val(Value::Obj(given))] = a.args.as_slice()
            else {
                continue;
            };
            for k in settings.get(name).into_iter().flatten() {
                if let Some(v) = given.get(k) {
                    let label = crate::value::null_label("provider", name, k);
                    secrets.push((spell::bare(v), label));
                }
            }
        }
        let reported = |i: usize, got: &String| {
            if let Some(label) = self.secret_accounts.borrow().get(&i) {
                return format!("{} (a secret)", crate::ir::label(label));
            }
            match secrets.iter().find(|(v, _)| v == got) {
                Some((_, label)) => format!("{label} (a secret)"),
                None => got.clone(),
            }
        };
        let mut wrong = Vec::new();
        for a in facts.into_iter().filter(|a| a.pred == EXPECT_ACCOUNT) {
            let [Term::Val(Value::Str(name)), Term::Val(want)] = a.args.as_slice() else {
                continue;
            };
            let Some(i) = self.link_for(name) else {
                continue;
            };
            if self.awaiting.borrow().contains(&i) {
                continue;
            }
            let want = spell::bare(want);
            let shown = if secret.contains(name) {
                format!(
                    "{} (a secret)",
                    crate::value::null_label("provider", name, "expect_account")
                )
            } else {
                want.clone()
            };
            match self.accounts.borrow().get(&i) {
                Some(got) if *got == want => {}
                Some(got) => wrong.push(format!(
                    "provider {name} reports account {}, but the program expects {shown} \
                     (expect_account)",
                    reported(i, got)
                )),
                None => wrong.push(format!(
                    "provider {name} reports no account, but the program expects {shown} \
                     (expect_account): {} cannot tell which account its credentials reach",
                    self.names[i]
                )),
            }
        }
        if wrong.is_empty() {
            return Ok(());
        }
        bail!("refusing to plan: {}", wrong.join("; "))
    }

    /// Ask every provider for its schema: the rows of the `scope` types
    /// (and the types they alias) and every `type_provider` and
    /// `type_alias` row, or, with no scope, all of it. Once per run; a
    /// second call is an error.
    pub fn load_schema(&self, scope: Option<&BTreeSet<String>>) -> Result<()> {
        let mut schema = Schema::default();
        let mut owner = BTreeMap::new();
        let mut externs = BTreeMap::new();
        let types = scope.map(|s| pb::TypeFilter {
            names: s.iter().cloned().collect(),
        });
        // Each spec's answer and the namespace it is in: a provider's
        // other names take it renamed rather than ask again (R-115).
        let mut asked: BTreeMap<&str, (pb::SchemaResponse, &str)> = BTreeMap::new();
        for (i, link) in self.links.iter().enumerate() {
            let share = self.shares.get(i).and_then(Option::as_ref);
            let resp = match share.and_then(|s| asked.get(s.spec.as_str())) {
                Some((resp, ns)) => {
                    let mut resp = resp.clone();
                    let to = share.map_or(*ns, |s| s.namespace.as_str());
                    wire::Rename::new(*ns, to).schema(&mut resp);
                    resp
                }
                None => {
                    // The types each name of the provider is asked for,
                    // under this one's.
                    let mut types = types.clone();
                    if let (Some(t), Some(s)) = (&mut types, share) {
                        let others = self.shares.iter().flatten();
                        for o in others.filter(|o| o.spec == s.spec && o.namespace != s.namespace) {
                            let r = wire::Rename::new(o.namespace.as_str(), s.namespace.as_str());
                            let more: Vec<String> = t.names.iter().map(|n| r.name(n)).collect();
                            t.names.extend(more);
                        }
                        t.names.sort();
                        t.names.dedup();
                    }
                    let req = pb::SchemaRequest { types };
                    let resp: pb::SchemaResponse = link.borrow_mut().call(req)?;
                    if let Some(s) = share {
                        asked.insert(&s.spec, (resp.clone(), &s.namespace));
                    }
                    resp
                }
            };
            let facts = resp
                .facts
                .iter()
                .map(Atom::try_from)
                .collect::<Result<Vec<Atom>>>()?;
            let mut s = Schema::from_facts(&facts)
                .with_context(|| format!("the schema of provider {}", self.names[i]))?;
            // A provider takes another name only when its types are under
            // its own (`ovh.instance`): one it names otherwise (the fake
            // cloud's `net.vpc`) stays as it is under every name.
            if let Some(Share {
                namespace,
                of: Some(of),
                ..
            }) = share
                && let Some(t) = s
                    .provider_of
                    .keys()
                    .chain(s.attrs.keys().map(|(t, _)| t))
                    .find(|t| !t.starts_with(&format!("{namespace}.")))
            {
                bail!(
                    "`use {of} as {namespace}`: provider {of} serves {t}, a type not named \
                     under it, which no other name renames: only a provider whose types are \
                     `{of}.T` takes another name"
                );
            }
            if resp.checks_refinements {
                s.checks_refinements = s
                    .provider_of
                    .keys()
                    .chain(s.attrs.keys().map(|(t, _)| t))
                    .cloned()
                    .collect();
            }
            for t in s.provider_of.keys().chain(s.attrs.keys().map(|(t, _)| t)) {
                owner.entry(t.clone()).or_insert(i);
            }
            for e in resp.externs.iter().map(|e| &e.pred).chain(s.externs.keys()) {
                externs.entry(e.clone()).or_insert(i);
            }
            // The settings its handshake declares, under each name the
            // program gives it; so its connection (R-193), which its
            // schema flags.
            let declared = &link.borrow().settings;
            let connection: BTreeSet<String> = s.connection.values().flatten().cloned().collect();
            if !declared.is_empty() {
                let names = self
                    .blocks
                    .iter()
                    .filter(|&(_, &j)| j == i)
                    .map(|(b, _)| b.clone())
                    .chain([self.names[i].clone()]);
                for n in names {
                    s.settings
                        .entry(n.clone())
                        .or_default()
                        .extend(declared.iter().cloned());
                    s.connection
                        .entry(n)
                        .or_default()
                        .extend(connection.iter().cloned());
                }
            }
            schema = schema.merge(s)?;
        }
        let loaded = Loaded {
            schema: Generation::new(schema),
            learned: RefCell::new(Vec::new()),
            owner,
            externs,
            scope: scope.cloned(),
        };
        if self.loaded.set(loaded).is_err() {
            bail!("internal: the providers' schema is loaded once");
        }
        Ok(())
    }

    fn loaded(&self) -> &Loaded {
        self.loaded
            .get()
            .expect("internal: Providers::load_schema before the schema is read")
    }

    /// The providers' schema as the run knows it now: as loaded, and what
    /// a provider configured since serves ([`Providers::learn`]).
    pub fn schema(&self) -> &Schema {
        &self.loaded().schema.latest().schema
    }

    /// The catalog facts of the types learned since the schema was loaded
    /// ([`Providers::learn`]), for each evaluation after.
    pub fn learned(&self) -> Vec<Atom> {
        self.loaded().learned.borrow().clone()
    }

    /// The kinds the provider `i`, configured now from the program's
    /// settings, serves that no schema declared when the run loaded it (a
    /// cluster's CRDs, which the provider cached at that Configure,
    /// R-110): of the types the run names (every one, with no scope),
    /// asked for and added to dform's schema for the rest of the run, so a
    /// later tick plans them typed, as the next run would.
    fn learn(&self, i: usize) -> Result<()> {
        let l = self.loaded();
        let known = self.schema();
        let missing = l.scope.as_ref().map(|scope| {
            scope
                .iter()
                .filter(|t| !known.knows_type(t))
                .filter(|t| {
                    t.split_once('.')
                        .is_some_and(|(ns, _)| self.link_for(ns) == Some(i))
                })
                .cloned()
                .collect::<Vec<_>>()
        });
        if missing.as_ref().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        let req = pb::SchemaRequest {
            types: missing.clone().map(|names| pb::TypeFilter { names }),
        };
        let resp: pb::SchemaResponse = self.links[i].borrow_mut().call(req)?;
        let new = |t: &str| {
            !known.knows_type(t) && missing.as_ref().is_none_or(|m| m.iter().any(|x| x == t))
        };
        let facts = resp
            .facts
            .iter()
            .map(Atom::try_from)
            .collect::<Result<Vec<Atom>>>()?
            .into_iter()
            .filter(|f| match f.args.first() {
                Some(Term::Val(Value::Str(t))) => new(t),
                _ => false,
            })
            .collect::<Vec<_>>();
        if facts.is_empty() {
            return Ok(());
        }
        let s = Schema::from_facts(&facts)
            .with_context(|| format!("the schema of provider {}", self.names[i]))?;
        let merged = known.clone().merge(s)?;
        if l.schema
            .latest()
            .next
            .set(Box::new(Generation::new(merged)))
            .is_err()
        {
            bail!("internal: the providers' schema extended twice at once");
        }
        l.learned
            .borrow_mut()
            .extend(facts.into_iter().filter(|f| f.pred != "type_doc"));
        Ok(())
    }

    /// The providers' schema facts (`type_attr`, `type_list_key`,
    /// `type_provider`, `type_mint`, ...), injected into the program as EDB:
    /// those of the `named` types ([`Schema::facts_for`]), or all of them;
    /// never `type_doc`, the descriptions the language server shows (a
    /// derived Kubernetes schema's are as many rows as its attributes, and
    /// long). A schema loaded for fewer types than asked for is an error.
    pub fn catalog(&self, named: Option<&BTreeSet<String>>) -> Result<Vec<Atom>> {
        let l = self.loaded();
        if let Some(scope) = &l.scope
            && named.is_none_or(|n| !n.is_subset(scope))
        {
            bail!("internal: the schema was loaded for fewer types than the run names");
        }
        let schema = self.schema();
        let mut facts = match named {
            Some(named) => schema.facts_for(named),
            None => schema.facts.clone(),
        };
        facts.retain(|f| f.pred != "type_doc");
        Ok(facts)
    }

    /// What the providers said during the last apply.
    pub fn take_notes(&self) -> Vec<String> {
        self.notes.take()
    }

    /// The calls sent again since the last time they were taken (R-81),
    /// for the audit log.
    pub fn take_retries(&self) -> Vec<Retry> {
        self.links
            .iter()
            .flat_map(|l| l.borrow_mut().take_retries())
            .collect()
    }

    /// The link `i`; none when the run started no provider (a program
    /// naming only built-in providers, R-26): nothing to ask.
    fn link(&self, i: usize) -> Result<&RefCell<Link>> {
        self.links
            .get(i)
            .ok_or_else(|| anyhow!(crate::deployment::NO_PROVIDER))
    }

    /// Whether the provider serving `typ` leaves a path an Apply update
    /// names in `keep` as the object has it (the `keep` capability): then
    /// a write-only secret proved unchanged without the master is kept,
    /// not sent (R-164).
    pub fn keeps(&self, typ: &str) -> bool {
        self.link(self.route(typ))
            .is_ok_and(|l| l.borrow().has("keep"))
    }

    /// The link serving `typ`: the one whose schema declares it; else the
    /// configured provider its namespace names (a cluster's CRD, which the
    /// run's schema, loaded before the cluster was reached, lacks: the
    /// provider has it since its settings arrived, R-45); else the
    /// fallback.
    fn route(&self, typ: &str) -> usize {
        if let Some(&i) = self.loaded().owner.get(typ) {
            return i;
        }
        self.reached(typ).unwrap_or(self.fallback)
    }

    /// The provider `typ`'s namespace names, configured, that the program
    /// configures (its settings arrived) or that serves a cluster's kinds
    /// whatever configures it (its schema has the namespace's
    /// `custom_resource_definition`: a CRD extends it, R-126).
    fn reached(&self, typ: &str) -> Option<usize> {
        let ns = typ.split_once('.')?.0;
        let i = self.link_for(ns)?;
        let cluster = || {
            self.loaded()
                .owner
                .get(&format!("{ns}.custom_resource_definition"))
                == Some(&i)
        };
        ((self.by_program.contains(&i) || cluster()) && !self.awaiting.borrow().contains(&i))
            .then_some(i)
    }

    fn link_named(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Read the world again at the next refresh, though nothing was
    /// written: a tick waits on what the world has not reached yet (R-81).
    pub fn reread(&self) {
        self.invalidate();
    }

    /// What a resource of `typ` waits on before its provider can plan it
    /// (R-110): the program's settings of the provider that serves it,
    /// while they are not known; or, for a type no schema declares whose
    /// namespace names a provider the program configures, that provider's
    /// schema (a cluster's CRD, served once the cluster is reached), until
    /// its settings arrive ([`Providers::route`]).
    pub fn waits(&self, typ: &str) -> Option<ProviderWait> {
        if let Some(&i) = self.loaded().owner.get(typ) {
            return self
                .awaiting
                .borrow()
                .contains(&i)
                .then(|| ProviderWait::Settings(self.block_name(i)));
        }
        let ns = typ.split_once('.')?.0;
        let i = self.link_for(ns)?;
        (self.by_program.contains(&i) && self.awaiting.borrow().contains(&i))
            .then(|| ProviderWait::Schema(self.block_name(i)))
    }

    /// Link `i` by the name the program's `use` gives it, else its own.
    fn block_name(&self, i: usize) -> String {
        self.blocks
            .iter()
            .find(|&(_, &j)| j == i)
            .map(|(b, _)| b.clone())
            .unwrap_or_else(|| self.names[i].clone())
    }

    /// Whether the provider serving `typ` plans it with no credentials as
    /// with them (the `offline` capability, R-188): against a schema it
    /// holds or a fake of its API. `Err` names the provider whose Plan
    /// needs them.
    pub fn plans_offline(&self, typ: &str) -> std::result::Result<(), String> {
        let i = self.route(typ);
        match self.link(i).is_ok_and(|l| l.borrow().has("offline")) {
            true => Ok(()),
            false => Err(self.block_name(i)),
        }
    }

    /// A write happened: the next refresh Reads again.
    fn invalidate(&self) {
        self.refreshed.take();
        self.imported.borrow_mut().clear();
    }

    /// State's entries some provider of this stack serves (one awaiting
    /// the program's settings serves none yet).
    fn entries<'a>(
        &'a self,
        map: &'a BTreeMap<String, StateEntry>,
    ) -> impl Iterator<Item = (Address, &'a StateEntry)> + 'a {
        map.iter()
            .filter_map(|(k, e)| state::parse_key(k).map(|a| (a, e)))
            .filter(|(a, e)| {
                self.link_serving(&a.typ, &e.provider)
                    .is_some_and(|i| !self.awaiting.borrow().contains(&i))
            })
    }

    /// The link that serves a state entry of `typ` made by `provider`:
    /// the one of that name, or, when two have it (the mock beside itself
    /// as a plugin, both `fakecloud`), the one serving the type.
    fn link_serving(&self, typ: &str, provider: &str) -> Option<usize> {
        match self.names.iter().filter(|n| *n == provider).count() {
            0 | 1 => self.link_named(provider),
            _ => Some(self.route(typ)).filter(|&i| self.names[i] == provider),
        }
    }

    /// Query (E DR-18): an extern's answer, the rows of `pred` whose `+`
    /// columns (`plus`) are `inputs`. No row is an answer too.
    pub fn query(&self, pred: &str, plus: &[bool], inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        let i = self
            .loaded()
            .externs
            .get(pred)
            .copied()
            .unwrap_or(self.fallback);
        self.query_at(i, pred, plus, inputs)
    }

    /// An extern's answer (`query`), its secret columns (`secret(T)`) as
    /// the provider holds them: each a secret null, labeled
    /// [`crate::externs::secret_label`], whose place ([`provider::Held`])
    /// the run keeps for the Apply documents that carry it. A secret column
    /// answered with its value is refused: the bytes would be in the
    /// engine, the plan and state.
    pub fn query_extern(
        &self,
        f: &crate::ast::ExternFn,
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let plus: Vec<bool> = f.args.iter().map(|b| b.input).collect();
        let secret: Vec<bool> = f.args.iter().map(crate::externs::is_secret).collect();
        if !secret.contains(&true) {
            return self.query(&f.name, &plus, inputs);
        }
        let i = self
            .loaded()
            .externs
            .get(&f.name)
            .copied()
            .unwrap_or(self.fallback);
        let rows: Vec<pb::Row> = self.link(i)?.borrow_mut().call(pb::QueryRequest {
            pred: f.name.clone(),
            input: plus,
            inputs: inputs.iter().map(pb::Value::from).collect(),
            secret: secret.clone(),
        })?;
        let mut out = Vec::new();
        for r in &rows {
            let mut row = Vec::new();
            for (c, v) in r.values.iter().enumerate() {
                if !secret.get(c).copied().unwrap_or(false) {
                    row.push(Value::try_from(v)?);
                    continue;
                }
                let held = match &v.kind {
                    Some(pb::value::Kind::Null(n)) if n.class == pb::NullClass::Secret as i32 => {
                        n.held.as_ref()
                    }
                    _ => None,
                };
                let Some(h) = held else {
                    bail!(
                        "provider {}: extern {} column {} is secret(T), and it answered with \
                         a value, not where it holds one (a SECRET null with `held`): a secret \
                         never enters dform",
                        self.names[i],
                        f.name,
                        c + 1
                    );
                };
                let label = crate::externs::secret_label(&f.name, inputs, c);
                self.held.borrow_mut().insert(
                    label.clone(),
                    provider::Held {
                        provider: h.provider.clone(),
                        deployment: h.deployment.clone(),
                        typ: h.r#type.clone(),
                        remote: h.remote.clone(),
                        path: h.path.clone(),
                        digest: h.digest.clone(),
                    },
                );
                let ty = match &f.args[c].ty {
                    Some(crate::ast::TypeExpr::Apply(_, a)) => match a.as_slice() {
                        [crate::ast::TypeExpr::Name(t)] => t.clone(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                row.push(Value::Null {
                    label,
                    class: NullClass::Secret,
                    ty,
                });
            }
            out.push(row);
        }
        Ok(out)
    }

    /// A world document as dform keeps it beyond the run (the in-flight
    /// record, the controller's baseline) and compares with what it kept:
    /// each leaf at a path the schema marks sensitive as its keyed digest,
    /// `"(sensitive hmac-sha256:..)"` (`"(sensitive)"` with no key), never
    /// its value. A marker, and a leaf already kept so, stays.
    pub fn stored(&self, typ: &str, doc: &Json) -> Json {
        fn walk(p: &Providers, typ: &str, v: &Json, path: &str) -> Json {
            if provider::marker(v).is_some()
                || v.as_str().is_some_and(|s| s.starts_with("(sensitive"))
            {
                return v.clone();
            }
            if !path.is_empty() && p.schema().is_sensitive(typ, &provider::norm_path(path)) {
                return Json::String(match &p.digest_key {
                    Some(k) => format!(
                        "(sensitive hmac-sha256:{})",
                        k.digest(crate::approval::canonical_json(v).as_bytes())
                    ),
                    None => "(sensitive)".into(),
                });
            }
            let join = |k: &str| crate::ir::path_join(path, k);
            match v {
                Json::Object(m) => Json::Object(
                    m.iter()
                        .map(|(k, x)| (k.clone(), walk(p, typ, x, &join(k))))
                        .collect(),
                ),
                Json::Array(xs) => Json::Array(
                    xs.iter()
                        .enumerate()
                        .map(|(i, x)| walk(p, typ, x, &format!("{path}[{i}]")))
                        .collect(),
                ),
                v => v.clone(),
            }
        }
        walk(self, typ, doc, "")
    }

    /// A write-only attribute's value as the world document has it (R-106):
    /// the world never answers one, so the program's value when its digest
    /// is what state kept from the last apply (or state kept none: an
    /// object made before, or elsewhere), else `(write-only)`, a change.
    fn written_before(&self, typ: &str, entry: &StateEntry, want: &Json, doc: &mut Json) {
        for p in self.compared_written(typ, want) {
            let Some(v) = get_path(want, &p) else {
                continue;
            };
            let same = entry
                .written
                .get(&p)
                .is_none_or(|kept| self.written_matches(v, kept));
            let v = match same {
                true => v.clone(),
                false => Json::String("(write-only)".into()),
            };
            set_path(doc, &p, v);
        }
    }

    /// The paths of `doc` compared by the digest of what was sent: its
    /// type's write-only attributes, and those a secret is revealed into.
    fn compared_written(&self, typ: &str, doc: &Json) -> Vec<String> {
        let mut out: Vec<String> = self
            .schema()
            .write_only_of(typ)
            .into_iter()
            .map(str::to_string)
            .collect();
        for p in self.revealed_attrs(typ, doc) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// Of an object that exists, each attribute `lifecycle` says is given
    /// at its creation only is dropped from both sides, `want` the
    /// program's and `doc` the world's (R-198): not compared, so neither a
    /// change nor, at a `force_new` path, a replace. A create (no world
    /// side) sends it. Of a `bootstrap` one, a value that differs from
    /// what the object was made with is kept: the plan says so.
    fn at_create(
        &self,
        addr: &Address,
        lifecycle: &Lifecycle,
        want: &mut Json,
        doc: &mut Json,
    ) -> Vec<Change> {
        // One attribute's leaves in the canonical form the Z-set compares.
        let flat = |d: &Json, p: &str| {
            let mut only = json!({});
            if let Some(v) = get_path(d, p) {
                set_path(&mut only, p, v.clone());
            }
            self.flat_value(&addr.typ, &only)
        };
        let mut kept = Vec::new();
        for (p, how) in lifecycle.at_create_of(addr) {
            if how == zset::AtCreate::Bootstrap && flat(want, p) != flat(doc, p) {
                kept.push(Change {
                    path: p.clone(),
                    before: get_path(doc, p).cloned(),
                    after: get_path(want, p).cloned(),
                    sensitive: self.schema().is_sensitive(&addr.typ, p),
                });
            }
            remove_path(doc, p);
            remove_path(want, p);
        }
        kept
    }

    /// The digest state keeps of a write-only value: keyed with the
    /// deployment's master, `hmac-sha256:..`; none in a run that does not
    /// hold it (never an unkeyed digest: a short password's is a table
    /// lookup away from it, R-164).
    fn written_digest(&self, v: &Json) -> Option<String> {
        let text = crate::approval::canonical_json(v);
        let k = self.digest_key.as_ref()?;
        Some(format!("hmac-sha256:{}", k.digest(text.as_bytes())))
    }

    /// Whether `v` is the value whose digest is `kept`. A run that does
    /// not hold the master cannot tell: the value is compared at apply (a
    /// leaf it derives may still be proven unchanged, [`Providers::derived_before`]).
    fn written_matches(&self, v: &Json, kept: &str) -> bool {
        let text = crate::approval::canonical_json(v);
        match (kept.split_once(':'), &self.digest_key) {
            (Some(("hmac-sha256", d)), Some(k)) => k.digest(text.as_bytes()) == d,
            _ => false,
        }
    }

    /// The digests of `doc`'s write-only attributes, by path; one this run
    /// cannot digest keeps what `before` had.
    fn written(
        &self,
        typ: &str,
        doc: &Json,
        before: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        self.compared_written(typ, doc)
            .into_iter()
            .filter_map(|p| {
                let d = self
                    .written_digest(get_path(doc, &p)?)
                    .or_else(|| before.get(&p).cloned())?;
                Some((p, d))
            })
            .collect()
    }

    /// The derivation digest of each leaf of `doc` that holds a derived
    /// value (`secrets::standin::digest`), by path: an object's members
    /// each, anything else (a string, a list) whole.
    pub fn derivations(&self, doc: &Json) -> BTreeMap<String, String> {
        fn walk(v: &Json, path: &str, out: &mut BTreeMap<String, String>) {
            match v {
                Json::Object(m) if provider::marker(v).is_none() => {
                    for (k, x) in m {
                        walk(x, &crate::ir::path_join(path, k), out);
                    }
                }
                _ if !path.is_empty() => {
                    if let Some(d) = crate::secrets::standin::digest(v) {
                        out.insert(path.to_string(), d);
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeMap::new();
        walk(doc, "", &mut out);
        out
    }

    /// In a run that does not hold the master (R-164): each leaf of `want`
    /// whose derivation digest is the one state recorded at the last apply
    /// is unchanged, so the world's side is taken to be it (the values
    /// are the stand-ins of what was applied), and the leaf is proven
    /// ([`Providers::proven`]). Any other leaf that holds a stand-in
    /// differs: a change that needs the master.
    fn derived_before(&self, addr: &Address, entry: &StateEntry, want: &Json, doc: &mut Json) {
        if !crate::secrets::standin::active() {
            return;
        }
        let mut proven = Vec::new();
        for (path, d) in &entry.derived {
            let Some(w) = get_path(want, path) else {
                continue;
            };
            if crate::secrets::standin::digest(w).as_ref() == Some(d) {
                set_path(doc, path, w.clone());
                proven.push(path.clone());
            }
        }
        if !proven.is_empty() {
            self.proven.borrow_mut().insert(addr.clone(), proven);
        }
    }

    /// Of `paths` of `addr`, those whose value the world answers (not a
    /// write-only attribute's): one a run without the master proves
    /// unchanged in the program, but cannot compare with the world.
    pub fn answered(&self, addr: &Address, paths: &[String]) -> Vec<String> {
        let wo = self.schema().write_only_of(&addr.typ);
        paths
            .iter()
            .filter(|p| {
                !wo.iter()
                    .any(|w| p.as_str() == *w || p.starts_with(&format!("{w}.")))
            })
            .cloned()
            .collect()
    }

    /// The leaves of `addr` the last plan proved unchanged without the
    /// master (R-164), by path.
    pub fn proven(&self, addr: &Address) -> Vec<String> {
        self.proven.borrow().get(addr).cloned().unwrap_or_default()
    }

    /// The paths of `a`'s changes a run that does not hold the master
    /// cannot make (R-164): each whose value holds a stand-in, and every
    /// one of a create's (or a replacement's) when its document `desired`
    /// holds one; an update whose unchanged secret leaf only stands in,
    /// where the world does not answer it (a write-only one), too. Empty
    /// when the run holds the master.
    pub fn needs_master(&self, a: &Action, desired: Option<&Json>) -> Vec<String> {
        use crate::secrets::standin::carries;
        if !crate::secrets::standin::active() {
            return Vec::new();
        }
        let changed: Vec<String> = a
            .changes
            .iter()
            .filter(|c| c.after.as_ref().is_some_and(carries))
            .map(|c| c.path.clone())
            .collect();
        match a.kind {
            ActionKind::Create | ActionKind::Adopt | ActionKind::Replace { .. } => {
                match (changed.is_empty(), desired.is_some_and(carries)) {
                    (true, true) => vec![String::new()],
                    _ => changed,
                }
            }
            ActionKind::Update | ActionKind::Drift => {
                if !changed.is_empty() {
                    return changed;
                }
                // Its provider leaves the write-only one as it is.
                if self.keeps(&a.addr.typ) {
                    return Vec::new();
                }
                let wo = self.schema().write_only_of(&a.addr.typ);
                self.proven(&a.addr)
                    .into_iter()
                    .filter(|p| {
                        wo.iter()
                            .any(|w| p.as_str() == *w || p.starts_with(&format!("{w}.")))
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// [`Providers::stored`] of each document.
    pub fn stored_world(&self, docs: &BTreeMap<Address, Json>) -> BTreeMap<Address, Json> {
        docs.iter()
            .map(|(a, d)| (a.clone(), self.stored(&a.typ, d)))
            .collect()
    }

    fn query_at(
        &self,
        i: usize,
        pred: &str,
        plus: &[bool],
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let rows: Vec<pb::Row> = self.link(i)?.borrow_mut().call(pb::QueryRequest {
            pred: pred.to_string(),
            input: plus.to_vec(),
            inputs: inputs.iter().map(pb::Value::from).collect(),
            secret: Vec::new(),
        })?;
        rows.iter()
            .map(|r| r.values.iter().map(Value::try_from).collect())
            .collect()
    }

    /// Discovery: the inventory relations of every provider that has one,
    /// of the types `types` names (every type: `None`), each asked with its
    /// type bound.
    pub fn discover(&self, types: Option<&BTreeSet<String>>) -> Result<Vec<Atom>> {
        let mut out = Vec::new();
        for (i, link) in self.links.iter().enumerate() {
            if !link.borrow().has("inventory") {
                continue;
            }
            for (pred, arity) in INVENTORY {
                let mut rows = Vec::new();
                match types {
                    None => rows = self.query_at(i, pred, &vec![false; arity], &[])?,
                    Some(ts) => {
                        let mut plus = vec![false; arity];
                        plus[0] = true;
                        for t in ts {
                            rows.extend(self.query_at(i, pred, &plus, &[Value::Str(t.clone())])?);
                        }
                    }
                }
                for row in rows {
                    out.push(Atom {
                        pred: pred.to_string(),
                        args: row.into_iter().map(Term::Val).collect(),
                        record: None,
                        span: Default::default(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// The provider that owns `typ`, by name, as state records it.
    pub fn provider_of(&self, typ: &str) -> &str {
        self.names.get(self.route(typ)).map_or("", String::as_str)
    }

    /// Whether the provider a program names `name` (its own name, or its
    /// provider's `use` block's: `provider_config(name, ..)`) serves `typ`.
    pub fn serves(&self, name: &str, typ: &str) -> bool {
        self.link_for(name) == Some(self.route(typ))
    }

    /// The providers started, by name, in link order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The object an Apply Create or Replace of `addr` with the
    /// idempotency key `key` made (`CREATED`): its remote id. `None` when
    /// it made none, or when its provider cannot say (no `managed`
    /// capability): then only a retry with the same key finds out.
    pub fn created(&self, addr: &Address, key: &str) -> Result<Option<String>> {
        let i = self.route(&addr.typ);
        if !self.link(i)?.borrow().has("managed") {
            return Ok(None);
        }
        let s = |x: &str| Value::Str(x.to_string());
        let rows = self.query_at(
            i,
            CREATED,
            &[true, true, true, false],
            &[s(&addr.typ), s(&addr.name), s(key)],
        )?;
        match rows.first().map(Vec::as_slice) {
            None => Ok(None),
            Some([_, _, _, Value::Str(remote)]) => Ok(Some(remote.clone())),
            Some(row) => bail!(
                "provider {}: {CREATED} answers (Type, Name, Key, RemoteId), not {row:?}",
                self.names[i]
            ),
        }
    }

    /// Whether the object `remote` of `addr`'s type is there: one Read.
    pub fn exists(&self, addr: &Address, remote: &str) -> Result<bool> {
        Ok(self.read(self.route(&addr.typ), addr, remote)?.is_some())
    }

    fn object(attrs: Option<&pb::Value>, computed: Option<&pb::Value>) -> Result<Object> {
        Ok(Object {
            attrs: wire::from_doc_or_empty(attrs)?,
            computed: wire::from_doc_or_empty(computed)?,
        })
    }

    /// Read one object; `None` when Read returns nothing (the provider
    /// retries a type's `type_retry` attempts before it says so).
    fn read(&self, i: usize, addr: &Address, remote: &str) -> Result<Option<Object>> {
        let req = pb::ReadRequest {
            r#type: addr.typ.clone(),
            remote: remote.to_string(),
            name: addr.name.clone(),
        };
        let r: pb::ReadResponse = self.link(i)?.borrow_mut().call(req)?;
        if !r.found {
            return Ok(None);
        }
        Self::object(r.attrs.as_ref(), r.computed.as_ref()).map(Some)
    }

    /// Import: an object by remote id, whether dform manages it or not.
    fn import(&self, typ: &str, remote: &str) -> Result<Option<Object>> {
        let k = key(typ, remote);
        if let Some(o) = self.imported.borrow().get(&k) {
            return Ok(o.clone());
        }
        let req = pb::ImportRequest {
            r#type: typ.to_string(),
            remote: remote.to_string(),
        };
        let r: pb::ImportResponse = self.link(self.route(typ))?.borrow_mut().call(req)?;
        let o = match r.found {
            true => Some(Self::object(r.attrs.as_ref(), r.computed.as_ref())?),
            false => None,
        };
        self.imported.borrow_mut().insert(k, o.clone());
        Ok(o)
    }

    /// Refresh: every object state maps (deposed ones included), as Read
    /// returns it. An object Read does not return is gone; one state does
    /// not map is not read.
    fn refresh(&self, state: &State) -> Result<World> {
        let mut mapped: BTreeMap<String, (Address, String, usize)> = BTreeMap::new();
        for (a, e) in self
            .entries(&state.resources)
            .chain(self.entries(&state.deposed))
        {
            let i = self
                .link_serving(&a.typ, &e.provider)
                .expect("entries are served");
            mapped.insert(key(&a.typ, &e.remote), (a, e.remote.clone(), i));
        }
        let keys: BTreeSet<String> = mapped.keys().cloned().collect();
        if let Some((m, w)) = &*self.refreshed.borrow()
            && *m == keys
        {
            return Ok(w.clone());
        }
        // Every Read is sent before any answer is taken, to every provider,
        // so a provider that can answers them at once: a plan waits one
        // round trip to its API, not one per object. A Read that fails is
        // sent again alone, under the provider's retry policy.
        let mut sent = Vec::new();
        for (k, (addr, remote, i)) in mapped {
            let req = pb::ReadRequest {
                r#type: addr.typ.clone(),
                remote: remote.clone(),
                name: addr.name.clone(),
            };
            let t = self.link(i)?.borrow_mut().submit(req);
            sent.push((k, addr, remote, i, t));
        }
        for l in &self.links {
            l.borrow_mut().flush();
        }
        let mut world = World::new();
        for (k, addr, remote, i, t) in sent {
            let answer = {
                let mut link = self.links[i].borrow_mut();
                link.wait(t)
                    .and_then(|r| link.expect::<pb::ReadResponse>("Read", r))
            };
            let found = match answer {
                Ok(r) if !r.found => None,
                Ok(r) => Some(Self::object(r.attrs.as_ref(), r.computed.as_ref())?),
                Err(_) => self.read(i, &addr, &remote)?,
            };
            if let Some(o) = found {
                world.insert(k, o);
            }
        }
        self.refreshed.replace(Some((keys, world.clone())));
        Ok(world)
    }

    /// Health (R-203): each object state maps (deposed ones not) whose
    /// provider's handshake says it answers Health for its type, as that
    /// provider judges it now; one call to each provider. An object of a
    /// type nobody answers for is not asked, and not in the answer; one
    /// whose provider waits on the program's settings is `unknown`. For
    /// `dform status`, and asked at no other time: nothing of it reaches
    /// the plan, the apply or state.
    pub fn health(&self, state: &State) -> Result<BTreeMap<Address, pb::Health>> {
        let mut out = BTreeMap::new();
        let mut asked: BTreeMap<usize, Vec<Address>> = BTreeMap::new();
        let mut objects: BTreeMap<usize, Vec<pb::Identity>> = BTreeMap::new();
        for (k, e) in &state.resources {
            let Some(addr) = state::parse_key(k) else {
                continue;
            };
            let Some(i) = self.link_serving(&addr.typ, &e.provider) else {
                continue;
            };
            if !self.links[i].borrow().answers_health(&addr.typ) {
                continue;
            }
            if self.awaiting.borrow().contains(&i) {
                let why = format!(
                    "provider {} waits on the program's settings",
                    self.block_name(i)
                );
                out.insert(addr, super::backend::health(pb::HealthState::Unknown, why));
                continue;
            }
            objects.entry(i).or_default().push(pb::Identity {
                r#type: addr.typ.clone(),
                name: addr.name.clone(),
                remote: e.remote.clone(),
            });
            asked.entry(i).or_default().push(addr);
        }
        for (i, objects) in objects {
            let n = objects.len();
            let r: pb::HealthResponse = self.links[i]
                .borrow_mut()
                .call(pb::HealthRequest { objects })
                .with_context(|| format!("provider {}: Health", self.block_name(i)))?;
            if r.answers.len() != n {
                bail!(
                    "provider {}: Health answered {} objects of the {n} asked",
                    self.block_name(i),
                    r.answers.len()
                );
            }
            out.extend(
                asked
                    .remove(&i)
                    .unwrap_or_default()
                    .into_iter()
                    .zip(r.answers),
            );
        }
        Ok(out)
    }

    /// Refresh as facts, for round-0 resolution (E Rule 4, F DR-11 revised):
    /// `identity(T, A, Rid)` for every address state maps to an object Read
    /// returns, `remote_name(T, A, Program, Name)` for one dform named
    /// (`StateEntry::name`), and `world_attr(T, Rid, P, V)` for every schema-computed or
    /// Optional+Computed path the world holds a value for. Secrets are never
    /// handed to the evaluator.
    pub fn world_facts(&self, state: &State) -> Result<Vec<Atom>> {
        let world = self.refresh(state)?;
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let mut out = Vec::new();
        for (addr, e) in self.entries(&state.resources) {
            let Some(o) = world.get(&key(&addr.typ, &e.remote)) else {
                continue;
            };
            out.push(Atom {
                pred: "identity".into(),
                args: vec![s(&addr.typ), s(&addr.name), s(&e.remote)],
                record: None,
                span: Default::default(),
            });
            // The name dform gave it, for the program's (R-189): a read of
            // the name answers it (`transform::remote_name_prelude`).
            if let Some(name) = &e.name
                && let Some((program, _)) = state::generation(name)
            {
                out.push(Atom {
                    pred: crate::transform::REMOTE_NAME.into(),
                    args: vec![s(&addr.typ), s(&addr.name), s(program), s(name)],
                    record: None,
                    span: Default::default(),
                });
            }
            let paths = self
                .schema()
                .computed_of(&addr.typ)
                .into_iter()
                .chain(self.schema().optional_computed_of(&addr.typ));
            for (path, class) in paths {
                if class == NullClass::Secret {
                    continue;
                }
                if let Some(v) = get_path(&o.computed, &path)
                    && provider::marker(v).is_none()
                {
                    // A quantity or a time is read at the edge (R-66).
                    let mut v = provider::json_to_value(v);
                    if let Some(r) = self
                        .schema()
                        .attr(&addr.typ, &path)
                        .and_then(|a| a.render())
                    {
                        v = r.read_back(v);
                    }
                    out.push(Atom {
                        pred: "world_attr".into(),
                        args: vec![s(&addr.typ), s(&e.remote), s(&path), Term::Val(v)],
                        record: None,
                        span: Default::default(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// The world's configured attributes for every address state maps to an
    /// object Read returns: what the executor compares to see whether the
    /// world moved under a deformation.
    pub fn observe(&self, state: &State) -> Result<BTreeMap<Address, Json>> {
        let world = self.refresh(state)?;
        Ok(self
            .entries(&state.resources)
            .filter_map(|(addr, e)| {
                let o = world.get(&key(&addr.typ, &e.remote))?;
                Some((addr, o.attrs.clone()))
            })
            .collect())
    }

    /// The leaf-by-leaf changes from `before` to `after` (`provider::diff`).
    pub fn diff(&self, typ: &str, before: Option<&Json>, after: Option<&Json>) -> Vec<Change> {
        provider::diff(self.schema(), typ, before, after)
    }

    /// The owning providers' Plan for each `(address, remote, prior, desired)`:
    /// its changes, and whether they replace it. Every call is submitted
    /// before any answer is taken, so a backend that can runs them at once;
    /// the first failure, in order, is the error.
    fn plan_many(&self, asks: Vec<Ask>) -> Result<Vec<Planned>> {
        let mut out: Vec<Option<Result<Planned>>> = Vec::new();
        let mut submitted: Vec<(usize, usize, Ticket, Address)> = Vec::new();
        for (i, (addr, remote, prior, desired)) in asks.into_iter().enumerate() {
            if prior.is_none() && desired.is_none() {
                out.push(Some(Ok((Vec::new(), false))));
                continue;
            }
            out.push(None);
            let req = pb::PlanRequest {
                r#type: addr.typ.clone(),
                name: addr.name.clone(),
                prior: prior.as_ref().map(wire::doc),
                desired: desired.as_ref().map(wire::doc),
                remote,
            };
            let l = self.route(&addr.typ);
            submitted.push((i, l, self.link(l)?.borrow_mut().submit(req), addr));
        }
        for l in &self.links {
            l.borrow_mut().flush();
        }
        for (i, l, t, addr) in submitted {
            let mut link = self.links[l].borrow_mut();
            let r = link
                .wait(t)
                .and_then(|r| link.expect::<pb::PlanResponse>("Plan", r));
            // A refusal in R-109's shape: the change as the plan prints it,
            // then the provider's message (`deployment::plan` adds the site).
            let r = r.map_err(|e| {
                let happened = match &e {
                    CallError::Refused(_) => "refused",
                    CallError::MaybeApplied(_) => "no answer",
                    CallError::Crashed(_) => "the provider died",
                };
                anyhow::Error::new(crate::report::Failure::of(
                    "plan",
                    &addr,
                    happened,
                    &e.to_string(),
                ))
            });
            out[i] = Some(r.and_then(|r| {
                let changes = r
                    .changes
                    .iter()
                    .map(Change::try_from)
                    .collect::<Result<_>>()?;
                Ok((changes, r.requires_replace))
            }));
        }
        out.into_iter()
            .map(|r| r.expect("every ask is answered"))
            .collect()
    }

    /// `ref(T, N, Attr)` to a configured attribute (a ref to a computed one
    /// is the evaluator's null): the program's value for N, else the world's.
    fn resolve_ref(&self, ctx: &Ctx, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let addr = Address {
            typ: typ.to_string(),
            name: name.to_string(),
        };
        let label = crate::value::null_label(typ, name, attr);
        if ctx.renaming.contains(&addr) && self.schema().remote_name_of(typ) == Some(attr) {
            return Ok(provider::null_json(&label));
        }
        if let Some(v) = ctx.resolved.get(&addr).and_then(|d| get_path(d, attr)) {
            return Ok(v.clone());
        }
        let existing = ctx.existing(&addr)?;
        match existing.as_ref().and_then(|o| get_path(&o.attrs, attr)) {
            Some(v) => Ok(v.clone()),
            None => match ctx.strict {
                Some(at) => Err(not_sent(
                    at,
                    &format!(
                        "{} is still unknown ({} does not set {attr})",
                        crate::report::attribute_label(&label),
                        crate::report::address(&addr)
                    ),
                )),
                None => Ok(provider::null_json(&label)),
            },
        }
    }

    /// A labeled null in a desired document. The executor fills a fresh or
    /// open one at Apply from its owner's Apply earlier in the dependency
    /// order (E §2.7); a secret travels as its label and the provider
    /// materializes it. Plan shows what is still unknown as `?label`.
    fn resolve_null(&self, ctx: &Ctx, label: &str, class: NullClass) -> Result<Json> {
        if class == NullClass::Secret {
            return Ok(match self.held.borrow().get(label) {
                Some(h) => provider::held_json(label, h),
                None => provider::secret_json(label),
            });
        }
        let mut found = None;
        if let Some((typ, name, path)) = crate::value::null_parts(label) {
            let owner = Address { typ, name };
            if !ctx.retracted.contains(&owner)
                && let Some(o) = ctx.existing(&owner)?
            {
                found = get_path(&o.computed, &path).cloned();
            }
        }
        match (found, ctx.strict) {
            (Some(v), _) => Ok(v),
            (None, Some(at)) => Err(not_sent(
                at,
                &format!(
                    "{} is still unknown (its resource has not been created)",
                    crate::report::attribute_label(label)
                ),
            )),
            (None, None) => Ok(provider::null_json(label)),
        }
    }

    fn resolve_cloud_ref(&self, typ: &str, name: &str, attr: &str) -> Result<Json> {
        let k = key(typ, name);
        let Some(cur) = self.import(typ, name)? else {
            bail!("cloud_ref missing inventory resource {k}");
        };
        if let Some(v) = get_path(&cur.computed, attr).or_else(|| get_path(&cur.attrs, attr)) {
            return Ok(v.clone());
        }
        bail!(
            "cloud_ref missing attribute {}",
            Address {
                typ: typ.to_string(),
                name: name.to_string()
            }
            .attr(attr)
        );
    }

    /// A secret output of another stack in a resource's document must be
    /// held by a provider (`Config::held`), or sealed to this deployment
    /// and opened (its value then, not a null): otherwise nothing can read
    /// it here.
    /// `verb` and `addr` name the resource, `path` the attribute.
    fn check_held(&self, verb: &str, addr: &Address, path: &str, v: &Value) -> Result<()> {
        let join = |k: &str| crate::ir::path_join(path, k);
        match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } if !self.held.borrow().contains_key(label) => {
                if let Some((name, _)) = crate::stack::deployment_output(label) {
                    bail!(
                        "{verb} {}: {} is a secret output of {name} that no provider holds, \
                         and it is not sealed to this deployment: apply {name} again, which \
                         seals it to each deployment of the project that reads it (R-166)",
                        addr.attr(path),
                        crate::ir::label(label)
                    );
                }
                Ok(())
            }
            Value::List(xs) => xs
                .iter()
                .enumerate()
                .try_for_each(|(i, x)| self.check_held(verb, addr, &join(&i.to_string()), x)),
            Value::Obj(m) => m
                .iter()
                .try_for_each(|(k, x)| self.check_held(verb, addr, &join(k), x)),
            _ => Ok(()),
        }
    }

    /// The program leaves no attribute the schema requires unset (R-184):
    /// an error at the resource's site, before its provider is asked
    /// (`deployment::plan` adds the site); what the schema does not know
    /// stays the provider's refusal.
    fn check_required(&self, addr: &Address, doc: &Json) -> Result<()> {
        match self.schema().unset_message(&addr.typ, doc) {
            Some(m) => Err(crate::report::Failure::located(addr, m).into()),
            None => Ok(()),
        }
    }

    /// A desired resource's document as its provider takes it: quantities
    /// and times in the schema's render form (R-66), references and nulls
    /// resolved.
    fn desired_doc(&self, ctx: &Ctx, r: &Resource) -> Result<Json> {
        let attrs = self
            .schema()
            .render(&r.addr.typ, &r.attrs)
            .map_err(|(path, why)| {
                crate::report::Failure::located(&r.addr, format!("{path} {why}"))
            })?;
        self.resolve_value(ctx, &attrs)
    }

    fn resolve_value(&self, ctx: &Ctx, v: &Value) -> Result<Json> {
        Ok(match v {
            Value::Str(s) => json!(s),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f.get()),
            Value::Bool(b) => json!(b),
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.resolve_value(ctx, x))
                    .collect::<Result<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Ok((k.clone(), self.resolve_value(ctx, x)?)))
                    .collect::<Result<_>>()?,
            ),
            Value::Ip(n) => json!(crate::value::u32_to_ipv4(*n)),
            Value::IpNet { addr, prefix } => json!(crate::value::ipnet_to_string(*addr, *prefix)),
            Value::Range(r) => json!(r.to_string()),
            Value::Ref { typ, name, attr } => self.resolve_ref(ctx, typ, name, attr)?,
            Value::CloudRef { typ, name, attr } => self.resolve_cloud_ref(typ, name, attr)?,
            Value::Null { label, class, .. } => self.resolve_null(ctx, label, *class)?,
            // Where the schema renders one, it already has (`render`).
            // A uri's host in its A-labels: the provider boundary (R-134).
            Value::Quantity(_)
            | Value::Time(_)
            | Value::Uri(_)
            | Value::Oci(_)
            | Value::Semver(_) => {
                json!(v.wire_text())
            }
        })
    }

    /// The world's side of the comparison: configured attributes, plus the
    /// value the provider picked for an Optional+Computed path the program now
    /// sets.
    fn world_doc(&self, typ: &str, cur: &Object, desired: &Json) -> Json {
        let mut doc = cur.attrs.clone();
        for (attr, _) in self.schema().optional_computed_of(typ) {
            if get_path(desired, &attr).is_some()
                && get_path(&doc, &attr).is_none()
                && let Some(v) = get_path(&cur.computed, &attr)
            {
                set_path(&mut doc, &attr, v.clone());
            }
        }
        doc
    }

    /// The remote names of a create-first replacement of `addr` whose
    /// type's remote name is its provider's (`type_remote_name`, R-189):
    /// the old object's and the one the replacement is given, the next
    /// generation of the program's, when the program's `want` would
    /// collide with the old object's (`was`, its world document, or what
    /// state recorded dform gave it). None when it would not, or the type
    /// has none.
    fn renamed(
        &self,
        addr: &Address,
        want: &Json,
        was: Option<&Json>,
        entry: Option<&StateEntry>,
    ) -> Option<(String, String)> {
        let path = self.schema().remote_name_of(&addr.typ)?;
        let want = get_path(want, path)?.as_str()?;
        let generated = entry.and_then(|e| e.name.as_deref());
        let was = generated
            .or_else(|| was.and_then(|d| get_path(d, path)).and_then(Json::as_str))
            .unwrap_or(want);
        let new = state::next_name(want, was, generated)?;
        Some((was.to_string(), new))
    }

    /// The name state recorded dform gave `addr`'s object, when `doc`
    /// keeps it (`StateEntry::name`): a destroy-first replacement of one
    /// made under a generation is made under it again.
    fn kept_name(&self, addr: &Address, doc: &Json, state: &State) -> Option<String> {
        let path = self.schema().remote_name_of(&addr.typ)?;
        let sent = get_path(doc, path)?.as_str()?;
        state
            .get(addr)
            .and_then(|e| e.name.clone())
            .filter(|n| n == sent)
    }

    /// Plan: refresh, then the Z-set `desired − world` (`zset::deformation`),
    /// then the owning provider's Plan (the diff, and replace when a
    /// `force_new` path changes) for each deformation. Actions come in
    /// dependency order; deletes last, in reverse dependency order (from
    /// the dependencies state recorded), with the objects deposed by a
    /// `create_before_destroy` replacement among them.
    pub fn plan(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
    ) -> Result<Plan> {
        self.plan_retracting(desired, adopts, lifecycle, state, &BTreeSet::new())
    }

    /// `plan`, leaving every null owned by a `retracted` address
    /// unresolved: those addresses are being replaced (`executor`).
    pub fn plan_retracting(
        &self,
        desired: &[Resource],
        adopts: &[Adopt],
        lifecycle: &Lifecycle,
        state: &State,
        retracted: &BTreeSet<Address>,
    ) -> Result<Plan> {
        let world = self.refresh(state)?;
        let adopt_map = state::adopt_map(adopts);
        self.proven.borrow_mut().clear();

        // desired: the assembled documents, refs and nulls resolved as far
        // as the world allows.
        let mut resolved: BTreeMap<Address, Json> = BTreeMap::new();
        let mut order = Vec::new();
        let renaming: BTreeSet<Address> = retracted
            .iter()
            .filter(|a| {
                self.schema().remote_name_of(&a.typ).is_some()
                    && lifecycle.create_first(self.schema(), a)
            })
            .cloned()
            .collect();
        for r in topo_sort(desired)? {
            let ctx = Ctx {
                cloud: self,
                world: &world,
                state,
                adopts: &adopt_map,
                resolved: &resolved,
                strict: None,
                retracted,
                renaming: &renaming,
            };
            self.check_held("plan", &r.addr, "", &r.attrs)?;
            let doc = self.desired_doc(&ctx, &r)?;
            self.check_required(&r.addr, &doc)?;
            order.push(r.addr.clone());
            resolved.insert(r.addr.clone(), doc);
        }

        // world: through the identity mapping. A state entry whose object
        // is gone is no row (a desired one is created again); one no longer
        // desired is deleted from state with nothing to delete.
        let mut before: BTreeMap<Address, Json> = BTreeMap::new();
        let mut kept: BTreeMap<Address, Vec<Change>> = BTreeMap::new();
        for (addr, entry) in self.entries(&state.resources) {
            if let Some(cur) = world.get(&key(&addr.typ, &entry.remote)) {
                let mut doc = match resolved.get(&addr) {
                    Some(d) => self.world_doc(&addr.typ, cur, d),
                    None => cur.attrs.clone(),
                };
                if let Some(want) = resolved.get(&addr) {
                    self.written_before(&addr.typ, entry, want, &mut doc);
                    self.derived_before(&addr, entry, want, &mut doc);
                }
                if let Some(want) = resolved.get_mut(&addr) {
                    let k = self.at_create(&addr, lifecycle, want, &mut doc);
                    if !k.is_empty() {
                        kept.insert(addr.clone(), k);
                    }
                }
                before.insert(addr, doc);
            }
        }

        let flat = |docs: &BTreeMap<Address, Json>| {
            docs.iter()
                .map(|(a, d)| (a.clone(), self.flat_value(&a.typ, d)))
                .collect::<BTreeMap<Address, Value>>()
        };
        let deformations: BTreeMap<Address, zset::Deformation> =
            zset::deformation(&flat(&resolved), &flat(&before))
                .into_iter()
                .map(|d| (d.addr.clone(), d))
                .collect();

        // What each action asks its provider to Plan: every desired
        // document against the world's (an adopt's against the object
        // Import finds; the provider validates every desired document,
        // undeformed ones too), then every delete.
        let mut asks = Vec::new();
        let mut adopting = BTreeSet::new();
        for addr in &order {
            let (remote, prior) = match (deformations[addr].kind, adopt_map.get(addr)) {
                (zset::Kind::Create, Some(remote_name)) if state.get(addr).is_none() => {
                    let Some(found) = self.import(&addr.typ, remote_name)? else {
                        bail!(
                            "adopt requested but inventory missing {}",
                            key(&addr.typ, remote_name)
                        );
                    };
                    adopting.insert(addr.clone());
                    (remote_name.clone(), Some(found.attrs))
                }
                (zset::Kind::Create, _) => (String::new(), None),
                _ => (
                    state
                        .get(addr)
                        .map(|e| e.remote.clone())
                        .unwrap_or_default(),
                    before.get(addr).cloned(),
                ),
            };
            asks.push((addr.clone(), remote, prior, resolved.get(addr).cloned()));
        }
        // Deletes: what is no longer desired, what state maps to a vanished
        // object (no world document: nothing to show), and deposed objects.
        let mut deleting: Vec<(ActionKind, Address, &[String])> = Vec::new();
        for (addr, entry) in self.entries(&state.resources) {
            if resolved.contains_key(&addr) {
                continue;
            }
            asks.push((
                addr.clone(),
                entry.remote.clone(),
                before.get(&addr).cloned(),
                None,
            ));
            deleting.push((ActionKind::Delete, addr, &entry.deps));
        }
        for (addr, entry) in self.entries(&state.deposed) {
            let prior = world.get(&key(&addr.typ, &entry.remote));
            asks.push((
                addr.clone(),
                entry.remote.clone(),
                prior.map(|o| o.attrs.clone()),
                None,
            ));
            deleting.push((ActionKind::DeleteDeposed, addr, &entry.deps));
        }
        let mut answers = self.plan_many(asks)?.into_iter();

        let mut actions = Vec::new();
        for (addr, (changes, replaces)) in order.iter().cloned().zip(answers.by_ref()) {
            let d = &deformations[&addr];
            let kind = match d.kind {
                zset::Kind::Create if adopting.contains(&addr) => ActionKind::Adopt,
                zset::Kind::Create => ActionKind::Create,
                zset::Kind::Delete => unreachable!("a desired address is never deleted"),
                zset::Kind::Update | zset::Kind::Drift if replaces => ActionKind::Replace {
                    create_first: lifecycle.create_first(self.schema(), &addr),
                },
                zset::Kind::Drift => ActionKind::Drift,
                // The provider's Plan finds nothing to change: the world
                // document differs only where the provider compares for
                // itself (a write-only attribute its API never answers,
                // such as an OVH instance's user data).
                zset::Kind::Update if changes.is_empty() => ActionKind::Noop,
                zset::Kind::Update => ActionKind::Update,
                zset::Kind::Pending => ActionKind::Pending,
                zset::Kind::Undeformed => ActionKind::Noop,
            };
            let (changes, on) = match d.kind {
                zset::Kind::Pending => (changes, d.unresolved.clone()),
                zset::Kind::Undeformed => (Vec::new(), BTreeSet::new()),
                _ => (changes, BTreeSet::new()),
            };
            let renamed = match kind {
                ActionKind::Replace { create_first: true } => {
                    if let Some(why) = zset::create_first_refused(self.schema(), &addr) {
                        return Err(crate::report::Failure::located(&addr, why).into());
                    }
                    let want = resolved.get(&addr).expect("a desired address");
                    self.renamed(&addr, want, before.get(&addr), state.get(&addr))
                }
                _ => None,
            };
            actions.push(Action {
                kind,
                addr: addr.clone(),
                changes,
                on,
                kept: kept.remove(&addr).unwrap_or_default(),
                renamed,
            });
        }

        // One delete goes before the deletes of what it depends on.
        let mut deletes: Vec<(Action, &[String])> = deleting
            .into_iter()
            .zip(answers)
            .map(|((kind, addr, deps), (changes, _))| {
                (
                    Action {
                        kind,
                        addr,
                        changes,
                        on: BTreeSet::new(),
                        kept: Vec::new(),
                        renamed: None,
                    },
                    deps,
                )
            })
            .collect();
        deletes.sort_by(|a, b| a.0.addr.cmp(&b.0.addr));
        actions.extend(reverse_dependency_order(deletes));
        Ok(Plan { actions })
    }

    /// A document in the canonical form the Z-set compares: leaf path to
    /// leaf value, keyed lists by key, sets by content (`provider::flatten`),
    /// null and secret markers as labeled nulls of the schema's class. A
    /// secret held by another stack's object is where it is held and its
    /// digest: what the reader knows of it, definite, so a changed one
    /// updates and an unchanged one is no change.
    fn flat_value(&self, typ: &str, doc: &Json) -> Value {
        let mut leaves = BTreeMap::new();
        // A set's elements by content: one added beside the world's is
        // an element more, whatever place it sorts to (R-158).
        provider::flatten(self.schema(), typ, doc, "", "", true, &mut leaves);
        Value::Obj(
            leaves
                .into_iter()
                .map(|(p, (v, _))| {
                    let v = match provider::marker(&v) {
                        Some((provider::NULL_KEY, label)) => Value::Null {
                            label: label.to_string(),
                            class: self.null_class(label),
                            ty: String::new(),
                        },
                        Some((_, label)) => match provider::held(&v) {
                            Some(h) => Value::Str(format!(
                                "(sensitive {label} held by {} {} {}/{}#{} {})",
                                h.provider, h.deployment, h.typ, h.remote, h.path, h.digest
                            )),
                            None => Value::Null {
                                label: label.to_string(),
                                class: NullClass::Secret,
                                ty: String::new(),
                            },
                        },
                        None => provider::json_to_value(&v),
                    };
                    (p, v)
                })
                .collect(),
        )
    }

    /// The class of the null labeled `T/A#P`: the schema's, else open (a
    /// ref to a configured attribute nobody set).
    /// Whether the null labeled `label` is a secret a provider holds,
    /// which no evaluation fills (R-218): a resource's sensitive computed
    /// value, an extern's secret column, another stack's held output.
    pub fn holds_secret(&self, label: &str) -> bool {
        self.held.borrow().contains_key(label) || self.null_class(label) == NullClass::Secret
    }

    fn null_class(&self, label: &str) -> NullClass {
        let Some((typ, _, path)) = crate::value::null_parts(label) else {
            return NullClass::Open;
        };
        self.schema()
            .class_of(&typ, &path)
            .or_else(|| self.schema().optional_computed_class(&typ, &path))
            .unwrap_or(NullClass::Open)
    }

    /// Open one tick for the executor's per-action Apply calls
    /// (`executor::run_tick`).
    pub fn begin_tick<'a>(
        &'a self,
        desired: &'a [Resource],
        adopts: &[Adopt],
        lifecycle: &'a Lifecycle,
    ) -> Result<Tick<'a>> {
        Ok(Tick {
            lifecycle,
            cloud: self,
            world: None,
            adopt_map: state::adopt_map(adopts),
            desired: desired.iter().map(|r| (r.addr.clone(), r)).collect(),
            resolved: BTreeMap::new(),
            timeline: Vec::new(),
            returned: BTreeMap::new(),
            elapsed: BTreeMap::new(),
            in_flight: Vec::new(),
        })
    }
}

/// What a ref resolves against: the world (refreshed at plan, live at
/// apply), the identity mapping, and the desired documents resolved so far
/// in dependency order.
struct Ctx<'a> {
    cloud: &'a Providers,
    world: &'a World,
    state: &'a State,
    adopts: &'a BTreeMap<Address, String>,
    resolved: &'a BTreeMap<Address, Json>,
    /// Apply: a null that cannot be filled is an error naming the resource.
    strict: Option<&'a Address>,
    /// Plan: addresses being replaced, whose nulls stay unresolved (the
    /// replacement is a new object).
    retracted: &'a BTreeSet<Address>,
    /// Plan: those of them replaced create-first under a name dform gives
    /// (R-189): a reference to the name is unknown until the replacement
    /// is made, as a null of theirs is.
    renaming: &'a BTreeSet<Address>,
}

impl Ctx<'_> {
    /// The object behind an address, through the identity mapping or an
    /// adopt (Imported: it may not be the stack's yet).
    fn existing(&self, addr: &Address) -> Result<Option<Object>> {
        if let Some(e) = self.state.get(addr) {
            return Ok(self.world.get(&key(&addr.typ, &e.remote)).cloned());
        }
        let Some(rn) = self.adopts.get(addr) else {
            return Ok(None);
        };
        match self.world.get(&key(&addr.typ, rn)) {
            Some(o) => Ok(Some(o.clone())),
            None => self.cloud.import(&addr.typ, rn),
        }
    }
}

/// One tick, open for Apply calls. The executor decides the order; each
/// call is one provider Apply, submitted (`submit`) and answered later
/// (`next_completed`), and what it returns is the tick's view of the world
/// for the calls submitted after it.
pub struct Tick<'a> {
    cloud: &'a Providers,
    lifecycle: &'a Lifecycle,
    /// The refresh the tick starts from, overlaid with what its calls
    /// returned; read at the first call.
    world: Option<World>,
    adopt_map: BTreeMap<Address, String>,
    desired: BTreeMap<Address, &'a Resource>,
    /// Documents applied so far this tick, for configured-attribute refs.
    resolved: BTreeMap<Address, Json>,
    /// The tick's Apply calls on the executor's clock.
    timeline: Vec<pb::Span>,
    /// What each answered Apply call returned: the object's configured
    /// attributes, `None` for a delete.
    returned: BTreeMap<Address, Option<Json>>,
    /// How long each call took on its provider's clock.
    elapsed: BTreeMap<Address, u64>,
    /// The Apply calls in flight, in the order they were submitted.
    in_flight: Vec<InFlight>,
}

/// One submitted Apply call, and what its answer is recorded against.
struct InFlight {
    /// The executor's name for the call.
    id: usize,
    link: usize,
    ticket: Ticket,
    kind: ActionKind,
    addr: Address,
    /// The object's remote id before the call; empty for a create.
    remote: String,
    /// A create's or a replace's remote name, when it is not the
    /// program's (`StateEntry::name`).
    named: Option<String>,
    /// The call, to send again when it failed in a way worth retrying.
    req: pb::ApplyRequest,
    /// How many times it has been sent again.
    retried: u32,
}

/// The strings inside a document value.
fn strings(v: &Json) -> Vec<&str> {
    match v {
        Json::String(s) => vec![s.as_str()],
        Json::Array(xs) => xs.iter().flat_map(strings).collect(),
        Json::Object(m) => m.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// `now`'s entries at `path` or under it as `was` has them: what an
/// update that did not send `path` leaves recorded of it.
fn keep_under(now: &mut BTreeMap<String, String>, was: &BTreeMap<String, String>, path: &str) {
    let under = |k: &str| {
        k.strip_prefix(path)
            .is_some_and(|r| r.is_empty() || r.starts_with('.') || r.starts_with('['))
    };
    now.retain(|k, _| !under(k));
    now.extend(
        was.iter()
            .filter(|(k, _)| under(k))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
}

/// An Apply call dform does not send, and why (R-109): `apply T A: not
/// sent`, the reason on its own line.
fn not_sent(addr: &Address, why: &str) -> anyhow::Error {
    crate::report::Failure::of("apply", addr, "not sent", why).into()
}

/// An Apply call not sent because the secret `label` at `path` of its
/// document was not revealed (R-218): why, as the holder said it.
fn unrevealed(addr: &Address, path: &str, label: &str, e: &anyhow::Error) -> anyhow::Error {
    let why = e
        .chain()
        .last()
        .map(ToString::to_string)
        .unwrap_or_default();
    not_sent(
        addr,
        &format!(
            "{} holds the secret {}, which was not revealed: {why}",
            crate::report::attribute(addr, path),
            crate::ir::label(label)
        ),
    )
}

impl Tick<'_> {
    /// Submit the Apply call for `a`, as the executor's call `id`. Returns
    /// whether a call is in flight; an action that needs none (the delete
    /// of an object state no longer maps) is answered at once.
    pub fn submit(&mut self, id: usize, a: &Action, state: &mut State) -> Result<bool> {
        let cloud = self.cloud;
        let addr = &a.addr;
        if matches!(a.kind, ActionKind::Noop | ActionKind::Pending) {
            return Ok(false);
        }
        if self.world.is_none() {
            self.world = Some(cloud.refresh(state)?);
        }
        // The remote name dform gives a create-first replacement.
        let mut generated = None;
        let doc = match a.kind {
            ActionKind::Delete | ActionKind::DeleteDeposed => Json::Null,
            _ => {
                let r = self
                    .desired
                    .get(addr)
                    .ok_or_else(|| not_sent(addr, "no desired resource"))?;
                let ctx = Ctx {
                    cloud,
                    world: self.world.as_ref().expect("read above"),
                    state,
                    adopts: &self.adopt_map,
                    resolved: &self.resolved,
                    strict: Some(addr),
                    retracted: &BTreeSet::new(),
                    renaming: &BTreeSet::new(),
                };
                let mut doc = cloud.desired_doc(&ctx, r)?;
                if let ActionKind::Replace { create_first: true } = a.kind {
                    generated = self.name_replacement(addr, &mut doc, state);
                }
                self.resolved.insert(addr.clone(), doc.clone());
                doc
            }
        };
        // The name dform gave the object this call makes, which state
        // records (`StateEntry::name`).
        let named = match a.kind {
            ActionKind::Create | ActionKind::Replace { .. } => {
                generated.or_else(|| cloud.kept_name(addr, &doc, state))
            }
            _ => None,
        };
        let world = self.world.as_ref().expect("read above");
        // The paths an update asks its provider to leave as they are.
        let mut keep = Vec::new();
        let (op, remote, config, create_first) = match a.kind {
            ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {
                unreachable!("returned above")
            }
            ActionKind::Delete => match state.get(addr) {
                None => {
                    self.answered(a.kind.clone(), addr, &Ok(None), state)?;
                    return Ok(false);
                }
                Some(e) => (pb::Op::Delete, e.remote.clone(), None, false),
            },
            ActionKind::DeleteDeposed => match state.deposed.get(&state::key(addr)) {
                None => {
                    self.answered(a.kind.clone(), addr, &Ok(None), state)?;
                    return Ok(false);
                }
                Some(e) => (pb::Op::Delete, e.remote.clone(), None, false),
            },
            ActionKind::Create => (pb::Op::Create, String::new(), Some(doc), false),
            ActionKind::Replace { create_first } => {
                let Some(old) = state.get(addr).map(|e| e.remote.clone()) else {
                    return Err(not_sent(addr, "replace without a state entry"));
                };
                (pb::Op::Replace, old, Some(doc), create_first)
            }
            ActionKind::Adopt => {
                let Some(remote_name) = self.adopt_map.get(addr).cloned() else {
                    return Err(not_sent(addr, "adopt action missing adopt mapping"));
                };
                (pb::Op::Adopt, remote_name, Some(doc), false)
            }
            ActionKind::Update | ActionKind::Drift => {
                let Some(remote) = state.get(addr).map(|e| e.remote.clone()) else {
                    return Err(not_sent(addr, "update without a state entry"));
                };
                let mut doc = doc;
                // What is given at creation only: the world keeps its
                // value, or its absence; a write-only one, which the world
                // never answers, its provider leaves as the object has it
                // when it can (R-198).
                if let Some(cur) = world.get(&key(&addr.typ, &remote)) {
                    let (keeps, wo) = (
                        cloud.keeps(&addr.typ),
                        cloud.schema().write_only_of(&addr.typ),
                    );
                    for (p, _) in self.lifecycle.at_create_of(addr) {
                        match get_path(&cur.attrs, p) {
                            Some(v) => set_path(&mut doc, p, v.clone()),
                            None if keeps
                                && wo.contains(&p.as_str())
                                && get_path(&doc, p).is_some() =>
                            {
                                remove_path(&mut doc, p);
                                keep.push(p.clone());
                            }
                            None => remove_path(&mut doc, p),
                        }
                    }
                }
                // A run that does not hold the master (R-164): a leaf it
                // proved unchanged holds a stand-in; the world's own value
                // goes in its place, as if only the changes were sent. One
                // the world does not answer (a write-only one) is kept by a
                // provider that can leave it as it is.
                if crate::secrets::standin::active()
                    && let Some(e) = state.get(addr)
                {
                    let cur = world.get(&key(&addr.typ, &remote));
                    let keeps = cloud.keeps(&addr.typ);
                    for p in e.derived.keys() {
                        if !get_path(&doc, p).is_some_and(crate::secrets::standin::carries) {
                            continue;
                        }
                        match cur.and_then(|c| get_path(&c.attrs, p)) {
                            Some(v) if provider::marker(v).is_none() => {
                                set_path(&mut doc, p, v.clone())
                            }
                            _ if keeps => {
                                remove_path(&mut doc, p);
                                keep.push(p.clone());
                            }
                            _ => {
                                return Err(not_sent(
                                    addr,
                                    &format!(
                                        "{} holds a value only the deployment's master \
                                         derives, and the world does not answer it",
                                        crate::report::attribute(addr, p)
                                    ),
                                ));
                            }
                        }
                    }
                }
                (pb::Op::Update, remote, Some(doc), false)
            }
        };
        // Refinements on sensitive paths: the provider checks each after
        // materializing the secret (F DR-13 revised).
        let assertions: Vec<pb::Assertion> = match config {
            None => Vec::new(),
            Some(_) => self
                .lifecycle
                .assertions
                .get(addr)
                .into_iter()
                .flatten()
                .map(|(path, c)| {
                    let (op, value) = crate::refine::to_assertion(c);
                    pb::Assertion {
                        path: path.clone(),
                        op,
                        value: Some(wire::doc(&value)),
                        message: format!(
                            "{} fails its refinement {c}",
                            crate::report::attribute(addr, path)
                        ),
                    }
                })
                .collect(),
        };
        // A stand-in never leaves the process (R-164): the executor is not
        // given an action that would send one ([`Providers::needs_master`]).
        if config
            .as_ref()
            .is_some_and(crate::secrets::standin::carries)
        {
            return Err(not_sent(
                addr,
                "its document holds a value only the deployment's master derives, which this \
                 run does not hold",
            ));
        }
        let config = match config {
            Some(mut doc) => {
                cloud.reveal_into(addr, &mut doc, state)?;
                Some(doc)
            }
            None => None,
        };
        let idempotency_key = uncertain_from_here(a, addr, &remote, named.clone(), state);
        let req = pb::ApplyRequest {
            op: op as i32,
            r#type: addr.typ.clone(),
            name: addr.name.clone(),
            remote: remote.clone(),
            config: config.as_ref().map(wire::doc),
            create_first,
            assertions,
            spans: Vec::new(),
            idempotency_key,
            keep,
        };
        let link = cloud.route(&addr.typ);
        let ticket = cloud.links[link].borrow_mut().submit(req.clone());
        cloud.invalidate();
        self.in_flight.push(InFlight {
            id,
            link,
            ticket,
            kind: a.kind.clone(),
            addr: addr.clone(),
            remote,
            named,
            req,
            retried: 0,
        });
        Ok(true)
    }

    /// Give a create-first replacement of `addr` the next generation of
    /// its remote name in `doc`, where the program's would be the old
    /// object's (`Providers::renamed`): the two exist at once until the
    /// old one is deleted, a tick later. Returns the name given.
    fn name_replacement(&self, addr: &Address, doc: &mut Json, state: &State) -> Option<String> {
        let world = self.world.as_ref().expect("read at submit");
        let entry = state.get(addr);
        let was = entry
            .and_then(|e| world.get(&key(&addr.typ, &e.remote)))
            .map(|o| &o.attrs);
        let (_, new) = self.cloud.renamed(addr, doc, was, entry)?;
        let path = self.cloud.schema().remote_name_of(&addr.typ)?;
        set_path(doc, path, json!(new));
        Some(new)
    }

    /// Whether an Apply call is in flight.
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Take the next answered Apply call and record it: identity in `state`
    /// when the call answered; a call that may have taken effect without
    /// answering (a timeout) records none. Returns the call's id and its
    /// outcome. An answer a link already holds comes first; else the link
    /// of the oldest call in flight is waited on. What a call in flight
    /// says meanwhile goes to `events` with its call's id (R-130).
    pub fn next_completed(
        &mut self,
        state: &mut State,
        events: &mut dyn FnMut(usize, pb::Event),
    ) -> (usize, Result<()>) {
        let cloud = self.cloud;
        let (f, answer) = loop {
            let (mut f, answer) = self.take_answer(events);
            match self.again(&mut f, answer) {
                None => self.in_flight.push(f),
                Some(answer) => break (f, answer),
            }
        };
        cloud.invalidate();
        let result = answer.and_then(|r| {
            cloud.links[f.link]
                .borrow()
                .expect::<pb::ApplyResponse>("Apply", r)
        });
        let result = match result.map(|resp| self.record_object(&f, resp, state)) {
            Ok(Ok(resp)) => Ok(Some(resp)),
            Ok(Err(e)) => return (f.id, Err(e)),
            Err(e) => Err(e),
        };
        self.forget_object(&f, &result, state);
        if !matches!(
            result,
            Err(CallError::MaybeApplied(_) | CallError::Crashed(_))
        ) {
            state.uncertain.remove(&uncertain_key(&f.kind, &f.addr));
        }
        (f.id, self.answered(f.kind, &f.addr, &result, state))
    }

    /// The next answered Apply call, out of `in_flight`: an answer a link
    /// already holds comes first; else the link of the oldest call in
    /// flight is waited on.
    fn take_answer(
        &mut self,
        events: &mut dyn FnMut(usize, pb::Event),
    ) -> (InFlight, Result<Reply, CallError>) {
        let cloud = self.cloud;
        assert!(self.busy(), "internal: no Apply call in flight");
        let in_flight = &self.in_flight;
        // The call an event is of, by its link and ticket; an event of a
        // call no longer in flight is dropped.
        let mut tell = |l: usize, t: Ticket, e: pb::Event| {
            if let Some(f) = in_flight.iter().find(|f| f.link == l && f.ticket == t) {
                // Its link is in a call: named by the address alone.
                if crate::timing::enabled() {
                    let says: Vec<&str> = [&e.status, &e.message]
                        .into_iter()
                        .flatten()
                        .map(String::as_str)
                        .collect();
                    crate::timing::line(
                        &format!(
                            "apply {}: {}",
                            crate::report::address(&f.addr),
                            says.join(": ")
                        ),
                        std::time::Duration::ZERO,
                    );
                }
                events(f.id, e);
            }
        };
        // Those kept while a link waited for another call come first.
        let links: BTreeSet<usize> = in_flight.iter().map(|f| f.link).collect();
        for l in links {
            for (t, e) in cloud.links[l].borrow_mut().take_events() {
                tell(l, t, e);
            }
        }
        let held = self
            .in_flight
            .iter()
            .position(|f| cloud.links[f.link].borrow().has_answer(f.ticket));
        let (k, answer) = match held {
            Some(k) => {
                let f = &self.in_flight[k];
                let r = cloud.links[f.link].borrow_mut().take_answer(f.ticket);
                (k, r.expect("held"))
            }
            None => {
                let l = self.in_flight[0].link;
                let (t, r) = cloud.links[l]
                    .borrow_mut()
                    .next_completed(&mut |t, e| tell(l, t, e));
                let k = self
                    .in_flight
                    .iter()
                    .position(|f| f.link == l && f.ticket == t)
                    .expect("internal: an answer to a call nobody made");
                (k, r)
            }
        };
        (self.in_flight.remove(k), answer)
    }

    /// A call that failed in a way worth trying again (a transient
    /// refusal: nothing changed, R-81) is sent again after its backoff,
    /// while its provider's budget lasts: `None`, `f` in flight again.
    /// Anything else is its answer, the budget's end saying so.
    fn again(
        &mut self,
        f: &mut InFlight,
        answer: Result<Reply, CallError>,
    ) -> Option<Result<Reply, CallError>> {
        let e = match answer {
            Err(e) if policy::class(&e) == Class::Retryable => e,
            // No answer within its timeout: what the call did is looked up
            // before it is sent again (R-81).
            Err(e) if timed_out(&e) => match self.look(f) {
                Looked::Made(reply) => return Some(Ok(*reply)),
                Looked::NotMade => e,
                Looked::Unknown(why) => {
                    return Some(Err(CallError::MaybeApplied(format!("{e}; {why}"))));
                }
            },
            answer => return Some(answer),
        };
        let mut link = self.cloud.links[f.link].borrow_mut();
        let budget = link.policy().retries;
        if f.retried >= budget {
            return Some(Err(super::link::gave_up(e, budget)));
        }
        f.retried += 1;
        let delay = link.policy().delay(f.retried);
        let retry = Retry {
            provider: link.name_or_program().to_string(),
            call: policy::describe(&Call::Apply(f.req.clone())),
            attempt: f.retried,
            of: budget,
            delay,
            // The provider's own naming of the change dropped (R-109).
            error: crate::report::said_of(&f.addr, &e.to_string()),
        };
        link.retried(retry);
        std::thread::sleep(delay);
        f.ticket = link.submit(f.req.clone());
        None
    }

    /// What an Apply call that timed out did (R-81), so that it is sent
    /// again only when that is safe: a Create is looked up by its
    /// idempotency key (`created`), and the object it made is the call's
    /// answer, adopted, never made twice; a Delete by a Read. An Update
    /// sends the same document again, an Adopt names the same object. A
    /// Replace, or a Create whose provider cannot say what a key made, is
    /// not sent again: the next apply resolves it (`State::uncertain`).
    fn look(&self, f: &InFlight) -> Looked {
        let cloud = self.cloud;
        let at = &f.addr;
        match f.kind {
            ActionKind::Update | ActionKind::Drift | ActionKind::Adopt => Looked::NotMade,
            ActionKind::Create => {
                let key = &f.req.idempotency_key;
                let unknown = || {
                    Looked::Unknown(format!(
                        "the provider {} cannot say what its idempotency key made, so it is \
                         not sent again: the next apply looks",
                        cloud.names[f.link]
                    ))
                };
                if key.is_empty() || !cloud.links[f.link].borrow().has("managed") {
                    return unknown();
                }
                let remote = match cloud.created(at, key) {
                    Ok(Some(remote)) => remote,
                    Ok(None) => return Looked::NotMade,
                    Err(e) => return Looked::Unknown(format!("looking it up failed: {e:#}")),
                };
                match cloud.read(f.link, at, &remote) {
                    Ok(Some(o)) => {
                        crate::progress::line(&format!(
                            "apply {}: the Create that timed out made {remote}; it is \
                             adopted, not made again",
                            crate::report::address(at)
                        ));
                        Looked::Made(Box::new(Reply::Apply(pb::ApplyResponse {
                            remote,
                            attrs: Some(wire::doc(&o.attrs)),
                            computed: Some(wire::doc(&o.computed)),
                            ..Default::default()
                        })))
                    }
                    Ok(None) => unknown(),
                    Err(e) => Looked::Unknown(format!("reading it failed: {e:#}")),
                }
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                match cloud.read(f.link, at, &f.remote) {
                    Ok(None) => {
                        crate::progress::line(&format!(
                            "apply {}: the Delete that timed out took effect; {} is gone",
                            crate::report::address(at),
                            f.remote
                        ));
                        Looked::Made(Box::new(Reply::Apply(pb::ApplyResponse::default())))
                    }
                    Ok(Some(_)) => Looked::NotMade,
                    Err(e) => Looked::Unknown(format!("reading it failed: {e:#}")),
                }
            }
            ActionKind::Replace { .. }
            | ActionKind::Noop
            | ActionKind::Pending
            | ActionKind::Forget => {
                Looked::Unknown("a replace is not sent again: the next apply looks".into())
            }
        }
    }

    /// An answered call's object, in the tick's world and in state.
    fn record_object(
        &mut self,
        f: &InFlight,
        resp: pb::ApplyResponse,
        state: &mut State,
    ) -> Result<pb::ApplyResponse> {
        let addr = &f.addr;
        let cloud = self.cloud;
        let provider = &cloud.names[f.link];
        let world = self.world.as_mut().expect("read at submit");
        match f.kind {
            ActionKind::Create | ActionKind::Adopt | ActionKind::Replace { .. } => {
                if let ActionKind::Replace { .. } = f.kind {
                    self.forget_old(f, state);
                }
                let world = self.world.as_mut().expect("read at submit");
                world.insert(
                    key(&addr.typ, &resp.remote),
                    Providers::object(resp.attrs.as_ref(), resp.computed.as_ref())?,
                );
                state.set(addr, provider.clone(), resp.remote.clone());
                state.set_name(addr, f.named.clone());
                self.record_written(addr, state, true);
            }
            ActionKind::Update | ActionKind::Drift => {
                world.insert(
                    key(&addr.typ, &f.remote),
                    Providers::object(resp.attrs.as_ref(), resp.computed.as_ref())?,
                );
                self.record_written(addr, state, false);
            }
            ActionKind::Delete => {
                world.remove(&key(&addr.typ, &f.remote));
                state.remove(addr);
            }
            ActionKind::DeleteDeposed => {
                world.remove(&key(&addr.typ, &f.remote));
                state.deposed.remove(&state::key(addr));
            }
            ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {}
        }
        Ok(resp)
    }

    /// The digests of the write-only attributes `addr`'s object was just
    /// applied with (R-106), in state beside it; of one just `made`, the
    /// secret generations its values given at creation only read
    /// (R-198). An update sent none of those: what state had of them
    /// stays.
    fn record_written(&self, addr: &Address, state: &mut State, made: bool) {
        let Some(doc) = self.resolved.get(addr) else {
            return;
        };
        let was = state.get(addr).cloned();
        let before = was.as_ref().map(|e| e.written.clone()).unwrap_or_default();
        let mut written = self.cloud.written(&addr.typ, doc, &before);
        let mut derived = self.cloud.derivations(doc);
        let given: Vec<&String> = self.lifecycle.at_create_of(addr).map(|(p, _)| p).collect();
        match (made, was) {
            (true, _) => {
                let made_with = given
                    .iter()
                    .filter_map(|p| {
                        let mut gens = BTreeMap::new();
                        for text in get_path(doc, p).map(strings).unwrap_or_default() {
                            gens.extend(crate::functions::random::generations_in(text));
                        }
                        (!gens.is_empty()).then(|| ((*p).clone(), gens))
                    })
                    .collect();
                state.set_made_with(addr, made_with);
            }
            (false, Some(was)) => {
                for p in given {
                    keep_under(&mut written, &was.written, p);
                    keep_under(&mut derived, &was.derived, p);
                }
            }
            (false, None) => {}
        }
        state.set_written(addr, written, derived);
    }

    /// A call that may have taken effect without answering: what it
    /// removes from the world is gone, and a replacement's old object is
    /// deposed or gone.
    fn forget_object(
        &mut self,
        f: &InFlight,
        result: &std::result::Result<Option<pb::ApplyResponse>, CallError>,
        state: &mut State,
    ) {
        if !matches!(result, Err(CallError::MaybeApplied(_))) {
            return;
        }
        match f.kind {
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                let world = self.world.as_mut().expect("read at submit");
                world.remove(&key(&f.addr.typ, &f.remote));
            }
            ActionKind::Replace { .. } => self.forget_old(f, state),
            _ => {}
        }
    }

    /// The old object of a replacement: it stays, deposed, until it is
    /// deleted after what depends on it has moved to the new one; or it
    /// went first.
    fn forget_old(&mut self, f: &InFlight, state: &mut State) {
        let ActionKind::Replace { create_first } = f.kind else {
            return;
        };
        if create_first {
            state.depose(&f.addr);
        } else {
            let world = self.world.as_mut().expect("read at submit");
            world.remove(&key(&f.addr.typ, &f.remote));
            state.remove(&f.addr);
        }
    }

    /// What every answered call records: its time, its notes, the
    /// dependencies of what it applied, and what it returned.
    fn answered(
        &mut self,
        kind: ActionKind,
        addr: &Address,
        result: &std::result::Result<Option<pb::ApplyResponse>, CallError>,
        state: &mut State,
    ) -> Result<()> {
        if let Ok(Some(resp)) = result {
            self.elapsed.insert(addr.clone(), resp.elapsed_ms);
            self.cloud
                .notes
                .borrow_mut()
                .extend(resp.notes.iter().cloned());
        }
        let deps = self.desired.get(addr).map(|r| r.deps.clone());
        if let (Some(deps), Ok(_) | Err(CallError::MaybeApplied(_))) = (deps, result) {
            state.set_deps(addr, deps);
        }
        if result.is_ok() && !matches!(kind, ActionKind::DeleteDeposed) {
            let world = self.world.as_ref().expect("read at submit");
            let now = state
                .get(addr)
                .and_then(|e| world.get(&key(&addr.typ, &e.remote)))
                .map(|o| o.attrs.clone());
            self.returned.insert(addr.clone(), now);
        }
        // In one shape (R-109): the change as the plan prints it and
        // what happened, then the provider's message, its own naming of
        // the change dropped.
        let happened = match result {
            Ok(_) => return Ok(()),
            Err(CallError::Refused(_)) => "refused, nothing changed",
            Err(CallError::MaybeApplied(_)) => "no answer; the change may have taken effect",
            Err(CallError::Crashed(_)) => "the provider died; the change may have taken effect",
        };
        let m = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        Err(crate::report::Failure::of("apply", addr, happened, &m).into())
    }

    /// How long the Apply call for `addr` took on its provider's clock
    /// (the mock's chaos `latency`, else no time).
    pub fn latency(&self, addr: &Address) -> u64 {
        self.elapsed.get(addr).copied().unwrap_or(0)
    }

    /// Record an Apply call's span on the executor's clock.
    pub fn record(&mut self, addr: &Address, start_ms: u64, end_ms: u64) {
        self.timeline.push(pb::Span {
            addr: addr.to_string(),
            start_ms,
            end_ms,
        });
    }

    /// The tick ends: state records every desired object's dependencies,
    /// and every provider still running hears the boundary (the mock's
    /// clock advances and its chaos mutations land). Returns what the
    /// tick's answered Apply calls returned. Every call is answered by now.
    pub fn end(self, state: &mut State) -> Result<BTreeMap<Address, Option<Json>>> {
        assert!(!self.busy(), "internal: the tick ends with calls in flight");
        // What each object depends on, for ordering its delete later.
        for (addr, r) in &self.desired {
            state.set_deps(addr, r.deps.iter().cloned());
        }
        for link in &self.cloud.links {
            let mut link = link.borrow_mut();
            if link.is_dead() {
                continue;
            }
            let req = pb::ApplyRequest {
                op: pb::Op::EndTick as i32,
                spans: self.timeline.clone(),
                ..Default::default()
            };
            let r: pb::ApplyResponse = link.call(req)?;
            self.cloud.notes.borrow_mut().extend(r.notes);
        }
        self.cloud.invalidate();
        Ok(self.returned)
    }
}

/// What an Apply call that timed out did ([`Tick::look`]).
enum Looked {
    /// It took effect: this is its answer.
    Made(Box<Reply>),
    /// It did not: send it again.
    NotMade,
    /// Nobody can say; why.
    Unknown(String),
}

/// `State::uncertain`'s key for a call of `kind` on `addr`.
fn uncertain_key(kind: &ActionKind, addr: &Address) -> String {
    match kind {
        ActionKind::DeleteDeposed => state::deposed_key(addr),
        _ => state::key(addr),
    }
}

/// A call is uncertain from its submission until it answers: record it,
/// and return the idempotency key a create or a replace carries (the one
/// `executor::mark_creates` gave it before the tick, else a new one).
fn uncertain_from_here(
    a: &Action,
    addr: &Address,
    remote: &str,
    name: Option<String>,
    state: &mut State,
) -> String {
    use state::UncertainOp as Op;
    let op = match a.kind {
        ActionKind::Create => Op::Create,
        ActionKind::Replace { create_first } => Op::Replace { create_first },
        ActionKind::Update | ActionKind::Drift => Op::Update,
        ActionKind::Delete => Op::Delete,
        ActionKind::DeleteDeposed => Op::DeleteDeposed,
        ActionKind::Adopt | ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {
            return String::new();
        }
    };
    let k = uncertain_key(&a.kind, addr);
    let key = match op {
        Op::Create | Op::Replace { .. } => state
            .uncertain
            .get(&k)
            .map(|u| u.key.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| state.new_idempotency_key("", addr, a)),
        _ => String::new(),
    };
    state.uncertain.insert(
        k,
        state::Uncertain {
            op,
            remote: remote.to_string(),
            key: key.clone(),
            name,
        },
    );
    key
}

/// Deletes in reverse dependency order: an object goes before every object
/// at an address it depended on. Ties keep address order.
/// A provider_config value as the provider receives it, if it is known:
/// no null, no ref to resolve.
fn known_json(v: &Value) -> Option<Json> {
    Some(match v {
        Value::Str(s) => json!(s),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f.get()),
        Value::Bool(b) => json!(b),
        Value::List(xs) => Json::Array(xs.iter().map(known_json).collect::<Option<_>>()?),
        Value::Obj(m) => Json::Object(
            m.iter()
                .map(|(k, x)| Some((k.clone(), known_json(x)?)))
                .collect::<Option<_>>()?,
        ),
        Value::Ip(n) => json!(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => json!(crate::value::ipnet_to_string(*addr, *prefix)),
        Value::Range(r) => json!(r.to_string()),
        Value::Quantity(_) | Value::Time(_) | Value::Uri(_) | Value::Oci(_) | Value::Semver(_) => {
            json!(v.wire_text())
        }
        Value::Ref { .. } | Value::CloudRef { .. } | Value::Null { .. } => return None,
    })
}

fn reverse_dependency_order(mut deletes: Vec<(Action, &[String])>) -> Vec<Action> {
    let mut out = Vec::new();
    while !deletes.is_empty() {
        // Ready: nothing still to be deleted depends on it.
        let ready = (0..deletes.len())
            .find(|&i| {
                let k = state::key(&deletes[i].0.addr);
                !deletes
                    .iter()
                    .enumerate()
                    .any(|(j, (_, deps))| j != i && deps.contains(&k))
            })
            // A cycle cannot come from a DAG; break it in address order.
            .unwrap_or(0);
        out.push(deletes.remove(ready).0);
    }
    out
}

fn topo_sort(desired: &[Resource]) -> Result<Vec<Resource>> {
    let mut by_addr: BTreeMap<Address, &Resource> = BTreeMap::new();
    for r in desired {
        by_addr.insert(r.addr.clone(), r);
    }

    let mut pending: BTreeSet<Address> = by_addr.keys().cloned().collect();
    let mut done: BTreeSet<Address> = BTreeSet::new();
    let mut out = Vec::new();

    let mut guard = 0usize;
    while !pending.is_empty() {
        guard += 1;
        if guard > 10_000 {
            bail!("dependency resolution did not converge");
        }
        let mut progressed = false;
        let snapshot: Vec<Address> = pending.iter().cloned().collect();
        for a in snapshot {
            let r = by_addr.get(&a).unwrap();
            if r.deps
                .iter()
                .all(|d| done.contains(d) || !by_addr.contains_key(d))
            {
                pending.remove(&a);
                done.insert(a.clone());
                out.push((*r).clone());
                progressed = true;
            }
        }
        if !progressed {
            bail!("dependency cycle detected");
        }
    }
    Ok(out)
}
