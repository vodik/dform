//! The provider: the plugin protocol (`Handler`) over the OVH API.
//!
//! Configure finds the credentials (`config`), and with the program's
//! settings (`provider ovh { endpoint, project }`, a second Configure) the
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
//! Apply refuses every change for now: Create, Update and Delete are
//! R-44's second half.

use crate::api::{self, Client, escape};
use crate::config;
use crate::map;
use crate::record::{self, Record};
use anyhow::{Result, anyhow, bail};
use dform_core::ir::Address;
use dform_core::plugin::backend::{self, CallError, Handler, Reply, VERSION};
use dform_core::plugin::pb;
use dform_core::plugin::wire;
use dform_core::provider::{self, diff, norm_path};
use dform_core::schema::Schema;
use dform_core::value::Value;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

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

/// The account a configured provider reaches.
struct Account {
    client: Client,
    /// The project's id (its `serviceName`), when one is configured.
    project: Option<String>,
}

/// A configured provider.
struct Configured {
    account: std::result::Result<Arc<Account>, String>,
    record: Arc<Record>,
    /// The program's settings are still to come (a `provider ovh { .. }`
    /// block): a data source answers "not yet".
    awaiting: bool,
}

pub struct Ovh {
    schema: Schema,
    state: RwLock<Option<Arc<Configured>>>,
    /// Each region's flavors and images, as the API listed them.
    lists: Mutex<BTreeMap<String, Json>>,
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
                "{what}: no project: name it in the program (`provider ovh {{ project = \"..\" }}`) \
                 or in OVH_CLOUD_PROJECT_SERVICE"
            )
        })?;
        Ok((a, p))
    }

    fn record(&self) -> Arc<Record> {
        self.configured()
            .map(|c| c.record.clone())
            .unwrap_or_else(|_| Arc::new(Record::new(None)))
    }

    /// Configure from `config` (dform's, with the program's `settings` the
    /// second time). The account's project id, for `expect_account`.
    pub fn configure(&self, config: &Json) -> Result<Option<String>> {
        let cache = s(config, "cache").map(PathBuf::from).or_else(|| {
            s(config, "world").and_then(|w| std::path::Path::new(w).parent().map(PathBuf::from))
        });
        let record = Arc::new(Record::new(cache.as_deref()));
        let settings = config.get("settings").filter(|v| v.is_object());
        let deferred = config.get("deferred") == Some(&Json::Bool(true));
        let set = |c: Configured| {
            *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
        };
        if settings.is_none() && deferred {
            set(Configured {
                account: Err(
                    "the program configures it (`provider ovh { .. }`) and has not yet".into(),
                ),
                record,
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
                    record,
                    awaiting: false,
                });
                return Ok(None);
            }
        };
        let project = setting("project")?
            .or_else(|| env("OVH_CLOUD_PROJECT_SERVICE").filter(|v| !v.is_empty()));
        let client = Client::new(creds);
        let project = match project {
            Some(p) => Some(resolve_project(&client, &p)?),
            None => None,
        };
        let account = project.clone();
        set(Configured {
            account: Ok(Arc::new(Account { client, project })),
            record,
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
        remote: &str,
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
        let mut changes = diff(&self.schema, typ, prior, Some(d));
        if typ == INSTANCE
            && let Some(prior) = prior
            && prior.get("user_data").is_none()
        {
            // The API never answers user data: compare it with what this
            // provider sent (`record`); with nothing kept, no change.
            let kept = self.record().get(remote);
            let now = d.get("user_data").map(record::digest);
            if kept.is_none() || kept == now {
                changes.retain(|c| norm_path(&c.path) != "user_data");
            }
        }
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

    // Apply: Create, Update and Delete are the second half of R-44.

    fn apply(&self, r: &pb::ApplyRequest) -> std::result::Result<pb::ApplyResponse, CallError> {
        let op = pb::Op::try_from(r.op).unwrap_or(pb::Op::Unspecified);
        if op == pb::Op::EndTick {
            return Ok(pb::ApplyResponse::default());
        }
        Err(CallError::Refused(format!(
            "apply {}: the ovh provider reads and plans; it does not create, change or \
             delete yet",
            address(&r.r#type, &r.name)
        )))
    }

    // Query.

    pub fn query(&self, pred: &str, plus: &[bool], inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
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
                let p = resolve_project(&a.client, input)?;
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
}

/// The project `given` names: its id (`serviceName`), or the description
/// or name it has in the console.
pub fn resolve_project(client: &Client, given: &str) -> Result<String> {
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
    let mut seen = Vec::new();
    for id in &ids {
        let p = client.get(&format!("/cloud/project/{}", escape(id)))?;
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
    fn handle(&self, call: backend::Call) -> std::result::Result<Reply, CallError> {
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
                    capabilities: ["resource"].map(String::from).to_vec(),
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
                examples: Vec::new(),
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
            C::Apply(r) => Reply::Apply(self.apply(&r)?),
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
        })
    }
}
