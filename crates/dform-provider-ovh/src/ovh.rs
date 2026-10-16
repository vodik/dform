//! The provider: the plugin protocol (`Handler`) over the OVH API. Each
//! session (a deployment's Configure) is an [`Ovh`] of its own: its
//! credentials, its project, what it made, and its client, which asks
//! each list the provider reads once per run (`api::Client::list`).
//!
//! Configure finds the credentials (`config`), and with the program's
//! settings (`use ovh { endpoint, project }`, a second Configure) the
//! project: its id, or the description or name it was given in the
//! console (`vodik`). It reports the project's id as the account
//! (`expect_account`). Without settings it is configured from the
//! environment alone (`OVH_CLOUD_PROJECT_SERVICE`), as `provider check`
//! runs it; without credentials it serves its schema and Plan, and every
//! call that needs the API fails naming why.
//!
//! Remote ids: an instance's, an SSH key's, a volume's, a private
//! network's and a user's are the API's ids; a DNS record's is `ZONE/ID`,
//! a subnet's `NETWORK/ID`, an S3 container's `REGION/NAME`, an S3
//! credential's `USER/ACCESS`. Each type is its module below (`instance`,
//! `ssh_key`, `record`, `storage`, `user`, `s3_credential`, `volume`,
//! `network`), what a region offers `catalog`, the protocol
//! `handler`, an object's health `health`; this file dispatches to them.
//! Plan diffs locally (`provider::diff`), and an
//! instance's flavor and image are checked against what its region offers.
//! An instance is resized in place to a larger flavor and replaced for a
//! smaller one; its networks are interfaces attached and detached.
//! A Create looks for an object of the same lookup first (the content
//! its schema's `type_lookup` names, R-195): one this process
//! made under the same idempotency key is the answer (a Create sent again
//! after a timeout, R-81), another is refused, to be adopted or renamed.
//! `provider.created` answers by the same key, so a Create whose answer
//! was lost is adopted. An instance's Create waits for it to be ACTIVE; a
//! Delete waits for it to go.

use crate::api::{self, Client, escape};
use crate::config;
use crate::map;
use anyhow::{Result, anyhow, bail};
use dform_core::address::Address;
use dform_core::plugin::backend::{self, CallError, Handler, Reply, Session, VERSION};
use dform_core::plugin::pb;
use dform_core::plugin::providers::CREATED;
use dform_core::plugin::wire;
use dform_core::provider::{self, diff, marker, norm_path};
use dform_core::schema::Schema;
use dform_core::value::Value;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

mod catalog;
mod handler;
pub use handler::Provider;
mod health;
mod instance;
mod network;
mod record;
mod s3_credential;
mod ssh_key;
mod storage;
mod user;
mod volume;

/// The provider's name, its types' namespace (R-36).
pub const PROVIDER: &str = "ovh";
pub const INSTANCE: &str = "ovh.instance";
pub const SSH_KEY: &str = "ovh.ssh_key";
pub const RECORD: &str = "ovh.domain_record";
pub const CONTAINER: &str = "ovh.storage_container";
pub const USER: &str = "ovh.cloud_project_user";
pub const S3_CREDENTIAL: &str = "ovh.s3_credential";
pub const VOLUME: &str = "ovh.volume";
pub const NETWORK: &str = "ovh.network";
pub const SUBNET: &str = "ovh.subnet";
pub const REGION: &str = "ovh.region";
pub const FLAVOR: &str = "ovh.flavor";
pub const IMAGE: &str = "ovh.image";
pub const ZONE: &str = "ovh.zone";

/// The data sources, each with its columns' binding (`+` an input).
pub const EXTERNS: [(&str, &[bool]); 4] = [
    (REGION, &[true, false, false]),
    (FLAVOR, &[true, false, false, false, false]),
    (IMAGE, &[true, false, false, false]),
    (ZONE, &[true, false, false]),
];

/// The schema, as facts.
pub const SCHEMA: &str = include_str!("schema.df");

pub fn schema() -> Result<Schema> {
    Schema::parse(SCHEMA, "dform-provider-ovh/src/schema.df")
}

/// How long an instance's Create waits for it to be ACTIVE, and a Delete
/// for it to go. dform's own timeout per call (R-81, `[providers.ovh]
/// timeout`) may be shorter: past it a Create is found again by its key.
const CREATE_WAIT: Duration = Duration::from_secs(15 * 60);
const DELETE_WAIT: Duration = Duration::from_secs(5 * 60);

/// How often an object is polled while it is made, changed or deleted:
/// `DFORM_OVH_POLL_MS`, else 5s.
fn poll_every(env: &dyn Fn(&str) -> Option<String>) -> Duration {
    env("DFORM_OVH_POLL_MS")
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(5), Duration::from_millis)
}

/// Says the object's status as the API gives it, and a message.
type Say<'a> = &'a dyn Fn(&str, Option<&str>);

/// What a Create that timed out is found by (R-195): its type and the
/// value of each attribute its `type_lookup` names, in that order. Not
/// its identity (the API's id, which state holds) and not what replaces
/// it (`force_new`): content the API lets this provider list and match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Made {
    typ: String,
    key: Vec<(String, String)>,
}

impl Made {
    /// The value of the lookup's attribute `attr`, `""` if it has none.
    fn get(&self, attr: &str) -> &str {
        self.key
            .iter()
            .find(|(a, _)| a == attr)
            .map_or("", |(_, v)| v)
    }
}

impl std::fmt::Display for Made {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (a, v)) in self.key.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}{a} {v:?}")?;
        }
        Ok(())
    }
}

