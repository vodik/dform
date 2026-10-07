//! The provider: the plugin protocol (`Handler`) over the OVH API.
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
//! Remote ids: an instance's and an SSH key's are the API's ids; a DNS
//! record's is `ZONE/ID`. Plan diffs locally (`provider::diff`), and an
//! instance's flavor and image are checked against what its region offers.
//! A Create looks for an object of the same key first: one this process
//! made under the same idempotency key is the answer (a Create sent again
//! after a timeout, R-81), another is refused, to be adopted or renamed.
//! `provider.created` answers by the same key, so a Create whose answer
//! was lost is adopted. An instance's Create waits for it to be ACTIVE; a
//! Delete waits for it to go.

use crate::api::{self, Client, escape};
use crate::config;
use crate::map;
use anyhow::{Result, anyhow, bail};
use dform_core::ir::Address;
use dform_core::plugin::backend::{self, CallError, Handler, Reply, VERSION};
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

/// The provider's name, its types' namespace (R-36).
pub const PROVIDER: &str = "ovh";
pub const INSTANCE: &str = "ovh.instance";
pub const SSH_KEY: &str = "ovh.ssh_key";
pub const RECORD: &str = "ovh.domain_record";
pub const REGION: &str = "ovh.region";
pub const FLAVOR: &str = "ovh.flavor";
pub const IMAGE: &str = "ovh.image";

