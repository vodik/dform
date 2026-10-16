//! Starting the providers: each link launched (or the tree's pool's),
//! its session opened by Configure, and the names the program gives it.

use super::schema::Share;
use super::{Config, Launch, Providers};
use crate::plugin::backend::Session;
use crate::plugin::link::Link;
use crate::plugin::source::{self, Source};
use crate::plugin::wire;
use anyhow::{Context, Result, bail};
use serde_json::{Value as Json, json};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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

impl dyn Launch + '_ {
    /// The mock provider, started: a phase of the run.
    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "plan.phase",
        skip_all,
        fields(otel.name = "provider mock started")
    )]
    fn start_mock(&self) -> Result<Link> {
        self.mock()
    }

    /// The provider executable at `exe` started, spawned and its
    /// handshake answered: a phase of the run.
    #[tracing::instrument(
        target = "dform",
        level = "debug",
        name = "plan.phase",
        skip_all,
        fields(otel.name = %format_args!("provider {} started (spawn and handshake)", exe.display()))
    )]
    fn start_plugin(&self, exe: &Path, grants: &crate::plugin::host::Grants) -> Result<Link> {
        self.plugin(exe, grants)
    }
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
    links: Vec<Rc<RefCell<Link>>>,
    /// Each link's session, as its Configure opened it.
    sessions: Vec<Session>,
    pooled: Vec<Option<Rc<crate::plugin::pool::Entry>>>,
    pool: Option<Rc<crate::plugin::pool::Pool>>,
    /// Each link's Configure.
    sent: BTreeMap<usize, Json>,
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
            sessions: Vec::new(),
            pooled: Vec::new(),
            pool: crate::plugin::pool::current(),
            sent: BTreeMap::new(),
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

    fn policy_of(&self, s: &String) -> Option<crate::plugin::policy::Policy> {
        self.cfg.policies.get(s).copied()
    }

    /// The mock link, playing every shared mock schema. A mock schema the
    /// program configures by its block serves nothing until the settings
    /// arrive, as a plugin would; a mock playing several providers is
    /// configured by none of them ([`Providers::link_for`]).
    fn start_mock(&mut self) -> Result<()> {
        let launch = self.launch;
        let link = self.started(crate::plugin::pool::Key::Mock, || launch.start_mock())?;
        let mut link = link.borrow_mut();
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
        let (session, account) = configure(&mut link, config.clone())?;
        self.sessions.push(session);
        self.accounts.extend(account.map(|a| (0, a)));
        drop(link);
        self.sent.insert(self.links.len() - 1, config.clone());
        self.bases.push(config);
        self.shares.push(None);
        Ok(())
    }

    /// The link `key` names, kept: the pool's when the run is a tree's
    /// (started now if it is not there yet), else started now.
    fn started(
        &mut self,
        key: crate::plugin::pool::Key,
        start: impl FnOnce() -> Result<Link>,
    ) -> Result<Rc<RefCell<Link>>> {
        let (link, entry) = match &self.pool {
            Some(pool) => {
                let e = pool.link(key, start)?;
                (e.link.clone(), Some(e))
            }
            None => (Rc::new(RefCell::new(start()?)), None),
        };
        self.links.push(link.clone());
        self.pooled.push(entry);
        Ok(link)
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
        let i = self.links.len();
        let key = crate::plugin::pool::Key::Spec {
            spec: st.spec.clone(),
            rename: st.rename.as_ref().map(|b| b.name.clone()),
        };
        let link = self.started(key, || match (&mock, source::resolve(&st.spec)) {
            (Some(_), _) => launch.start_mock(),
            (None, Source::Plugin(p)) => {
                let mut none = crate::plugin::host::Grants::none_for(&p);
                none.files = crate::files::Shared(Some(cfg.files.clone()));
                launch.start_plugin(&p, cfg.grants.get(&st.spec).unwrap_or(&none))
            }
            (None, Source::Mock(_)) => unreachable!("a plugin"),
        })?;
        let mut link = link.borrow_mut();
        if let Some(policy) = self.policy_of(&st.spec) {
            link.set_policy(policy);
        }
        // The provider's types under the `use`'s name, renamed at the link
        // only: the provider never learns of it.
        if let Some(b) = &st.rename {
            link.rename(wire::Rename::new(b.name.clone(), b.provider.clone()));
        }
        let mut base = self.base.clone();
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
        let (session, account) = configure(&mut link, config.clone())
            .with_context(|| format!("configure provider {shown}"))?;
        self.sessions.push(session);
        drop(link);
        self.sent.insert(i, config.clone());
        self.accounts.extend(account.map(|a| (i, a)));
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
            let link = link.borrow();
            if let Some(r) = &link.reader {
                for s in &link.schemes {
                    cfg.files.declare(s, &link.name, r.clone());
                }
            }
        }
        let p = Providers::deferred(self.links, self.sessions);
        Ok(Providers {
            pooled: self.pooled,
            pool: self.pool,
            sent: RefCell::new(self.sent),
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
            secrets: cfg.secrets.clone(),
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

/// Open a session of a link, configured with `config`: the session, and
/// the account its credentials reach, if it tells.
pub(super) fn configure(link: &mut Link, config: Json) -> Result<(Session, Option<String>)> {
    reconfigure(link, Session::NONE, config)
}

/// Configure `session` of a link (none: open one) with `config`: the
/// session, and the account its credentials reach, if it tells; what it
/// notes of its configuration (a credential about to expire) on stderr.
pub(super) fn reconfigure(
    link: &mut Link,
    session: Session,
    config: Json,
) -> Result<(Session, Option<String>)> {
    let r = link.configure(session, wire::doc(&config))?;
    for n in &r.notes {
        eprintln!("{n}");
    }
    Ok((Session(r.session), r.account))
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
    /// empty), of the run whose secrets are `secrets`. Asking it anything
    /// else is the no-provider error.
    pub fn none(secrets: Rc<crate::secrets::Run>) -> Providers {
        Providers {
            secrets,
            ..Self::deferred(Vec::new(), Vec::new())
        }
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

    fn deferred(links: Vec<Rc<RefCell<Link>>>, sessions: Vec<Session>) -> Providers {
        Providers {
            names: links.iter().map(|l| l.borrow().name.clone()).collect(),
            pooled: links.iter().map(|_| None).collect(),
            pool: None,
            sent: RefCell::new(BTreeMap::new()),
            links,
            sessions,
            loaded: OnceCell::new(),
            fallback: 0,
            refreshed: RefCell::new(BTreeMap::new()),
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
            secrets: Rc::default(),
        }
    }
}