/// The schema predicate naming what a Create that timed out is found by.
pub const LOOKUP: &str = "type_lookup";

/// The schema's `type_lookup(Type, [Attr, ..])` rows, by type: each
/// attribute one of the type's.
pub fn lookups(schema: &Schema) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    for f in schema.facts.iter().filter(|f| f.pred == LOOKUP) {
        let (Some(typ), Some(Value::List(attrs))) = (
            f.args.first().and_then(|t| t.as_str()),
            f.args.get(1).and_then(|t| t.ground()),
        ) else {
            bail!("{LOOKUP}: a type and a list of its attributes");
        };
        let attrs = attrs
            .iter()
            .map(|a| match a.as_str() {
                Some(a) if schema.attr(typ, a).is_some() => Ok(a.to_string()),
                Some(a) => Err(anyhow!("{LOOKUP}({typ}): {a} is not an attribute of {typ}")),
                None => Err(anyhow!(
                    "{LOOKUP}({typ}): an attribute is named by a string"
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        if out.insert(typ.to_string(), attrs).is_some() {
            bail!("{LOOKUP}({typ}) declared twice");
        }
    }
    Ok(out)
}

/// The account a configured provider reaches.
struct Account {
    client: Client,
    /// The project's id (its `serviceName`), when one is configured.
    project: Option<String>,
    /// dform's cache directory, where the ids of projects named by
    /// description are kept ([`resolve_project`]).
    cache: Option<std::path::PathBuf>,
    /// How often an object is polled while it changes.
    poll: Duration,
}

/// A configured provider.
struct Configured {
    account: std::result::Result<Arc<Account>, String>,
    /// The program's settings are still to come (a `use ovh { .. }`
    /// block): a data source answers "not yet".
    awaiting: bool,
}

pub struct Ovh {
    schema: Schema,
    state: RwLock<Option<Arc<Configured>>>,
    /// Each type's `type_lookup`.
    lookups: BTreeMap<String, Vec<String>>,
    /// The Creates this process was sent, by idempotency key.
    made: Mutex<BTreeMap<String, Made>>,
    /// The private addresses instances asked at Plan, by network.
    asked: Mutex<network::Asked>,
}

/// Why an Apply failed.
enum Failed {
    Refused(String),
    MaybeApplied(String),
}

impl From<Failed> for CallError {
    fn from(f: Failed) -> CallError {
        match f {
            Failed::Refused(m) => CallError::Refused(m),
            Failed::MaybeApplied(m) => CallError::MaybeApplied(m),
        }
    }
}

/// An API failure inside an Apply: no answer may have taken effect; an
/// answer refused it (a 5xx or 429 says so, and dform sends it again).
fn failed(at: &str, e: api::Error) -> Failed {
    match e {
        api::Error::Unreachable { .. } => Failed::MaybeApplied(format!("{at}: {e}")),
        api::Error::Status { .. } => Failed::Refused(format!("{at}: {e}")),
    }
}

/// A call refused with `e`.
fn invalid(e: anyhow::Error) -> CallError {
    CallError::Refused(format!("{e:#}"))
}

fn refused(at: &str, e: impl std::fmt::Display) -> Failed {
    Failed::Refused(format!("{at}: {e}"))
}

fn address(typ: &str, name: &str) -> Address {
    Address {
        typ: typ.to_string(),
        name: name.to_string(),
    }
}

fn s<'a>(doc: &'a Json, k: &str) -> Option<&'a str> {
    doc.get(k).and_then(Json::as_str)
}

/// A string attribute an Apply needs, known.
fn need<'a>(at: &str, doc: &'a Json, k: &str) -> std::result::Result<&'a str, Failed> {
    match doc.get(k) {
        Some(Json::String(v)) => Ok(v),
        Some(v) if marker(v).is_some() => Err(Failed::Refused(format!(
            "{at}: {k} is {}, not a value the provider can send",
            provider::fmt_value(Some(v))
        ))),
        _ => Err(Failed::Refused(format!("{at}: {k} is not set"))),
    }
}

/// A status as the API gives it, or `unknown`.
/// The world's document `prior` as Plan compares it with `d`: an
/// instance's flavor or image the API no longer names (an image retired
/// from the region) is unknown, not changed, so it does not replace the
/// instance. Read fails rather than leave one out on a failed call.
fn known(typ: &str, prior: &Json, d: &Json) -> Json {
    let mut p = prior.clone();
    if typ == INSTANCE {
        for k in ["flavor", "image"] {
            if p.get(k).is_none()
                && let Some(v) = d.get(k)
            {
                p[k] = v.clone();
            }
        }
    }
    p
}

/// Whether `d` asks of attribute `k` what `prior` (the object as Read
/// answered it) does not have in the same region: every attribute of an
/// object not there.
fn changed(prior: Option<&Json>, d: &Json, k: &str) -> bool {
    prior.is_none_or(|p| p.get("region") != d.get("region") || p.get(k) != d.get(k))
}

fn status(o: &Json) -> String {
    s(o, "status").unwrap_or("unknown").to_string()
}

impl Default for Ovh {
    fn default() -> Ovh {
        Ovh::new()
    }
}