/// The data sources, each with its columns' binding (`+` an input).
pub const EXTERNS: [(&str, &[bool]); 3] = [
    (REGION, &[true, false, false]),
    (FLAVOR, &[true, false, false, false, false]),
    (IMAGE, &[true, false, false, false]),
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

/// How often an instance is polled while it is made or deleted:
/// `DFORM_OVH_POLL_MS`, else 5s.
fn poll() -> Duration {
    std::env::var("DFORM_OVH_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(5), Duration::from_millis)
}

/// Says the object's status as the API gives it, and a message.
type Say<'a> = &'a dyn Fn(&str, Option<&str>);

/// How to find what a Create made, by the object's key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Made {
    Instance { name: String, region: String },
    SshKey { name: String },
    Record(RecordKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordKey {
    zone: String,
    subdomain: String,
    typ: String,
    target: String,
}

/// The account a configured provider reaches.
struct Account {
    client: Client,
    /// The project's id (its `serviceName`), when one is configured.
    project: Option<String>,
    /// dform's cache directory, where the ids of projects named by
    /// description are kept ([`resolve_project`]).
    cache: Option<std::path::PathBuf>,
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
    /// The Creates this process was sent, by idempotency key.
    made: Mutex<BTreeMap<String, Made>>,
    /// Each region's flavors and images, as the API listed them.
    lists: Mutex<BTreeMap<String, Json>>,
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

/// A 404 under `/domain/zone/{zone}` when the account does not host the
/// zone itself: says so, and where it is delegated when DNS answers.
fn not_hosted(a: &Account, zone: &str, e: &api::Error) -> Option<String> {
    if !e.is_not_found() {
        return None;
    }
    let path = format!("/domain/zone/{}", escape(zone));
    if !matches!(a.client.get_opt(&path), Ok(None)) {
        return None;
    }
    Some(match crate::dns::nameservers(zone) {
        Some(ns) => format!(
            "zone {zone} is not hosted on this OVH account (its nameservers are {})",
            ns.join(", ")
        ),
        None => format!("zone {zone} is not hosted on this OVH account"),
    })
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

impl Default for Ovh {
    fn default() -> Ovh {
        Ovh::new()
    }
}

impl Ovh {
    pub fn new() -> Ovh {
        Ovh {
            schema: schema().expect("the provider's schema parses"),
            state: RwLock::new(None),
            made: Mutex::new(BTreeMap::new()),
            lists: Mutex::new(BTreeMap::new()),
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
    /// second time). The account's project id, for `expect_account`.
    pub fn configure(&self, config: &Json) -> Result<Option<String>> {
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
            return Ok(None);
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
        let env = |k: &str| std::env::var(k).ok();
        let creds = config::resolve(
            setting("endpoint")?.as_deref(),
            &env,
            &config::default_files(),
        );
        let creds = match (creds, settings) {
            (Ok(c), _) => c,
            // The program names the account: it cannot be reached.
            (Err(e), Some(_)) => return Err(e.context("provider ovh")),
            (Err(e), None) => {
                set(Configured {
                    account: Err(format!("{e:#}")),
                    awaiting: false,
                });
                return Ok(None);
            }
        };
        let project = setting("project")?
            .or_else(|| env("OVH_CLOUD_PROJECT_SERVICE").filter(|v| !v.is_empty()));
        let client = Client::new(creds);
        let cache = config
            .get("cache")
            .and_then(Json::as_str)
            .map(std::path::PathBuf::from);
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
            })),
            awaiting: false,
        });
        Ok(account)
    }

    /// A region's flavors or images (`what`), listed once.
    fn list(&self, a: &Account, p: &str, what: &str, region: &str) -> api::Result<Json> {
        let k = format!("{what}/{region}");
        if let Some(v) = self.lists.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            return Ok(v.clone());
        }
        let v = a.client.get(&format!(
            "/cloud/project/{p}/{what}?region={}",
            escape(region)
        ))?;
        self.lists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, v.clone());
        Ok(v)
    }

    /// A region's flavors and images, both listed at once when either is
    /// not yet: an instance is checked or read against both, and the API
    /// is a round trip away. A failure is left for the one that asks.
    fn list_region(&self, a: &Account, p: &str, region: &str) {
        let lists = self.lists.lock().unwrap_or_else(|e| e.into_inner());
        let missing: Vec<&str> = ["flavor", "image"]
            .into_iter()
            .filter(|w| !lists.contains_key(&format!("{w}/{region}")))
            .collect();
        drop(lists);
        if missing.len() < 2 {
            return;
        }
        std::thread::scope(|scope| {
            for what in missing {
                scope.spawn(move || self.list(a, p, what, region));
            }
        });
    }

    fn flavor_id(&self, a: &Account, p: &str, region: &str, name: &str) -> Result<String> {
        let flavors = self.list(a, p, "flavor", region)?;
        map::flavor_id(&flavors, name).ok_or_else(|| {
            anyhow!(
                "flavor {name:?} is not offered in region {region} (it offers {})",
                map::names(&flavors)
            )
        })
    }

    fn image_id(&self, a: &Account, p: &str, region: &str, name: &str) -> Result<String> {
        let images = self.list(a, p, "image", region)?;
        map::image_id(&images, name).ok_or_else(|| {
            anyhow!(
                "image {name:?} is not in region {region} (it has {})",
                map::names(&images)
            )
        })
    }

    /// A flavor's or an image's name by id, from the region's list, else
    /// asked.
    fn name_of(&self, a: &Account, p: &str, what: &str, region: &str, id: &str) -> Option<String> {
        let by_id = |list: &Json| {
            list.as_array()?
                .iter()
                .find(|x| s(x, "id") == Some(id))
                .and_then(|x| s(x, "name"))
                .map(str::to_string)
        };
        if let Ok(list) = self.list(a, p, what, region)
            && let Some(n) = by_id(&list)
        {
            return Some(n);
        }
        let one = a
            .client
            .get_opt(&format!("/cloud/project/{p}/{what}/{}", escape(id)))
            .ok()??;
        s(&one, "name").map(str::to_string)
    }

    // Read.

    fn read_instance(&self, a: &Account, p: &str, id: &str) -> api::Result<Option<(Json, Json)>> {
        let Some(o) = a
            .client
            .get_opt(&format!("/cloud/project/{p}/instance/{}", escape(id)))?
        else {
            return Ok(None);
        };
        Ok(self.instance_doc(a, p, &o))
    }

    fn instance_doc(&self, a: &Account, p: &str, o: &Json) -> Option<(Json, Json)> {
        if matches!(s(o, "status"), Some("DELETED" | "SOFT_DELETED")) {
            return None;
        }
        let region = s(o, "region").unwrap_or_default();
        if ["flavor", "image"]
            .iter()
            .any(|k| o.get(k).and_then(|x| s(x, "name")).is_none())
        {
            self.list_region(a, p, region);
        }
        let named = |what: &str, embedded: &str, id: &str| {
            o.get(embedded)
                .and_then(|x| s(x, "name"))
                .map(str::to_string)
                .or_else(|| self.name_of(a, p, what, region, s(o, id).unwrap_or_default()))
        };
        let flavor = named("flavor", "flavor", "flavorId");
        let image = named("image", "image", "imageId");
        Some(map::instance(o, flavor.as_deref(), image.as_deref()))
    }

    fn read_record(&self, a: &Account, remote: &str) -> Result<Option<(Json, Json)>> {
        let (zone, id) = remote
            .rsplit_once('/')
            .ok_or_else(|| anyhow!("a record's remote id is ZONE/ID, not {remote:?}"))?;
        Ok(a.client
            .get_opt(&format!(
                "/domain/zone/{}/record/{}",
                escape(zone),
                escape(id)
            ))?
            .map(|o| map::record(&o)))
    }

    pub fn read(&self, typ: &str, remote: &str, name: &str) -> Result<Option<(Json, Json)>> {
        let at = format!("read {}", address(typ, name));
        Ok(match typ {
            INSTANCE => {
                let (a, p) = self.project(&at)?;
                self.read_instance(&a, &p, remote)?
            }
            SSH_KEY => {
                let (a, p) = self.project(&at)?;
                a.client
                    .get_opt(&format!("/cloud/project/{p}/sshkey/{}", escape(remote)))?
                    .map(|o| map::ssh_key(&o))
            }
            RECORD => {
                let a = self.account(&at)?;
                self.read_record(&a, remote)?
            }
            _ => bail!("{at}: the ovh provider has no type {typ}"),
        })
    }

    // Plan.

    /// Validate `desired` and diff it against `prior`; whether a change
    /// replaces the object.
    pub fn plan(
        &self,
        typ: &str,
        name: &str,
        _remote: &str,
        prior: Option<&Json>,
        desired: Option<&Json>,
    ) -> Result<(Vec<provider::Change>, bool)> {
        let at = format!("plan {}", address(typ, name));
        if !self.schema.knows_type(typ) {
            bail!("{at}: the ovh provider has no type {typ}");
        }
        let Some(d) = desired else {
            return Ok((diff(&self.schema, typ, prior, None), false));
        };
        for ((t, p), a) in &self.schema.attrs {
            if t == typ && a.has("required") && d.get(p).is_none() {
                bail!("{at}: required attribute {p} is not set");
            }
        }
        if typ == INSTANCE {
            self.check_instance(&at, d)?;
        }
        // An instance's user data is write-only (R-106): the API never
        // answers it, and dform compares it with the digest state keeps,
        // giving the program's value in `prior` when it is the same.
        let changes = diff(&self.schema, typ, prior, Some(d));
        let replaces = prior.is_some()
            && changes
                .iter()
                .any(|c| self.schema.forces_new(typ, &norm_path(&c.path)));
        Ok((changes, replaces))
    }

    /// An instance's flavor and image are offered in its region, when the
    /// account is reachable and they are known.
    fn check_instance(&self, at: &str, d: &Json) -> Result<()> {
        let Ok((a, p)) = self.project(at) else {
            return Ok(());
        };
        let (Some(region), flavor, image) = (s(d, "region"), s(d, "flavor"), s(d, "image")) else {
            return Ok(());
        };
        if flavor.is_some() && image.is_some() {
            self.list_region(&a, &p, region);
        }
        if let Some(f) = flavor {
            self.flavor_id(&a, &p, region, f)
                .map_err(|e| anyhow!("{at}: flavor: {e:#}"))?;
        }
        if let Some(i) = image {
            self.image_id(&a, &p, region, i)
                .map_err(|e| anyhow!("{at}: image: {e:#}"))?;
        }
        Ok(())
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
            pb::Op::Update | pb::Op::Adopt => self.update(typ, &at, &r.remote, &config)?,
            pb::Op::Delete => {
                self.delete(typ, &at, &r.remote, &mut notes, &say)?;
                return Ok(pb::ApplyResponse {
                    notes,
                    ..Default::default()
                });
            }
            pb::Op::Replace => {
                // The key is the name: the old object goes first unless the
                // replacement has another key.
                let old = self
                    .read(typ, &r.remote, &r.name)
                    .map_err(|e| refused(&at, format!("{e:#}")))?;
                let same = old
                    .as_ref()
                    .is_none_or(|(attrs, _)| self.key_of(typ, attrs) == self.key_of(typ, &config));
                if r.create_first && same {
                    notes.push(format!(
                        "{at}: the replacement has the old object's key, so the old one goes first"
                    ));
                }
                if !r.create_first || same {
                    self.delete(typ, &at, &r.remote, &mut notes, &say)?;
                }
                self.create(typ, &at, &config, key, &mut notes, &say)?
            }
            _ => return Err(refused(&at, "no operation")),
        };
        Ok(pb::ApplyResponse {
            remote,
            attrs: Some(wire::doc(&attrs)),
            computed: Some(wire::doc(&computed)),
            elapsed_ms: 0,
            notes,
        })
    }

    /// What names an object of `typ` uniquely, from its document.
    fn key_of(&self, typ: &str, doc: &Json) -> Option<Made> {
        let st = |k: &str| s(doc, k).map(str::to_string);
        Some(match typ {
            INSTANCE => Made::Instance {
                name: st("name")?,
                region: st("region")?,
            },
            SSH_KEY => Made::SshKey { name: st("name")? },
            RECORD => Made::Record(RecordKey {
                zone: st("zone")?,
                subdomain: st("subdomain").unwrap_or_default(),
                typ: st("type")?,
                target: st("target")?,
            }),
            _ => return None,
        })
    }

    /// The remote id of the object `made` names, if it exists.
    fn find(&self, made: &Made) -> Result<Option<String>> {
        Ok(match made {
            Made::Instance { name, region } => {
                let (a, p) = self.project("find an instance")?;
                let list = a.client.get(&format!(
                    "/cloud/project/{p}/instance?region={}",
                    escape(region)
                ))?;
                list.as_array()
                    .into_iter()
                    .flatten()
                    .filter(|o| !matches!(s(o, "status"), Some("DELETED" | "SOFT_DELETED")))
                    .find(|o| s(o, "name") == Some(name) && s(o, "region") == Some(region))
                    .and_then(|o| s(o, "id"))
                    .map(str::to_string)
            }
            Made::SshKey { name } => {
                let (a, p) = self.project("find an SSH key")?;
                let list = a.client.get(&format!("/cloud/project/{p}/sshkey"))?;
                list.as_array()
                    .into_iter()
                    .flatten()
                    .find(|o| s(o, "name") == Some(name))
                    .and_then(|o| s(o, "id"))
                    .map(str::to_string)
            }
            Made::Record(k) => {
                let a = self.account("find a DNS record")?;
                let zone = escape(&k.zone);
                let ids = a
                    .client
                    .get(&format!(
                        "/domain/zone/{zone}/record?fieldType={}&subDomain={}",
                        escape(&k.typ),
                        escape(&k.subdomain)
                    ))
                    .map_err(|e| match not_hosted(&a, &k.zone, &e) {
                        Some(m) => anyhow!(m),
                        None => e.into(),
                    })?;
                let mut found = None;
                for id in ids
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_i64)
                {
                    let Some(o) = a
                        .client
                        .get_opt(&format!("/domain/zone/{zone}/record/{id}"))?
                    else {
                        continue;
                    };
                    if s(&o, "target") == Some(k.target.as_str()) {
                        found = Some(map::record_remote(&k.zone, id));
                        break;
                    }
                }
                found
            }
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
        let made = self
            .key_of(typ, config)
            .ok_or_else(|| refused(at, "its key (name, or zone, type and target) is not known"))?;
        // An object of the same key: this process's own (the same Create
        // sent again) is the answer; another is not taken over.
        let ours = !key.is_empty()
            && self.made.lock().unwrap_or_else(|e| e.into_inner()).get(key) == Some(&made);
        if let Some(remote) = self
            .find(&made)
            .map_err(|e| refused(at, format!("{e:#}")))?
        {
            if ours {
                let (attrs, computed) = self.read_made(typ, at, &remote)?;
                return Ok((remote, attrs, computed));
            }
            return Err(refused(
                at,
                format!(
                    "{typ} {remote} already exists with this key ({made:?}); adopt it or name another"
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
            SSH_KEY => {
                let (a, p) = self
                    .project(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let body = json!({
                    "name": need(at, config, "name")?,
                    "publicKey": need(at, config, "public_key")?,
                });
                let o = a
                    .client
                    .post(&format!("/cloud/project/{p}/sshkey"), &body)
                    .map_err(|e| failed(at, e))?;
                let (attrs, computed) = map::ssh_key(&o);
                Ok((s(&o, "id").unwrap_or_default().to_string(), attrs, computed))
            }
            RECORD => {
                let a = self
                    .account(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let zone = need(at, config, "zone")?;
                let mut body = json!({
                    "fieldType": need(at, config, "type")?,
                    "subDomain": s(config, "subdomain").unwrap_or_default(),
                    "target": need(at, config, "target")?,
                });
                if let Some(ttl) = config.get("ttl").and_then(Json::as_i64) {
                    body["ttl"] = json!(ttl);
                }
                let o = a
                    .client
                    .post(&format!("/domain/zone/{}/record", escape(zone)), &body)
                    .map_err(|e| match not_hosted(&a, zone, &e) {
                        Some(m) => refused(at, m),
                        None => failed(at, e),
                    })?;
                self.refresh_zone(&a, at, zone, notes);
                let (attrs, computed) = map::record(&o);
                let id = o.get("id").and_then(Json::as_i64).unwrap_or(0);
                Ok((map::record_remote(zone, id), attrs, computed))
            }
            _ => Err(refused(at, format!("the ovh provider has no type {typ}"))),
        }
    }

    /// The object a Create made, read.
    fn read_made(
        &self,
        typ: &str,
        at: &str,
        remote: &str,
    ) -> std::result::Result<(Json, Json), Failed> {
        self.read(typ, remote, "")
            .map_err(|e| refused(at, format!("{e:#}")))?
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: {typ} {remote} is gone again")))
    }

    fn create_instance(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self
            .project(at)
            .map_err(|e| refused(at, format!("{e:#}")))?;
        let region = need(at, config, "region")?;
        let flavor = need(at, config, "flavor")?;
        let image = need(at, config, "image")?;
        let mut body = json!({
            "name": need(at, config, "name")?,
            "region": region,
            "flavorId": self.flavor_id(&a, &p, region, flavor).map_err(|e| refused(at, format!("{e:#}")))?,
            "imageId": self.image_id(&a, &p, region, image).map_err(|e| refused(at, format!("{e:#}")))?,
            "monthlyBilling": false,
        });
        if config.get("ssh_key").is_some() {
            body["sshKeyId"] = json!(need(at, config, "ssh_key")?);
        }
        let user_data = match config.get("user_data") {
            None => None,
            Some(_) => Some(need(at, config, "user_data")?),
        };
        if let Some(u) = user_data {
            body["userData"] = json!(u);
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/instance"), &body)
            .map_err(|e| failed(at, e))?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        // Wait for it to run: until then it has no address. Each status
        // the API gives that is not the last one's is said (`BUILD`,
        // `ACTIVE`).
        let start = Instant::now();
        let status = |o: &Json| s(o, "status").unwrap_or("unknown").to_string();
        let mut said = status(&o);
        say(&said, None);
        let mut last = o;
        while s(&last, "status") != Some("ACTIVE") {
            if s(&last, "status") == Some("ERROR") {
                notes.push(format!(
                    "{at}: instance {id} is in ERROR; it is kept in state, to be replaced or \
                     deleted"
                ));
                break;
            }
            if start.elapsed() > CREATE_WAIT {
                notes.push(format!(
                    "{at}: instance {id} is still {} after {}s",
                    s(&last, "status").unwrap_or("unknown"),
                    CREATE_WAIT.as_secs()
                ));
                break;
            }
            std::thread::sleep(poll());
            match a
                .client
                .get_opt(&format!("/cloud/project/{p}/instance/{}", escape(&id)))
            {
                Ok(Some(o)) => {
                    if status(&o) != said {
                        said = status(&o);
                        say(&said, None);
                    }
                    last = o;
                }
                Ok(None) => {
                    return Err(Failed::MaybeApplied(format!(
                        "{at}: instance {id} went away while it was made"
                    )));
                }
                // A failed poll is no answer about the instance: ask again.
                Err(e) => say(&said, Some(&format!("a poll failed, asking again: {e:#}"))),
            }
        }
        let (attrs, computed) = self
            .instance_doc(&a, &p, &last)
            .ok_or_else(|| Failed::MaybeApplied(format!("{at}: instance {id} was deleted")))?;
        Ok((id, attrs, computed))
    }

    fn update(
        &self,
        typ: &str,
        at: &str,
        remote: &str,
        config: &Json,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let now = self
            .read(typ, remote, "")
            .map_err(|e| refused(at, format!("{e:#}")))?
            .ok_or_else(|| refused(at, format!("{typ} {remote} is not there")))?;
        match typ {
            INSTANCE => {
                let (a, p) = self
                    .project(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let name = need(at, config, "name")?;
                if s(&now.0, "name") != Some(name) {
                    a.client
                        .put(
                            &format!("/cloud/project/{p}/instance/{}", escape(remote)),
                            &json!({"instanceName": name}),
                        )
                        .map_err(|e| failed(at, e))?;
                }
            }
            RECORD => {
                let a = self
                    .account(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let (zone, id) = remote.rsplit_once('/').unwrap_or_default();
                let ttl = config.get("ttl").and_then(Json::as_i64).unwrap_or(0);
                let was = now.1.get("ttl").and_then(Json::as_i64).unwrap_or(0);
                if ttl != was {
                    let body = json!({
                        "subDomain": s(config, "subdomain").unwrap_or_default(),
                        "target": need(at, config, "target")?,
                        "ttl": ttl,
                    });
                    a.client
                        .put(
                            &format!("/domain/zone/{}/record/{}", escape(zone), escape(id)),
                            &body,
                        )
                        .map_err(|e| failed(at, e))?;
                    self.refresh_zone(&a, at, zone, &mut Vec::new());
                }
            }
            // Nothing of an SSH key changes in place (the schema replaces it).
            _ => {}
        }
        let (attrs, computed) = self.read_made(typ, at, remote)?;
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
        let gone_already = |e: &api::Error| e.is_not_found();
        match typ {
            INSTANCE => {
                let (a, p) = self
                    .project(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let path = format!("/cloud/project/{p}/instance/{}", escape(remote));
                match a.client.delete(&path) {
                    Ok(_) => {}
                    Err(e) if gone_already(&e) => {}
                    Err(e) => return Err(failed(at, e)),
                }
                let start = Instant::now();
                let mut said = None;
                loop {
                    let got = a.client.get_opt(&path);
                    if let Ok(Some(o)) = &got
                        && let Some(now) = s(o, "status")
                        && said.as_deref() != Some(now)
                    {
                        say(now, None);
                        said = Some(now.to_string());
                    }
                    match got {
                        Ok(None) => break,
                        Ok(Some(o)) if matches!(s(&o, "status"), Some("DELETED")) => break,
                        _ if start.elapsed() > DELETE_WAIT => {
                            notes.push(format!(
                                "{at}: instance {remote} is still being deleted after {}s",
                                DELETE_WAIT.as_secs()
                            ));
                            break;
                        }
                        _ => std::thread::sleep(poll()),
                    }
                }
            }
            SSH_KEY => {
                let (a, p) = self
                    .project(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                match a
                    .client
                    .delete(&format!("/cloud/project/{p}/sshkey/{}", escape(remote)))
                {
                    Ok(_) => {}
                    Err(e) if gone_already(&e) => {}
                    Err(e) => return Err(failed(at, e)),
                }
            }
            RECORD => {
                let a = self
                    .account(at)
                    .map_err(|e| refused(at, format!("{e:#}")))?;
                let (zone, id) = remote.rsplit_once('/').unwrap_or_default();
                match a.client.delete(&format!(
                    "/domain/zone/{}/record/{}",
                    escape(zone),
                    escape(id)
                )) {
                    Ok(_) => {}
                    Err(e) if gone_already(&e) => {}
                    Err(e) => return Err(failed(at, e)),
                }
                self.refresh_zone(&a, at, zone, notes);
            }
            _ => return Err(refused(at, format!("the ovh provider has no type {typ}"))),
        }
        Ok(())
    }

    /// A zone's changes are served once it is refreshed; a refresh that
    /// fails is a note, the record is written.
    fn refresh_zone(&self, a: &Account, at: &str, zone: &str, notes: &mut Vec<String>) {
        if let Err(e) = a.client.post(
            &format!("/domain/zone/{}/refresh", escape(zone)),
            &json!({}),
        ) {
            notes.push(format!("{at}: the zone {zone} is not refreshed: {e}"));
        }
    }

    // Query.

    pub fn query(&self, pred: &str, plus: &[bool], inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        if pred == CREATED {
            let [Value::Str(typ), name, Value::Str(key)] = inputs else {
                bail!("{CREATED} is asked with Type, Name and Key bound");
            };
            if key.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(self
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
                .collect());
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
            let ins = dform_core::partition::fmt_bare(&inputs[0]);
            return Ok(vec![
                binding
                    .iter()
                    .enumerate()
                    .map(|(c, plus)| match plus {
                        true => Value::Str(input.clone()),
                        false => Value::Null {
                            label: dform_core::value::null_label(pred, &ins, &(c + 1).to_string()),
                            class: dform_core::value::NullClass::Open,
                            ty: String::new(),
                        },
                    })
                    .collect(),
            ]);
        }
        let what = format!("answer {pred}({input:?}, ..)");
        Ok(match pred {
            REGION => {
                let a = self.account(&what)?;
                let p = resolve_project(&a.client, input, a.cache.as_deref())?;
                let names = a.client.get(&format!("/cloud/project/{p}/region"))?;
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
                        Value::Str(input.clone()),
                        Value::Str(n.to_string()),
                        Value::Str(s(&r, "status").unwrap_or_default().to_string()),
                    ]);
                }
                rows
            }
            FLAVOR => {
                let (a, p) = self.project(&what)?;
                map::flavor_rows(input, &self.list(&a, &p, "flavor", input)?)
            }
            _ => {
                let (a, p) = self.project(&what)?;
                map::image_rows(input, &self.list(&a, &p, "image", input)?)
            }
        })
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
    let file = cache.map(|c| c.join("ovh-projects.json"));
    let mut kept: BTreeMap<String, String> = file
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
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
        kept.insert(key, id.clone());
        if let Ok(bytes) = serde_json::to_vec_pretty(&kept) {
            let _ = f.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(f, bytes);
        }
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
    let projects: Vec<api::Result<Json>> = std::thread::scope(|scope| {
        let asks: Vec<_> = ids
            .iter()
            .map(|id| scope.spawn(move || client.get(&format!("/cloud/project/{}", escape(id)))))
            .collect();
        asks.into_iter()
            .map(|h| h.join().expect("a project's GET does not panic"))
            .collect()
    });
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

fn invalid(e: anyhow::Error) -> CallError {
    CallError::Refused(format!("{e:#}"))
}

fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, CallError> {
    v.map(wire::from_doc).transpose().map_err(invalid)
}

impl Handler for Ovh {
    fn handle(
        &self,
        call: backend::Call,
        progress: backend::Progress,
    ) -> std::result::Result<Reply, CallError> {
        use backend::Call as C;
        Ok(match call {
            C::Handshake(req) => {
                let v = req.protocol_version;
                if v != VERSION {
                    return Err(CallError::Refused(format!(
                        "this provider speaks protocol version {VERSION}, not {v}"
                    )));
                }
                Reply::Handshake(pb::HandshakeResponse {
                    protocol_version: VERSION,
                    name: PROVIDER.into(),
                    capabilities: ["resource", "managed"].map(String::from).to_vec(),
                    version: backend::BUILD.into(),
                })
            }
            C::Configure(req) => {
                let config = doc_of(req.config.as_ref())?.unwrap_or(json!({}));
                let account = self.configure(&config).map_err(invalid)?;
                Reply::Configure(pb::ConfigureResponse { account })
            }
            C::Schema(req) => Reply::Schema(pb::SchemaResponse {
                facts: wire::schema_facts(&self.schema, &req).map_err(invalid)?,
                externs: EXTERNS
                    .iter()
                    .map(|(pred, input)| pb::ExternDecl {
                        pred: pred.to_string(),
                        arity: input.len() as u32,
                        input: input.to_vec(),
                    })
                    .collect(),
                checks_refinements: false,
                examples: self.examples(),
            }),
            C::Query(q) => {
                let inputs = q
                    .inputs
                    .iter()
                    .map(wire::from_value)
                    .collect::<Result<Vec<_>>>()
                    .map_err(invalid)?;
                let rows = self.query(&q.pred, &q.input, &inputs).map_err(invalid)?;
                Reply::Query(
                    rows.iter()
                        .map(|row| pb::Row {
                            values: row.iter().map(wire::value).collect(),
                        })
                        .collect(),
                )
            }
            C::Read(r) => Reply::Read(
                match self.read(&r.r#type, &r.remote, &r.name).map_err(invalid)? {
                    Some((attrs, computed)) => pb::ReadResponse {
                        found: true,
                        attrs: Some(wire::doc(&attrs)),
                        computed: Some(wire::doc(&computed)),
                    },
                    None => pb::ReadResponse::default(),
                },
            ),
            C::Plan(r) => {
                let prior = doc_of(r.prior.as_ref())?;
                let desired = doc_of(r.desired.as_ref())?;
                let (changes, requires_replace) = self
                    .plan(
                        &r.r#type,
                        &r.name,
                        &r.remote,
                        prior.as_ref(),
                        desired.as_ref(),
                    )
                    .map_err(invalid)?;
                Reply::Plan(pb::PlanResponse {
                    changes: changes
                        .iter()
                        .map(|c| pb::Change {
                            path: c.path.clone(),
                            before: c.before.as_ref().map(wire::doc),
                            after: c.after.as_ref().map(wire::doc),
                            sensitive: c.sensitive,
                        })
                        .collect(),
                    requires_replace,
                })
            }
            C::Apply(r) => Reply::Apply(self.apply(&r, progress)?),
            C::Import(r) => Reply::Import(
                match self.read(&r.r#type, &r.remote, "").map_err(invalid)? {
                    Some((attrs, computed)) => pb::ImportResponse {
                        found: true,
                        name: s(&attrs, "name").unwrap_or(&r.remote).to_string(),
                        r#type: r.r#type,
                        attrs: Some(wire::doc(&attrs)),
                        computed: Some(wire::doc(&computed)),
                    },
                    None => pb::ImportResponse::default(),
                },
            ),
            // No type of this provider has a sensitive attribute.
            C::Reveal(r) => {
                let h = r.held.unwrap_or_default();
                return Err(CallError::Refused(format!(
                    "reveal {} {}#{}: the ovh provider holds no secret",
                    h.r#type, h.remote, h.path
                )));
            }
        })
    }
}
