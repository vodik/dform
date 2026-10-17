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

mod apply;
mod checks;
mod planning;
mod query;
mod reveal;
mod schema;
mod settings;
mod start;
mod world;
mod written;

pub use apply::{Answer, Part, Tick};
pub use query::Asked;
use schema::{Loaded, Share};
pub use start::Block;
use start::reconfigure;

use super::backend::{Call, Reply, Session, Ticket};
use super::link::{Link, Retry};
use crate::address::Address;
use crate::provider::{self};
use crate::state::{self, StateEntry};
use anyhow::{Result, anyhow};
use serde_json::Value as Json;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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
    /// The mock's world of each deployment a held secret names, or may
    /// name once its plan earlier in this run is applied (a tree's
    /// upstream): its objects are there, not in this run's world.
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
    /// The deployment's run's secrets ([`Providers::secrets`]).
    pub secrets: Rc<crate::secrets::Run>,
}

impl Config {
    /// What dform.toml grants the provider at `spec`: its entry's, by
    /// spec or by the file both name (a `use`'s `source` is spelled from
    /// its file's directory).
    fn grants_of(&self, spec: &str) -> Option<&super::host::Grants> {
        self.grants.get(spec).or_else(|| {
            self.grants
                .iter()
                .find(|(s, _)| super::source::same_file(s, spec))
                .map(|(_, g)| g)
        })
    }
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
    /// Each link: its own, or the tree's pool's ([`super::pool`]).
    links: Vec<Rc<RefCell<Link>>>,
    /// Each link's session that these providers' Configure opened: what
    /// every call of theirs to it names, so a link the tree's deployments
    /// share answers each deployment's calls with its own configuration.
    sessions: Vec<Session>,
    /// Of each link the pool shares, its entry ([`super::pool`]): its
    /// Schema answer, asked once for the tree.
    pooled: Vec<Option<std::sync::Arc<super::pool::Entry>>>,
    /// The pool, when the run is a tree's.
    pool: Option<std::sync::Arc<super::pool::Pool>>,
    /// The Configure each link's session was last sent: what its Schema
    /// answer is asked under. Held in memory only, as the call was (a
    /// revealed setting among it is never written or printed).
    sent: RefCell<BTreeMap<usize, Json>>,
    /// Each provider's name, as its handshake gave it.
    names: Vec<String>,
    /// The providers' Schema answers: loaded at start, or, for a run that
    /// scopes them to the types it names, by [`Providers::load_schema`].
    loaded: OnceCell<Loaded>,
    /// The provider for a type nobody declares: the mock, else the first.
    fallback: usize,
    /// What each object's Read answered since the last write, by object,
    /// with the link that answered it: one run reads an object once
    /// between two writes, so every consumer of a refresh (round 0, the
    /// plan, the executor's comparison) sees the same Reads, and a
    /// provider configured mid-plan adds its own objects' Reads, not
    /// every object's again. A Read that failed is not kept.
    refreshed: RefCell<BTreeMap<String, (usize, Option<Object>)>>,
    /// Import answers since the last write, with the link that answered.
    imported: RefCell<BTreeMap<String, (usize, Option<Object>)>>,
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
    settings: RefCell<BTreeMap<usize, settings::Given>>,
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
    /// The deployment's run's secrets ([`Providers::secrets`]).
    secrets: Rc<crate::secrets::Run>,
    /// The run's Plan calls' answers, by what each asked: what a plan made
    /// again takes for a row asked as it was ([`Providers::planning_again`]).
    answered: RefCell<BTreeMap<String, provider::Planned>>,
    /// Whether a plan is being made again ([`PlanningAgain`]).
    again: std::cell::Cell<bool>,
}

/// While it lives, the providers' plan is made again over what changed
/// ([`Providers::planning_again`]).
pub struct PlanningAgain<'p>(&'p Providers);

impl Drop for PlanningAgain<'_> {
    fn drop(&mut self) {
        self.0.again.set(false);
    }
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

/// How a run reaches its providers: a backend (`plugin::backend`). The
/// CLI's starts each as a process and speaks gRPC to it (`dform-grpc`);
/// tests and benches link the mock in (`dform-mock`).
pub trait Launch {
    /// The mock provider, which plays every mock schema.
    fn mock(&self) -> Result<Link>;
    /// The provider executable at `exe`, with what dform.toml grants it.
    fn plugin(&self, exe: &Path, grants: &super::host::Grants) -> Result<Link>;
}

/// A world's key of the object `remote` of type `typ`.
fn key(typ: &str, remote: &str) -> String {
    format!("{typ}::{remote}")
}

impl Providers {
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

    /// The secrets of the deployment's run these providers serve: what
    /// its evaluation derives and registers, and what its calls must
    /// never send ([`crate::secrets::Run`]).
    pub fn secrets(&self) -> &Rc<crate::secrets::Run> {
        &self.secrets
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

    /// What the providers said during the last apply.
    pub fn take_notes(&self) -> Vec<String> {
        self.notes.take()
    }

    /// Where more secrets the run reads are held, by label (another
    /// deployment's outputs a tree's boundary made known).
    pub fn hold(&self, more: BTreeMap<String, provider::Held>) {
        self.held.borrow_mut().extend(more);
    }

    /// While the guard lives, a plan is made again over what changed of
    /// the program's (another deployment's output its plan made other):
    /// a row whose Plan call asks what one of the run asked already is
    /// answered as it was, not sent again.
    pub fn planning_again(&self) -> PlanningAgain<'_> {
        self.again.set(true);
        PlanningAgain(self)
    }

    /// The calls sent again since the last time they were taken (R-81),
    /// for the audit log.
    pub fn take_retries(&self) -> Vec<Retry> {
        self.links
            .iter()
            .zip(&self.sessions)
            .flat_map(|(l, s)| l.borrow_mut().take_retries(*s))
            .collect()
    }

    /// The link `i`, to call; none when the run started no provider (a
    /// program naming only built-in providers, R-26): nothing to ask.
    fn link(&self, i: usize) -> Result<&RefCell<Link>> {
        self.links
            .get(i)
            .map(|l| &**l)
            .ok_or_else(|| anyhow!(crate::deployment::NO_PROVIDER))
    }

    /// One call to link `i`, in these providers' session of it.
    fn call<R>(&self, i: usize, call: impl Into<Call>) -> Result<R>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        self.link(i)?.borrow_mut().call(self.sessions[i], call)
    }

    /// Start `call` on link `i`, in these providers' session of it, once
    /// a provisional link may send it ([`Link::settled`]).
    fn submit(&self, i: usize, call: impl Into<Call>) -> Result<Ticket> {
        let mut link = self.link(i)?.borrow_mut();
        link.settled(self.sessions[i])?;
        Ok(link.submit(self.sessions[i], call))
    }

    /// Configure these providers' session of link `i` again, with
    /// `config`: the same session, what its Schema is asked under.
    fn configure_link(&self, i: usize, config: Json) -> Result<Option<String>> {
        let (_, account) = reconfigure(
            &mut self.link(i)?.borrow_mut(),
            self.sessions[i],
            config.clone(),
        )?;
        self.sent.borrow_mut().insert(i, config);
        Ok(account)
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

    /// Send every call submitted so far, to every provider.
    pub fn flush(&self) {
        for l in &self.links {
            l.borrow_mut().flush();
        }
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
}

/// An Apply call dform does not send, and why (R-109): `apply T A: not
/// sent`, the reason on its own line.
fn not_sent(addr: &Address, why: &str) -> anyhow::Error {
    crate::report::Failure::of("apply", addr, "not sent", why).into()
}