impl Ovh {
    pub fn new() -> Ovh {
        let schema = schema().expect("the provider's schema parses");
        Ovh {
            lookups: lookups(&schema).expect("the provider's lookups are its types' attributes"),
            schema,
            state: RwLock::new(None),
            made: Mutex::new(BTreeMap::new()),
            asked: Mutex::new(network::Asked::default()),
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn configured(&self) -> std::result::Result<Arc<Configured>, String> {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "the provider is not configured".to_string())
    }

    fn account(&self, what: &str) -> Result<Arc<Account>> {
        let c = self.configured().map_err(|e| anyhow!("{what}: {e}"))?;
        c.account
            .clone()
            .map_err(|why| anyhow!("{what}: no OVH account ({why})"))
    }

    /// The account and its project, for an Apply.
    fn project_for(&self, at: &str) -> std::result::Result<(Arc<Account>, String), Failed> {
        self.project(at).map_err(|e| refused(at, format!("{e:#}")))
    }

    /// Poll the object at `path`, `last` as the API last answered it,
    /// until its status is one of `done`; each status the API gives that
    /// is not the last one's is said (R-130). A status in `failed` (an
    /// `error`) stops it with a note: the object is kept in state, to be
    /// replaced or deleted. Past `limit`, a note; a failed poll is asked
    /// again.
    #[allow(clippy::too_many_arguments)]
    fn settle(
        &self,
        a: &Account,
        at: &str,
        path: &str,
        last: Json,
        done: &[&str],
        failed: &[&str],
        limit: Duration,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<Json, Failed> {
        let done = |o: &Json| done.contains(&status(o).as_str());
        self.settle_until(a, at, path, last, &done, failed, limit, notes, say)
    }

    /// [`Ovh::settle`] until the object is `done`, which may ask more of
    /// it than its status (a resized instance is ACTIVE again on its new
    /// flavor).
    #[allow(clippy::too_many_arguments)]
    fn settle_until(
        &self,
        a: &Account,
        at: &str,
        path: &str,
        mut last: Json,
        done: &dyn Fn(&Json) -> bool,
        failed: &[&str],
        limit: Duration,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<Json, Failed> {
        let start = Instant::now();
        let mut said = status(&last);
        say(&said, None);
        while !done(&last) {
            if failed.contains(&said.as_str()) {
                notes.push(format!(
                    "{at}: it is {said}; it is kept in state, to be replaced or deleted"
                ));
                break;
            }
            if start.elapsed() > limit {
                notes.push(format!(
                    "{at}: it is still {said} after {}s",
                    limit.as_secs()
                ));
                break;
            }
            std::thread::sleep(a.poll);
            match a.client.get_opt(path) {
                Ok(Some(o)) => {
                    if status(&o) != said {
                        said = status(&o);
                        say(&said, None);
                    }
                    last = o;
                }
                Ok(None) => {
                    return Err(Failed::MaybeApplied(format!(
                        "{at}: it went away while it changed"
                    )));
                }
                Err(e) => say(&said, Some(&format!("a poll failed, asking again: {e:#}"))),
            }
        }
        Ok(last)
    }

    /// DELETE `path` (gone already is done), then, when `wait`, poll it
    /// until the API no longer answers it, saying each status it gives.
    fn delete_at(
        &self,
        a: &Account,
        at: &str,
        path: &str,
        wait: bool,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        match a.client.delete(path) {
            Ok(_) => {}
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => return Err(failed(at, e)),
        }
        if !wait {
            return Ok(());
        }
        let start = Instant::now();
        let mut said = None;
        loop {
            match a.client.get_opt(path) {
                Ok(None) => break,
                Ok(Some(o)) => {
                    let now = status(&o);
                    if matches!(now.as_str(), "deleted" | "DELETED") {
                        break;
                    }
                    if said.as_ref() != Some(&now) {
                        say(&now, None);
                        said = Some(now);
                    }
                }
                Err(_) => {}
            }
            if start.elapsed() > DELETE_WAIT {
                notes.push(format!(
                    "{at}: it is still being deleted after {}s",
                    DELETE_WAIT.as_secs()
                ));
                break;
            }
            std::thread::sleep(a.poll);
        }
        Ok(())
    }

    /// The account and its project.
    fn project(&self, what: &str) -> Result<(Arc<Account>, String)> {
        let a = self.account(what)?;
        let p = a.project.clone().ok_or_else(|| {
            anyhow!(
                "{what}: no project: name it in the program (`use ovh {{ project = \"..\" }}`) \
                 or in OVH_CLOUD_PROJECT_SERVICE"
            )
        })?;
        Ok((a, p))
    }

    /// Configure from `config` (dform's, with the program's `settings` the
    /// second time): the account's project id, for `expect_account`, and
    /// what the credentials are and lack, as notes.
    pub fn configure(&self, config: &Json) -> Result<pb::ConfigureResponse> {
        let env = |k: &str| std::env::var(k).ok();
        self.configure_with(config, &env, &config::default_files())
    }

    /// `configure` with the environment `env` and the configuration files
    /// `files` (a test's own, so no real `ovh.conf` is read).
    pub fn configure_with(
        &self,
        config: &Json,
        env: &dyn Fn(&str) -> Option<String>,
        files: &[std::path::PathBuf],
    ) -> Result<pb::ConfigureResponse> {
        let settings = config.get("settings").filter(|v| v.is_object());
        let deferred = config.get("deferred") == Some(&Json::Bool(true));
        let set = |c: Configured| {
            *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
        };
        if settings.is_none() && deferred {
            set(Configured {
                account: Err("the program configures it (`use ovh { .. }`) and has not yet".into()),
                awaiting: true,
            });
            return Ok(pb::ConfigureResponse::default());
        }
        let setting = |k: &str| -> Result<Option<String>> {
            match settings.and_then(|s| s.get(k)) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(v)) => Ok(Some(v.clone())),
                Some(v) => bail!(
                    "provider ovh: {k} is {}, not a string",
                    provider::fmt_value(Some(v))
                ),
            }
        };
        let creds = config::resolve(setting("endpoint")?.as_deref(), env, files);
        let creds = match (creds, settings) {
            (Ok(c), _) => c,
            // The program names the account: it cannot be reached.
            (Err(e), Some(_)) => return Err(e.context("provider ovh")),
            (Err(e), None) => {
                set(Configured {
                    account: Err(format!("{e:#}")),
                    awaiting: false,
                });
                return Ok(pb::ConfigureResponse::default());
            }
        };
        let project = setting("project")?
            .or_else(|| env("OVH_CLOUD_PROJECT_SERVICE").filter(|v| !v.is_empty()));
        let cache = config
            .get("cache")
            .and_then(Json::as_str)
            .map(std::path::PathBuf::from);
        let client = Client::new(creds).with_cache(cache.as_deref());
        // Run outside a program and a project, as `provider check` runs
        // it: which credentials, and what they may do.
        let full = settings.is_none() && cache.is_none();
        let notes = crate::credential::notes(&client, full, cache.as_deref(), unix_now());
        let project = match project {
            Some(p) => Some(resolve_project(&client, &p, cache.as_deref())?),
            None => None,
        };
        let account = project.clone();
        set(Configured {
            account: Ok(Arc::new(Account {
                client,
                project,
                cache,
                poll: poll_every(env),
            })),
            awaiting: false,
        });
        Ok(pb::ConfigureResponse {
            account,
            notes,
            ..Default::default()
        })
    }

    // Read.

    /// The object `remote` of type `typ`, if it is there, `prior` its
    /// computed values as dform last kept them: an instance's own ports
    /// (`map::Owned`).
    pub fn read(
        &self,
        typ: &str,
        remote: &str,
        name: &str,
        prior: Option<&Json>,
    ) -> Result<Option<(Json, Json)>> {
        let at = format!("read {}", address(typ, name));
        Ok(match typ {
            INSTANCE => {
                let (a, p) = self.project(&at)?;
                self.read_instance(&a, &p, remote, &prior.into())?
            }
            SSH_KEY => {
                let (a, p) = self.project(&at)?;
                self.read_ssh_key(&a, &p, remote)?
            }
            RECORD => {
                let a = self.account(&at)?;
                self.read_record(&a, remote)?
            }
            CONTAINER => {
                let (a, p) = self.project(&at)?;
                self.read_container(&a, &p, remote)?
            }
            USER => {
                let (a, p) = self.project(&at)?;
                self.read_user(&a, &p, remote)?
            }
            S3_CREDENTIAL => {
                let (a, p) = self.project(&at)?;
                self.read_s3_credential(&a, &p, remote)?
            }
            VOLUME => {
                let (a, p) = self.project(&at)?;
                self.read_volume(&a, &p, remote)?.map(|o| map::volume(&o))
            }
            NETWORK => {
                let (a, p) = self.project(&at)?;
                self.read_network(&a, &p, remote)?.map(|o| map::network(&o))
            }
            SUBNET => {
                let (a, p) = self.project(&at)?;
                let (network, _) = map::subnet_parts(remote);
                self.read_subnet(&a, &p, remote)?
                    .map(|o| map::subnet(network, &o))
            }
            _ => bail!("{at}: the ovh provider has no type {typ}"),
        })
    }

    // Plan.

    /// Validate `desired` and diff it against `prior`; whether a change
    /// replaces the object, and why where the schema does not say.
    pub fn plan(
        &self,
        typ: &str,
        name: &str,
        _remote: &str,
        prior: Option<&Json>,
        desired: Option<&Json>,
    ) -> Result<provider::Planned> {
        let at = format!("plan {}", address(typ, name));
        if !self.schema.knows_type(typ) {
            bail!("{at}: the ovh provider has no type {typ}");
        }
        // An instance's networks and the addresses it asks, which one
        // that goes asks no more.
        if typ == INSTANCE {
            let who = dform_core::report::address(&address(typ, name));
            self.check_networks(&at, &who, desired.unwrap_or(&Json::Null))?;
        }
        let Some(d) = desired else {
            return Ok(provider::Planned {
                changes: diff(&self.schema, typ, prior, None),
                ..Default::default()
            });
        };
        let pool = self.check_desired(&at, typ, prior, d)?;
        let known = prior.map(|p| known(typ, p, d));
        let prior = known.as_ref();
        // An instance's user data is write-only (R-106): the API never
        // answers it, and dform compares it with the digest state keeps,
        // giving the program's value in `prior` when it is the same.
        let mut changes = diff(&self.schema, typ, prior, Some(d));
        // A subnet made without a pool is given the range's hosts: the
        // plan says which.
        if prior.is_none()
            && let Some(pool) = pool
        {
            changes.push(provider::Change {
                path: "pool".into(),
                before: None,
                after: Some(Json::String(pool)),
                sensitive: false,
                note: None,
            });
        }
        if typ == NETWORK && prior.is_none() {
            self.check_vrack(&at)?;
        }
        let Some(prior) = prior else {
            return Ok(provider::Planned {
                changes,
                ..Default::default()
            });
        };
        // A path the schema forces new replaces the object, which dform
        // names; else one the API changes in one direction only, said.
        let forced = changes
            .iter()
            .any(|c| self.schema.forces_new(typ, &norm_path(&c.path)));
        let reason = match forced {
            true => None,
            false => self.replaced(&at, typ, prior, d),
        };
        Ok(provider::Planned {
            changes,
            replaces: forced || reason.is_some(),
            reason,
        })
    }

    /// Refuse a desired document `d` of `typ` the API would: a required
    /// attribute unset, an instance's flavor or image the region does
    /// not offer, a bootable volume's image, a subnet's pool. A subnet's
    /// pool, when the plan gives it one. A flavor or an image `prior` (the
    /// object as Read answered it) has in the same region is the API's
    /// answer already, and is not asked again.
    fn check_desired(
        &self,
        at: &str,
        typ: &str,
        prior: Option<&Json>,
        d: &Json,
    ) -> Result<Option<String>> {
        for ((t, p), a) in &self.schema.attrs {
            if t == typ && a.has("required") && d.get(p).is_none() {
                bail!("{at}: required attribute {p} is not set");
            }
        }
        if typ == INSTANCE {
            self.check_instance(at, prior, d)?;
        }
        // A bootable volume's image is the region's, as an instance's is.
        if typ == VOLUME
            && let (Some(region), Some(image)) = (s(d, "region"), s(d, "image"))
            && changed(prior, d, "image")
            && let Ok((a, p)) = self.project(at)
        {
            self.image_id(&a, &p, region, image)
                .map_err(|e| anyhow!("{at}: image: {e:#}"))?;
        }
        match typ {
            SUBNET => network::check_subnet(at, d),
            _ => Ok(None),
        }
    }

    /// Why a change of `prior` to `d` replaces the object where no path
    /// the schema forces new says so: one the API changes in one
    /// direction only.
    fn replaced(&self, at: &str, typ: &str, prior: &Json, d: &Json) -> Option<String> {
        let reasons: Vec<String> = match typ {
            VOLUME => volume::shrinks(prior, d).into_iter().collect(),
            NETWORK => network::leaves(prior, d).into_iter().collect(),
            // A smaller flavor; an address asked anew on a network it is
            // on.
            INSTANCE => {
                let moved: Vec<String> = network::readdressed(prior, d)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let moved = (!moved.is_empty())
                    .then(|| format!("{}; no API call changes a port's address", moved.join("; ")));
                self.flavor_shrinks(at, prior, d)
                    .into_iter()
                    .chain(moved)
                    .collect()
            }
            _ => Vec::new(),
        };
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }

    // Apply.

    fn apply(
        &self,
        r: &pb::ApplyRequest,
        progress: backend::Progress,
    ) -> std::result::Result<pb::ApplyResponse, Failed> {
        let op = pb::Op::try_from(r.op).unwrap_or(pb::Op::Unspecified);
        if op == pb::Op::EndTick {
            return Ok(pb::ApplyResponse::default());
        }
        let typ = r.r#type.as_str();
        let at = format!("apply {}", address(typ, &r.name));
        if !self.schema.knows_type(typ) {
            return Err(refused(&at, format!("the ovh provider has no type {typ}")));
        }
        let config = match r.config.as_ref().map(wire::from_doc).transpose() {
            Ok(c) => c.unwrap_or(Json::Null),
            Err(e) => return Err(refused(&at, format!("{e:#}"))),
        };
        let key = r.idempotency_key.as_str();
        let mut notes = Vec::new();
        // What the API says of the object as it changes (R-130).
        let addr = address(typ, &r.name).to_string();
        let say = |status: &str, message: Option<&str>| {
            progress(backend::event(&addr, Some(status), message));
        };
        let (remote, attrs, computed) = match op {
            pb::Op::Create => self.create(typ, &at, &config, key, &mut notes, &say)?,
            pb::Op::Update | pb::Op::Adopt => {
                // `keep` (R-164): an update sends no write-only attribute
                // (an instance's `user_data` is set at its creation only),
                // so one is left as it is by not being sent; another path
                // it cannot leave.
                let wo = self.schema.write_only_of(typ);
                if let Some(p) = r.keep.iter().find(|p| !wo.contains(&p.as_str())) {
                    return Err(refused(
                        &at,
                        format!("keep {p}: only a write-only attribute is left as it is"),
                    ));
                }
                let prior = r.prior.as_ref().map(wire::from_doc).transpose();
                let prior = prior.map_err(|e| refused(&at, format!("{e:#}")))?;
                self.update(typ, &at, &r.remote, &config, prior, &mut notes, &say)?
            }
            pb::Op::Delete => {
                self.delete(typ, &at, &r.remote, &mut notes, &say)?;
                return Ok(pb::ApplyResponse {
                    notes,
                    ..Default::default()
                });
            }
            pb::Op::Replace => self.replace(r, &at, &config, &mut notes, &say)?,
            _ => return Err(refused(&at, "no operation")),
        };
        let computed = self.outward(typ, &r.name, &computed);
        Ok(pb::ApplyResponse {
            remote,
            attrs: Some(wire::doc(&attrs)),
            computed: Some(wire::doc(&computed)),
            elapsed_ms: 0,
            notes,
        })
    }

    /// An object replaced: the old one deleted first when the replacement
    /// has its key or the host does not create first, then the new one
    /// created.
    fn replace(
        &self,
        r: &pb::ApplyRequest,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (typ, key) = (r.r#type.as_str(), r.idempotency_key.as_str());
        // Read as dform kept it: an instance's own ports, and the
        // addresses asked of them.
        let prior = r.prior.as_ref().map(wire::from_doc).transpose();
        let prior = prior.map_err(|e| refused(at, format!("{e:#}")))?;
        let old = self
            .read(typ, &r.remote, &r.name, prior.as_ref())
            .map_err(|e| refused(at, format!("{e:#}")))?;
        // The key is the name: the old object goes first unless the
        // replacement has another key, or its type has none (two S3
        // credentials of a user are two objects).
        let same = old.as_ref().is_none_or(|(attrs, _)| {
            self.lookups.contains_key(typ) && self.key_of(typ, attrs) == self.key_of(typ, config)
        });
        if r.create_first && same {
            notes.push(format!(
                "{at}: the replacement has the old object's key, so the old one goes first"
            ));
        }
        let mut first = !r.create_first || same;
        if typ == INSTANCE
            && let Some((_, computed)) = &old
        {
            first |= network::holds_asked(at, computed, config, !first, notes);
        }
        if first {
            self.delete(typ, at, &r.remote, notes, say)?;
        }
        self.create(typ, at, config, key, notes, say)
    }

    /// What a Create of `doc` is found by: each attribute of the type's
    /// lookup, a required one's absence leaving it unknown, another's
    /// (a record's subdomain at the apex) `""`.
    fn key_of(&self, typ: &str, doc: &Json) -> Option<Made> {
        let key = self
            .lookups
            .get(typ)?
            .iter()
            .map(|a| match s(doc, a) {
                Some(v) => Some((a.clone(), v.to_string())),
                None if self.schema.attr(typ, a)?.has("required") => None,
                None => Some((a.clone(), String::new())),
            })
            .collect::<Option<_>>()?;
        Some(Made {
            typ: typ.to_string(),
            key,
        })
    }

    /// The remote id of the object `made` names, if it exists.
    fn find(&self, made: &Made) -> Result<Option<String>> {
        let k = |a: &str| made.get(a);
        Ok(match made.typ.as_str() {
            INSTANCE => self.find_instance(k("name"), k("region"))?,
            SSH_KEY => self.find_ssh_key(k("name"))?,
            RECORD => self.find_record(k("zone"), k("type"), k("subdomain"), k("target"))?,
            CONTAINER => self.find_container(k("region"), k("name"))?,
            USER => self.find_user(k("description"))?,
            S3_CREDENTIAL => self.find_s3_credential(k("user"), k("access"))?,
            VOLUME => self.find_volume(k("name"), k("region"))?,
            NETWORK => self.find_network(k("name"))?,
            SUBNET => self.find_subnet(k("network"), k("region"), k("range"))?,
            typ => bail!("the ovh provider has no lookup for {typ}"),
        })
    }

    /// `provider.created(Type, Name, Key, Remote)`: what a Create this
    /// process was sent with `key` made.
    pub fn created(&self, key: &str) -> Result<Option<String>> {
        let made = self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned();
        match made {
            Some(m) => self.find(&m),
            None => Ok(None),
        }
    }

    fn create(
        &self,
        typ: &str,
        at: &str,
        config: &Json,
        key: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        // Nothing of the program's names an S3 credential (the API makes
        // its access key): a Create sent again is answered by the one this
        // process made under the key, if it was answered.
        if typ == S3_CREDENTIAL {
            if let Some(remote) = self
                .created(key)
                .map_err(|e| refused(at, format!("{e:#}")))?
            {
                return self.update(typ, at, &remote, config, None, notes, say);
            }
            return self.create_s3_credential(at, config, key);
        }
        let made = self
            .key_of(typ, config)
            .ok_or_else(|| refused(at, "its key (what names it in the account) is not known"))?;
        // An object of the same key: this process's own (the same Create
        // sent again) is the answer; another is not taken over.
        let ours = !key.is_empty()
            && self.made.lock().unwrap_or_else(|e| e.into_inner()).get(key) == Some(&made);
        if let Some(remote) = self
            .find(&made)
            .map_err(|e| refused(at, format!("{e:#}")))?
        {
            // The same Create again: what it made, brought to the
            // document (a user's roles, a volume's attachment),
            // owning what it was made with (an instance's ports).
            if ours {
                let prior = match typ {
                    INSTANCE => self.made_instance(at, &remote, config)?,
                    _ => None,
                };
                return self.update(typ, at, &remote, config, prior, notes, say);
            }
            return Err(refused(
                at,
                format!(
                    "{typ} {remote} already exists with this key ({made}); adopt it or name another"
                ),
            ));
        }
        if !key.is_empty() {
            self.made
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), made);
        }
        match typ {
            INSTANCE => self.create_instance(at, config, notes, say),
            SSH_KEY => self.create_ssh_key(at, config),
            RECORD => self.create_record(at, config, notes),
            CONTAINER => self.create_container(at, config),
            USER => self.create_user(at, config, notes, say),
            VOLUME => self.create_volume(at, config, notes, say),
            NETWORK => self.create_network(at, config, notes, say),
            SUBNET => self.create_subnet(at, config),
            _ => Err(refused(at, format!("the ovh provider has no type {typ}"))),
        }
    }

