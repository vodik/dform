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
//! mock (which plays unknown types), else the first.

use super::backend::{CallError, Ticket};
use super::link::Link;
use super::pb;
use super::source::{self, Source};
use super::wire;
use crate::ast::{Atom, Term};
use crate::ir::{Address, Adopt, Resource};
use crate::provider::{self, Action, ActionKind, Change, Plan, get_path, remove_path, set_path};
use crate::schema::Schema;
use crate::state::{self, State, StateEntry};
use crate::value::{NullClass, Value};
use crate::zset::{self, Lifecycle};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as Json, json};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `provider_expect_account(Name, Account)`: a `provider` block's
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
    /// The program's `provider NAME { .. }` blocks: each one's spec (as
    /// `specs` names it) -> NAME, so `provider_config(NAME, ..)` (what a
    /// block's settings lower to) finds its link by the block's name as
    /// well as by the provider's own.
    pub blocks: BTreeMap<String, String>,
}

/// How a run reaches its providers: a backend (`plugin::backend`). The
/// CLI's starts each as a process and speaks gRPC to it (`dform-grpc`);
/// tests and benches link the mock in (`dform-mock`).
pub trait Launch {
    /// The mock provider, which plays every mock schema.
    fn mock(&self) -> Result<Link>;
    /// The provider executable at `exe`.
    fn plugin(&self, exe: &Path) -> Result<Link>;
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
    /// The links whose settings the program gives and has not yet (a
    /// null, or not evaluated): they serve no state entry.
    awaiting: RefCell<BTreeSet<usize>>,
    /// The settings each link was last configured with from the program.
    settings: RefCell<BTreeMap<usize, Json>>,
    /// A program's provider block name -> its link ([`Config::blocks`]).
    blocks: BTreeMap<String, usize>,
    /// The mock schemas' blocks: name -> the schema (the mock's one link
    /// plays every one).
    mock_blocks: BTreeMap<String, String>,
    /// The account each link's last Configure reported, if it tells.
    accounts: RefCell<BTreeMap<usize, String>>,
}

/// What the providers' Schema calls answered.
struct Loaded {
    schema: Schema,
    /// Type -> the provider serving it.
    owner: BTreeMap<String, usize>,
    /// Extern predicate -> the provider answering it.
    externs: BTreeMap<String, usize>,
    /// The types asked for; `None`: every one.
    scope: Option<BTreeSet<String>>,
}

/// Configure a link: the account its credentials reach, if it tells.
fn configure(link: &mut Link, config: Json) -> Result<Option<String>> {
    let config = Some(wire::doc(&config));
    let r: pb::ConfigureResponse = link.call(pb::ConfigureRequest { config })?;
    Ok(r.account)
}

impl Providers {
    /// Start the providers `specs` name (`--provider` or the `provider`
    /// statements; none is the mock's `fake` schema): every mock schema in
    /// one mock provider, every plugin in its own process.
    pub fn start(launch: &dyn Launch, specs: &[String], cfg: &Config) -> Result<Providers> {
        let p = Self::start_deferred(launch, specs, cfg)?;
        p.load_schema(None)?;
        Ok(p)
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
        let (mut mocks, mut plugins) = (Vec::new(), Vec::new());
        for s in &specs {
            match source::resolve(s) {
                Source::Mock(m) => mocks.push(m),
                Source::Plugin(p) => plugins.push(p),
            }
        }
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
        let (mut links, mut bases, mut awaiting) = (Vec::new(), Vec::new(), BTreeSet::new());
        let mut accounts = BTreeMap::new();
        // Each block's link: the mock is the first when there is one, the
        // plugins follow in order.
        let (mut blocks, mut mock_blocks) = (BTreeMap::new(), BTreeMap::new());
        let block = |s: &String| cfg.blocks.get(s).cloned();
        let mut next = usize::from(!mocks.is_empty());
        for s in &specs {
            match source::resolve(s) {
                Source::Mock(m) => {
                    blocks.extend(block(s).map(|b| (b, 0)));
                    mock_blocks.extend(block(s).map(|b| (b, m)));
                }
                Source::Plugin(_) => {
                    blocks.extend(block(s).map(|b| (b, next)));
                    next += 1;
                }
            }
        }
        let by_block = |i: usize, name: &str| {
            cfg.configured.contains(name)
                || blocks
                    .iter()
                    .any(|(b, &j)| j == i && cfg.configured.contains(b))
        };
        if !mocks.is_empty() {
            let mut link = launch.mock()?;
            let mut config = base.clone();
            config["schemas"] = json!(mocks);
            // A mock schema the program configures by its block serves
            // nothing until the settings arrive, as a plugin would.
            if blocks
                .iter()
                .any(|(b, &j)| j == 0 && cfg.configured.contains(b))
            {
                awaiting.insert(0);
            }
            accounts.extend(configure(&mut link, config.clone())?.map(|a| (0, a)));
            links.push(link);
            bases.push(config);
        }
        for p in plugins {
            let mut link = launch.plugin(&p)?;
            let mut config = base.clone();
            let i = links.len();
            if by_block(i, &link.name) {
                config["deferred"] = json!(true);
                awaiting.insert(i);
            }
            let account = configure(&mut link, config)
                .with_context(|| format!("configure provider {}", p.display()))?;
            accounts.extend(account.map(|a| (i, a)));
            links.push(link);
            bases.push(base.clone());
        }
        if links.is_empty() {
            bail!("no providers");
        }
        let p = Self::deferred(links);
        Ok(Providers {
            bases,
            awaiting: RefCell::new(awaiting),
            blocks,
            mock_blocks,
            accounts: RefCell::new(accounts),
            ..p
        })
    }