    /// The object a Create or an Update made, read, `prior` what it owns
    /// below it.
    fn read_made(
        &self,
        typ: &str,
        at: &str,
        remote: &str,
        prior: Option<&Json>,
    ) -> std::result::Result<(Json, Json), Failed> {
        self.read(typ, remote, "", prior)
            .map_err(|e| refused(at, format!("{e:#}")))?
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: {typ} {remote} is gone again")))
    }

    /// `remote` brought to `config`, `prior` its computed values as dform
    /// keeps them; what it now is, read with what it owns below it (the
    /// ports an instance had, and the ones attached here).
    #[allow(clippy::too_many_arguments)]
    fn update(
        &self,
        typ: &str,
        at: &str,
        remote: &str,
        config: &Json,
        prior: Option<Json>,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let mut prior = prior.unwrap_or(Json::Null);
        let now = self
            .read(typ, remote, "", Some(&prior))
            .map_err(|e| refused(at, format!("{e:#}")))?
            .ok_or_else(|| refused(at, format!("{typ} {remote} is not there")))?;
        match typ {
            INSTANCE => {
                let owned = map::Owned::from(Some(&prior));
                let own = self.update_instance(at, remote, &now.0, config, &owned, notes, say)?;
                let fixed = network::fixed(config);
                for (net, port) in own {
                    map::own_port(&mut prior, &net, &port, fixed.contains(&net));
                }
            }
            RECORD => self.update_record(at, remote, &now, config)?,
            CONTAINER => self.update_container(at, remote, &now.1, config)?,
            USER => self.update_user(at, remote, &now.0, config)?,
            VOLUME => self.update_volume(at, remote, config, notes, say)?,
            NETWORK => self.update_network(at, remote, &now, config, notes, say)?,
            // Nothing of an SSH key or an S3 credential changes in place
            // (the schema replaces it).
            _ => {}
        }
        let (attrs, computed) = self.read_made(typ, at, remote, Some(&prior))?;
        Ok((remote.to_string(), attrs, computed))
    }

    fn delete(
        &self,
        typ: &str,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        match typ {
            INSTANCE => self.delete_instance(at, remote, notes, say)?,
            SSH_KEY => self.delete_ssh_key(at, remote)?,
            RECORD => self.delete_record(at, remote, notes)?,
            CONTAINER => self.delete_container(at, remote, notes, say)?,
            USER => self.delete_user(at, remote, notes, say)?,
            S3_CREDENTIAL => self.delete_s3_credential(at, remote, notes, say)?,
            VOLUME => self.delete_volume(at, remote, notes, say)?,
            NETWORK => self.delete_network(at, remote, notes, say)?,
            SUBNET => self.delete_subnet(at, remote, notes, say)?,
            _ => return Err(refused(at, format!("the ovh provider has no type {typ}"))),
        }
        Ok(())
    }

    /// A computed document as it leaves the provider: each sensitive
    /// value (an S3 credential's secret) is its label, `{"$secret": "T/N#P"}`;
    /// the value stays with the API, and Reveal reads it there.
    fn outward(&self, typ: &str, name: &str, computed: &Json) -> Json {
        let mut out = computed.clone();
        for (path, class) in self.schema.computed_of(typ) {
            if class == dform_core::value::NullClass::Secret
                && provider::get_path(&out, &path).is_some_and(|v| marker(v).is_none())
            {
                let label = dform_core::value::null_label(typ, name, &path);
                provider::set_path(&mut out, &path, provider::secret_json(&label));
            }
        }
        out
    }