    /// Configure each provider the program configures
    /// (`provider_config(Name, Settings)` in `facts`) whose settings are
    /// known now and changed: Configure again with them as `settings`.
    /// Settings holding a null (a cluster not created yet) wait. Whether
    /// any provider was configured: then what was read is read again.
    pub fn configure_from<'a>(&self, facts: impl IntoIterator<Item = &'a Atom>) -> Result<bool> {
        let mut changed = false;
        for a in facts.into_iter().filter(|a| a.pred == "provider_config") {
            let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() else {
                continue;
            };
            let Some(i) = self.link_for(name) else {
                continue;
            };
            let Some(settings) = known_json(v) else {
                continue;
            };
            if self.settings.borrow().get(&i) == Some(&settings) {
                continue;
            }
            let mut config = self.bases.get(i).cloned().unwrap_or_else(|| json!({}));
            config["settings"] = settings.clone();
            let account = configure(&mut self.links[i].borrow_mut(), config)
                .with_context(|| format!("configure provider {name} from provider_config"))?;
            let mut accounts = self.accounts.borrow_mut();
            match account {
                Some(a) => accounts.insert(i, a),
                None => accounts.remove(&i),
            };
            drop(accounts);
            self.settings.borrow_mut().insert(i, settings);
            self.awaiting.borrow_mut().remove(&i);
            changed = true;
        }
        if changed {
            self.invalidate();
        }
        Ok(changed)
    }

    /// The providers of started, configured links, their schema loaded.
    /// The first serves the types no schema declares.
    pub fn from_links(links: Vec<Link>) -> Result<Providers> {
        if links.is_empty() {
            bail!("no providers");
        }
        let p = Self::deferred(links);
        p.load_schema(None)?;
        Ok(p)
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
            awaiting: RefCell::new(BTreeSet::new()),
            settings: RefCell::new(BTreeMap::new()),
            blocks: BTreeMap::new(),
            mock_blocks: BTreeMap::new(),
            accounts: RefCell::new(BTreeMap::new()),
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

    /// The link a program names: by the provider's own name, else by the
    /// name of the `provider` block that selects it.
    fn link_for(&self, name: &str) -> Option<usize> {
        self.link_named(name)
            .or_else(|| self.blocks.get(name).copied())
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
                    (Some(n), Some(p)) if a.pred != "cloud_exists" => {
                        format!("{t}.{n}.{}", p.trim_start_matches('.'))
                    }
                    (Some(n), _) => format!("{t}.{n}"),
                    _ => t.clone(),
                };
                types.contains(&t).then_some(what)
            };
            // A reference in a head: `ref(T, N, P)` of a type it serves.
            fn reference(t: &Term, types: &BTreeSet<String>) -> Option<String> {
                match t {
                    Term::Val(Value::Ref { typ, name, attr }) if types.contains(typ) => {
                        Some(format!("{typ}.{name}.{attr}"))
                    }
                    Term::Func { name, args } if name == "ref" => match args.as_slice() {
                        [Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), p]
                            if types.contains(t) =>
                        {
                            let p = match p {
                                Term::Val(Value::Str(p)) => p.trim_start_matches('.').to_string(),
                                _ => "..".to_string(),
                            };
                            Some(format!("{t}.{n}.{p}"))
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

    /// `expect_account` (`provider_expect_account(Name, Account)` in
    /// `facts`): each configured provider it names reports that account at
    /// Configure, or the run is refused before anything is planned. A
    /// provider still waiting on the program's settings is checked once
    /// they arrive.
    pub fn check_accounts<'a>(&self, facts: impl IntoIterator<Item = &'a Atom>) -> Result<()> {
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
            let want = match want {
                Value::Str(s) => s.clone(),
                v => crate::partition::fmt_value(v),
            };
            match self.accounts.borrow().get(&i) {
                Some(got) if *got == want => {}
                Some(got) => wrong.push(format!(
                    "provider {name} reports account {got}, but the program expects {want} \
                     (expect_account)"
                )),
                None => wrong.push(format!(
                    "provider {name} reports no account, but the program expects {want} \
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
        for (i, link) in self.links.iter().enumerate() {
            let req = pb::SchemaRequest {
                types: types.clone(),
            };
            let resp: pb::SchemaResponse = link.borrow_mut().call(req)?;
            let facts = resp
                .facts
                .iter()
                .map(wire::from_fact)
                .collect::<Result<Vec<Atom>>>()?;
            let mut s = Schema::from_facts(&facts)
                .with_context(|| format!("the schema of provider {}", self.names[i]))?;
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
            for e in &resp.externs {
                externs.entry(e.pred.clone()).or_insert(i);
            }
            schema = schema.merge(s)?;
        }
        let loaded = Loaded {
            schema,
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

    pub fn schema(&self) -> &Schema {
        &self.loaded().schema
    }

    /// The providers' schema facts (`type_attr`, `type_list_key`,
    /// `type_provider`, `type_mint`, ...), injected into the program as EDB:
    /// those of the `named` types ([`Schema::facts_for`]), or all of them.
    /// A schema loaded for fewer types than asked for is an error.
    pub fn catalog(&self, named: Option<&BTreeSet<String>>) -> Result<Vec<Atom>> {
        let l = self.loaded();
        if let Some(scope) = &l.scope
            && named.is_none_or(|n| !n.is_subset(scope))
        {
            bail!("internal: the schema was loaded for fewer types than the run names");
        }
        Ok(match named {
            Some(named) => l.schema.facts_for(named),
            None => l.schema.facts.clone(),
        })
    }

    /// What the providers said during the last apply.
    pub fn take_notes(&self) -> Vec<String> {
        self.notes.take()
    }

    fn route(&self, typ: &str) -> usize {
        self.loaded()
            .owner
            .get(typ)
            .copied()
            .unwrap_or(self.fallback)
    }

    fn link_named(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
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
            .filter(|(_, e)| {
                self.link_named(&e.provider)
                    .is_some_and(|i| !self.awaiting.borrow().contains(&i))
            })
            .filter_map(|(k, e)| state::parse_key(k).map(|a| (a, e)))
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

    fn query_at(
        &self,
        i: usize,
        pred: &str,
        plus: &[bool],
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let rows: Vec<pb::Row> = self.links[i].borrow_mut().call(pb::QueryRequest {
            pred: pred.to_string(),
            input: plus.to_vec(),
            inputs: inputs.iter().map(wire::value).collect(),
        })?;
        rows.iter()
            .map(|r| r.values.iter().map(wire::from_value).collect())
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
        &self.names[self.route(typ)]
    }

    /// The object an Apply Create or Replace of `addr` with the
    /// idempotency key `key` made (`CREATED`): its remote id. `None` when
    /// it made none, or when its provider cannot say (no `managed`
    /// capability): then only a retry with the same key finds out.
    pub fn created(&self, addr: &Address, key: &str) -> Result<Option<String>> {
        let i = self.route(&addr.typ);
        if !self.links[i].borrow().has("managed") {
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
        let r: pb::ReadResponse = self.links[i].borrow_mut().call(req)?;
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
        let r: pb::ImportResponse = self.links[self.route(typ)].borrow_mut().call(req)?;
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
            let i = self.link_named(&e.provider).expect("entries are served");
            mapped.insert(key(&a.typ, &e.remote), (a, e.remote.clone(), i));
        }
        let keys: BTreeSet<String> = mapped.keys().cloned().collect();
        if let Some((m, w)) = &*self.refreshed.borrow()
            && *m == keys
        {
            return Ok(w.clone());
        }
        let mut world = World::new();
        for (k, (addr, remote, i)) in mapped {
            if let Some(o) = self.read(i, &addr, &remote)? {
                world.insert(k, o);
            }
        }
        self.refreshed.replace(Some((keys, world.clone())));
        Ok(world)
    }

    /// Refresh as facts, for round-0 resolution (E Rule 4, F DR-11 revised):
    /// `identity(T, A, Rid)` for every address state maps to an object Read
    /// returns, and `world_attr(T, Rid, P, V)` for every schema-computed or
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
                    out.push(Atom {
                        pred: "world_attr".into(),
                        args: vec![
                            s(&addr.typ),
                            s(&e.remote),
                            s(&path),
                            Term::Val(provider::json_to_value(v)),
                        ],
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
        let mut submitted: Vec<(usize, usize, Ticket)> = Vec::new();
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
            submitted.push((i, l, self.links[l].borrow_mut().submit(req)));
        }
        for (i, l, t) in submitted {
            let mut link = self.links[l].borrow_mut();
            let r = link
                .wait(t)
                .and_then(|r| link.expect::<pb::PlanResponse>("Plan", r));
            out[i] = Some(r.map_err(anyhow::Error::new).and_then(|r| {
                let side = |v: &Option<pb::Value>| v.as_ref().map(wire::from_doc).transpose();
                let changes = r
                    .changes
                    .iter()
                    .map(|c| {
                        Ok(Change {
                            path: c.path.clone(),
                            before: side(&c.before)?,
                            after: side(&c.after)?,
                            sensitive: c.sensitive,
                        })
                    })
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
        if let Some(v) = ctx.resolved.get(&addr).and_then(|d| get_path(d, attr)) {
            return Ok(v.clone());
        }
        let existing = ctx.existing(&addr)?;
        match existing.as_ref().and_then(|o| get_path(&o.attrs, attr)) {
            Some(v) => Ok(v.clone()),
            None => match ctx.strict {
                Some(at) => bail!(
                    "apply {}/{}: ?{label} is still unknown ({typ}/{name} does not set {attr})",
                    at.typ,
                    at.name
                ),
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
            return Ok(provider::secret_json(label));
        }
        let mut found = None;
        if let (Some((_, path)), Some((typ, name))) =
            (label.split_once('#'), crate::value::null_owner(label))
        {
            let owner = Address { typ, name };
            if !ctx.retracted.contains(&owner)
                && let Some(o) = ctx.existing(&owner)?
            {
                found = get_path(&o.computed, path).cloned();
            }
        }
        match (found, ctx.strict) {
            (Some(v), _) => Ok(v),
            (None, Some(at)) => bail!(
                "apply {}/{}: ?{label} is still unknown (its resource has not been created)",
                at.typ,
                at.name
            ),
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
        bail!("cloud_ref missing attribute {typ}.{name}.{attr}");
    }

    fn resolve_value(&self, ctx: &Ctx, v: &Value) -> Result<Json> {
        Ok(match v {
            Value::Str(s) => json!(s),
            Value::Int(i) => json!(i),
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
            Value::IpRange { start, end } => json!(format!(
                "{}-{}",
                crate::value::u32_to_ipv4(*start),
                crate::value::u32_to_ipv4(*end)
            )),
            Value::Ref { typ, name, attr } => self.resolve_ref(ctx, typ, name, attr)?,
            Value::CloudRef { typ, name, attr } => self.resolve_cloud_ref(typ, name, attr)?,
            Value::Null { label, class, .. } => self.resolve_null(ctx, label, *class)?,
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

        // desired: the assembled documents, refs and nulls resolved as far
        // as the world allows.
        let mut resolved: BTreeMap<Address, Json> = BTreeMap::new();
        let mut order = Vec::new();
        for r in topo_sort(desired)? {
            let ctx = Ctx {
                cloud: self,
                world: &world,
                state,
                adopts: &adopt_map,
                resolved: &resolved,
                strict: None,
                retracted,
            };
            let doc = self.resolve_value(&ctx, &r.attrs)?;
            order.push(r.addr.clone());
            resolved.insert(r.addr.clone(), doc);
        }

        // world: through the identity mapping. A state entry whose object
        // is gone is no row (a desired one is created again); one no longer
        // desired is deleted from state with nothing to delete.
        let mut before: BTreeMap<Address, Json> = BTreeMap::new();
        for (addr, entry) in self.entries(&state.resources) {
            if let Some(cur) = world.get(&key(&addr.typ, &entry.remote)) {
                let mut doc = match resolved.get(&addr) {
                    Some(d) => self.world_doc(&addr.typ, cur, d),
                    None => cur.attrs.clone(),
                };
                // ignore_changes: an object that exists ignores changes at
                // the path, so it is dropped from both sides. A create (no
                // world side) sets it.
                if let Some(want) = resolved.get_mut(&addr) {
                    for p in lifecycle.ignore_changes.get(&addr).into_iter().flatten() {
                        remove_path(&mut doc, p);
                        remove_path(want, p);
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
                zset::Kind::Update => ActionKind::Update,
                zset::Kind::Pending => ActionKind::Pending,
                zset::Kind::Undeformed => ActionKind::Noop,
            };
            let (changes, on) = match d.kind {
                zset::Kind::Pending => (changes, d.unresolved.clone()),
                zset::Kind::Undeformed => (Vec::new(), BTreeSet::new()),
                _ => (changes, BTreeSet::new()),
            };
            actions.push(Action {
                kind,
                addr,
                changes,
                on,
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
    /// leaf value, keyed lists by key, sets sorted (`provider::flatten`),
    /// null and secret markers as labeled nulls of the schema's class.
    fn flat_value(&self, typ: &str, doc: &Json) -> Value {
        let mut leaves = BTreeMap::new();
        provider::flatten(self.schema(), typ, doc, "", "", false, &mut leaves);
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
                        Some((_, label)) => Value::Null {
                            label: label.to_string(),
                            class: NullClass::Secret,
                            ty: String::new(),
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
    fn null_class(&self, label: &str) -> NullClass {
        let Some((ta, path)) = label.split_once('#') else {
            return NullClass::Open;
        };
        let typ = ta.split_once('/').map(|x| x.0).unwrap_or(ta);
        self.schema()
            .class_of(typ, path)
            .or_else(|| self.schema().optional_computed_class(typ, path))
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
}

impl Tick<'_> {
    /// Submit the Apply call for `a`, as the executor's call `id`. Returns
    /// whether a call is in flight; an action that needs none (the delete
    /// of an object state no longer maps) is answered at once.
    pub fn submit(&mut self, id: usize, a: &Action, state: &mut State) -> Result<bool> {
        let cloud = self.cloud;
        let addr = &a.addr;
        let at = format!("{}/{}", addr.typ, addr.name);
        if matches!(a.kind, ActionKind::Noop | ActionKind::Pending) {
            return Ok(false);
        }
        if self.world.is_none() {
            self.world = Some(cloud.refresh(state)?);
        }
        let doc = match a.kind {
            ActionKind::Delete | ActionKind::DeleteDeposed => Json::Null,
            _ => {
                let r = self
                    .desired
                    .get(addr)
                    .ok_or_else(|| anyhow!("apply {at}: no desired resource"))?;
                let ctx = Ctx {
                    cloud,
                    world: self.world.as_ref().expect("read above"),
                    state,
                    adopts: &self.adopt_map,
                    resolved: &self.resolved,
                    strict: Some(addr),
                    retracted: &BTreeSet::new(),
                };
                let doc = cloud.resolve_value(&ctx, &r.attrs)?;
                self.resolved.insert(addr.clone(), doc.clone());
                doc
            }
        };
        let world = self.world.as_ref().expect("read above");
        let (op, remote, config, create_first) = match a.kind {
            ActionKind::Noop | ActionKind::Pending => unreachable!("returned above"),
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
                    bail!("apply {at}: replace without a state entry");
                };
                (pb::Op::Replace, old, Some(doc), create_first)
            }
            ActionKind::Adopt => {
                let Some(remote_name) = self.adopt_map.get(addr).cloned() else {
                    bail!("apply {at}: adopt action missing adopt mapping");
                };
                (pb::Op::Adopt, remote_name, Some(doc), false)
            }
            ActionKind::Update | ActionKind::Drift => {
                let Some(remote) = state.get(addr).map(|e| e.remote.clone()) else {
                    bail!("apply {at}: update without a state entry");
                };
                let mut doc = doc;
                // ignore_changes: the world keeps its value, or its absence.
                if let Some(cur) = world.get(&key(&addr.typ, &remote)) {
                    for p in self
                        .lifecycle
                        .ignore_changes
                        .get(addr)
                        .into_iter()
                        .flatten()
                    {
                        match get_path(&cur.attrs, p) {
                            Some(v) => set_path(&mut doc, p, v.clone()),
                            None => remove_path(&mut doc, p),
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
                        message: format!("{at} .{path} fails its refinement {c}"),
                    }
                })
                .collect(),
        };
        let idempotency_key = uncertain_from_here(a, addr, &remote, state);
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
        };
        let link = cloud.route(&addr.typ);
        let ticket = cloud.links[link].borrow_mut().submit(req);
        cloud.invalidate();
        self.in_flight.push(InFlight {
            id,
            link,
            ticket,
            kind: a.kind.clone(),
            addr: addr.clone(),
            remote,
        });
        Ok(true)
    }

    /// Whether an Apply call is in flight.
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Take the next answered Apply call and record it: identity in `state`
    /// when the call answered; a call that may have taken effect without
    /// answering (a timeout) records none. Returns the call's id and its
    /// outcome. An answer a link already holds comes first; else the link
    /// of the oldest call in flight is waited on.
    pub fn next_completed(&mut self, state: &mut State) -> (usize, Result<()>) {
        let cloud = self.cloud;
        assert!(self.busy(), "internal: no Apply call in flight");
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
                let (t, r) = cloud.links[l].borrow_mut().next_completed();
                let k = self
                    .in_flight
                    .iter()
                    .position(|f| f.link == l && f.ticket == t)
                    .expect("internal: an answer to a call nobody made");
                (k, r)
            }
        };
        let f = self.in_flight.remove(k);
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
                state.set(addr.clone(), provider.clone(), resp.remote.clone());
            }
            ActionKind::Update | ActionKind::Drift => {
                world.insert(
                    key(&addr.typ, &f.remote),
                    Providers::object(resp.attrs.as_ref(), resp.computed.as_ref())?,
                );
            }
            ActionKind::Delete => {
                world.remove(&key(&addr.typ, &f.remote));
                state.remove(addr);
            }
            ActionKind::DeleteDeposed => {
                world.remove(&key(&addr.typ, &f.remote));
                state.deposed.remove(&state::key(addr));
            }
            ActionKind::Noop | ActionKind::Pending => {}
        }
        Ok(resp)
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
        let at = format!("{}/{}", addr.typ, addr.name);
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
        match result {
            Ok(_) => Ok(()),
            Err(CallError::Crashed(m)) => {
                bail!("apply {at}: {m}; the change may have taken effect")
            }
            Err(e) => Err(anyhow!(e.clone())),
        }
    }

    /// How long the Apply call for `addr` took on its provider's clock
    /// (the mock's chaos `latency`, else no time).
    pub fn latency(&self, addr: &Address) -> u64 {
        self.elapsed.get(addr).copied().unwrap_or(0)
    }

    /// Record an Apply call's span on the executor's clock.
    pub fn record(&mut self, addr: &Address, start_ms: u64, end_ms: u64) {
        self.timeline.push(pb::Span {
            addr: format!("{}/{}", addr.typ, addr.name),
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
fn uncertain_from_here(a: &Action, addr: &Address, remote: &str, state: &mut State) -> String {
    use state::UncertainOp as Op;
    let op = match a.kind {
        ActionKind::Create => Op::Create,
        ActionKind::Replace { create_first } => Op::Replace { create_first },
        ActionKind::Update | ActionKind::Drift => Op::Update,
        ActionKind::Delete => Op::Delete,
        ActionKind::DeleteDeposed => Op::DeleteDeposed,
        ActionKind::Adopt | ActionKind::Noop | ActionKind::Pending => return String::new(),
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
        Value::Bool(b) => json!(b),
        Value::List(xs) => Json::Array(xs.iter().map(known_json).collect::<Option<_>>()?),
        Value::Obj(m) => Json::Object(
            m.iter()
                .map(|(k, x)| Some((k.clone(), known_json(x)?)))
                .collect::<Option<_>>()?,
        ),
        Value::Ip(n) => json!(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => json!(crate::value::ipnet_to_string(*addr, *prefix)),
        Value::IpRange { start, end } => json!(format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        )),
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