    // Query.

    pub fn query(&self, pred: &str, plus: &[bool], inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        if pred == CREATED {
            return self.created_rows(inputs);
        }
        let Some((_, binding)) = EXTERNS.iter().find(|(p, _)| *p == pred) else {
            bail!("the ovh provider answers no extern {pred}");
        };
        if plus != *binding {
            bail!("{pred} is asked with its first column bound, and only it");
        }
        let [Value::Str(input)] = inputs else {
            bail!("{pred}: its input is a string");
        };
        // Asked before the program's settings came: not yet (an open null
        // in each output column), asked again once they have.
        if self.configured().is_ok_and(|c| c.awaiting) {
            return Ok(vec![not_yet(pred, binding, input)]);
        }
        let what = format!("answer {pred}({input:?}, ..)");
        Ok(match pred {
            REGION => self.region_rows(&what, input)?,
            ZONE => self.zone_rows(&what, input)?,
            FLAVOR => {
                let (a, p) = self.project(&what)?;
                map::flavor_rows(input, &*self.list(&a, &p, "flavor", input)?)
            }
            _ => {
                let (a, p) = self.project(&what)?;
                map::image_rows(input, &*self.list(&a, &p, "image", input)?)
            }
        })
    }

    /// `provider.created(Type, Name, Key, Remote)` asked with its first
    /// three columns bound.
    fn created_rows(&self, inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        let [Value::Str(typ), name, Value::Str(key)] = inputs else {
            bail!("{CREATED} is asked with Type, Name and Key bound");
        };
        if key.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .created(key)?
            .map(|remote| {
                vec![
                    Value::Str(typ.clone()),
                    name.clone(),
                    Value::Str(key.clone()),
                    Value::Str(remote),
                ]
            })
            .into_iter()
            .collect())
    }

    /// `ovh.region(Project, Region, Status)`: each region of the project
    /// `input` names, with its status.
    fn region_rows(&self, what: &str, input: &str) -> Result<Vec<Vec<Value>>> {
        let a = self.account(what)?;
        let p = resolve_project(&a.client, input, a.cache.as_deref())?;
        let names = a.client.list(&format!("/cloud/project/{p}/region"))?;
        let mut rows = Vec::new();
        for n in names
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
        {
            let r = a
                .client
                .get(&format!("/cloud/project/{p}/region/{}", escape(n)))?;
            rows.push(vec![
                Value::Str(input.to_string()),
                Value::Str(n.to_string()),
                Value::Str(s(&r, "status").unwrap_or_default().to_string()),
            ]);
        }
        Ok(rows)
    }

    /// Documents for `dform provider check`: an instance, renamed in
    /// place, with no user data (Read cannot answer it); one with user
    /// data for Plan's sensitive check. The region, flavor and image are
    /// `OVH_CHECK_REGION`, `OVH_CHECK_FLAVOR` and `OVH_CHECK_IMAGE`, else
    /// the cheapest Linux instance of BHS5.
    pub fn examples(&self) -> Vec<pb::Example> {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let doc = |name: &str| {
            json!({
                "name": name,
                "region": env("OVH_CHECK_REGION", "BHS5"),
                "flavor": env("OVH_CHECK_FLAVOR", "d2-2"),
                "image": env("OVH_CHECK_IMAGE", "Ubuntu 24.04"),
            })
        };
        let mut with_data = doc("dform-check-data");
        with_data["user_data"] = json!("#cloud-config\n");
        let ex = |create: Json, update: Json, required: &str| pb::Example {
            r#type: INSTANCE.into(),
            create: Some(wire::doc(&create)),
            update: Some(wire::doc(&update)),
            required: required.into(),
        };
        vec![
            ex(doc("dform-check"), doc("dform-check-renamed"), "flavor"),
            ex(with_data.clone(), with_data, ""),
        ]
    }
}

/// A data source asked before the program's settings came: not yet, an
/// open null in each output column, asked again once they have.
fn not_yet(pred: &str, binding: &[bool], input: &str) -> Vec<Value> {
    let ins = dform_core::spell::bare(&Value::Str(input.to_string()));
    binding
        .iter()
        .enumerate()
        .map(|(c, plus)| match plus {
            true => Value::Str(input.to_string()),
            false => Value::Null {
                label: dform_core::value::null_label(pred, &ins, &(c + 1).to_string()),
                class: dform_core::value::NullClass::Open,
                ty: String::new(),
            },
        })
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The project `given` names: its id (`serviceName`), or the description
/// or name it has in the console. An id, or the id `cache` (dform's cache
/// directory) kept for a description, costs one GET of that project,
/// which must still answer to it; otherwise every project of the account
/// is asked, and the id found is kept.
pub fn resolve_project(
    client: &Client,
    given: &str,
    cache: Option<&std::path::Path>,
) -> Result<String> {
    let file = cache.map(|c| c.join(api::CACHE_FILE));
    let kept = file.as_deref().map(api::read_cache).unwrap_or_default();
    let key = format!("{} {given}", client.credentials().endpoint);
    let is_id = given.len() == 32 && given.bytes().all(|b| b.is_ascii_hexdigit());
    let guess = kept
        .get(&key)
        .cloned()
        .or_else(|| is_id.then(|| given.to_string()));
    if let Some(id) = guess
        && let Ok(Some(p)) = client.get_opt(&format!("/cloud/project/{}", escape(&id)))
        && (id == given || [s(&p, "description"), s(&p, "projectName")].contains(&Some(given)))
    {
        return Ok(id);
    }
    let id = resolve_listed(client, given)?;
    if let Some(f) = &file
        && id != given
    {
        api::write_cache(f, &key, &id);
    }
    Ok(id)
}

/// `resolve_project` by listing the account's projects.
fn resolve_listed(client: &Client, given: &str) -> Result<String> {
    let ids = client.get("/cloud/project")?;
    let ids: Vec<&str> = ids
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .collect();
    if ids.contains(&given) {
        return Ok(given.to_string());
    }
    // Each project's description, asked at once: an account with several
    // waits one round trip, not one per project.
    let projects: Vec<api::Result<Json>> = dform_sdk::at_once(
        ids.iter()
            .map(|id| move || client.get(&format!("/cloud/project/{}", escape(id)))),
    );
    let mut seen = Vec::new();
    for (id, p) in ids.iter().zip(projects) {
        let p = p?;
        let names = [s(&p, "description"), s(&p, "projectName")];
        if names.contains(&Some(given)) {
            return Ok(id.to_string());
        }
        seen.push(format!(
            "{id} ({})",
            names
                .into_iter()
                .flatten()
                .next()
                .unwrap_or("no description")
        ));
    }
    bail!(
        "no Public Cloud project {given:?} on {}: the account has {}",
        client.credentials().endpoint,
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(", ")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every type but the S3 credential (which nothing of the program's
    /// names) has a lookup, of its own attributes; a Create of a record
    /// is found by its content, the apex's subdomain empty.
    #[test]
    fn each_type_is_found_by_its_lookup() {
        let ovh = Ovh::new();
        for typ in ovh.schema.provider_of.keys() {
            let has = ovh.lookups.contains_key(typ);
            assert_eq!(has, typ != S3_CREDENTIAL, "{typ}: type_lookup");
        }
        let doc = json!({"zone": "example.com", "type": "A", "target": "10.0.0.1", "ttl": 60});
        let made = ovh.key_of(RECORD, &doc).unwrap();
        assert_eq!(
            made.to_string(),
            r#"zone "example.com", subdomain "", type "A", target "10.0.0.1""#
        );
        // A required attribute missing leaves it unknown.
        assert_eq!(ovh.key_of(RECORD, &json!({"zone": "example.com"})), None);
    }

    #[test]
    fn a_lookup_names_attributes_of_its_type() {
        let s = Schema::parse(
            "type_attr(\"t.a\", \"name\", \"string\", [\"required\"])\n\
             type_lookup(\"t.a\", [\"name\", \"nmae\"])\n",
            "test",
        )
        .unwrap();
        assert_eq!(
            lookups(&s).unwrap_err().to_string(),
            "type_lookup(t.a): nmae is not an attribute of t.a"
        );
    }
}
