//! A provider from Rust types (R-25): each resource type a struct with
//! `#[derive(Resource)]` (its schema facts) and [`Lifecycle`] (read,
//! create, update, delete), the provider a [`Provider`] (its name and
//! Configure). [`Typed`] is the [`Handler`] they make: the schema is the
//! derives', Plan is derived from it (the leaf diff, a `force_new` change
//! replaces, a `required` attribute missing refuses), Apply dispatches to
//! the lifecycle, Import is a read. An attribute the schema marks
//! `computed` is returned as computed; the rest as configured.
//!
//! Create, update and delete are told a [`Progress`]: each time the object's
//! status as the API gives it changes (a poll that saw `BUILD` turn
//! `ACTIVE`), say so, and dform prints it beside the change (R-130).
//!
//! An error from a lifecycle function refuses the call; one the host said
//! may have taken effect (a timeout) is `MaybeApplied`, so dform looks
//! before it sends it again; a transient one says `retryable:`, so dform's
//! retry policy sends it again (R-81).

use dform_core::ir::Address;
use dform_core::plugin::backend::{self, Call, CallError, Handler, Reply, VERSION};
use dform_core::plugin::{pb, wire};
use dform_core::schema::Schema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// A resource type's schema, from `#[derive(Resource)]`.
pub trait Resource: Serialize + DeserializeOwned {
    /// `provider.type`.
    const TYPE: &'static str;
    /// Its `type_attr` (and `type_list_key`, `type_replace`, `type_retry`,
    /// `type_lookup`) facts, as `.df` text.
    const FACTS: &'static str;
    /// Whether the provider answers Health for it (`#[dform(health)]`,
    /// R-203): the handshake lists it, and `dform status` asks
    /// [`Lifecycle::health`].
    const HEALTH: bool = false;
}

/// How a call failed, as a lifecycle function says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Nothing changed.
    Refused(String),
    /// It may have taken effect.
    MaybeApplied(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Refused(m) | Error::MaybeApplied(m) => f.write_str(m),
        }
    }
}

impl Error {
    /// The message, after `at` (`plan postgres.role["x"]`).
    pub fn at(self, at: &str) -> Error {
        match self {
            Error::Refused(m) => Error::Refused(format!("{at}: {m}")),
            Error::MaybeApplied(m) => Error::MaybeApplied(format!("{at}: {m}")),
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Error {
        Error::Refused(format!("{e:#}"))
    }
}

impl From<crate::Error> for Error {
    fn from(e: crate::Error) -> Error {
        match e.class {
            crate::Class::MaybeApplied => Error::MaybeApplied(e.message),
            crate::Class::Retryable | crate::Class::Final => Error::Refused(e.to_string()),
        }
    }
}

impl From<crate::Failure> for Error {
    fn from(f: crate::Failure) -> Error {
        match f {
            // The engine waits on what the world has not reached yet: a
            // transient refusal, sent again with backoff.
            crate::Failure::NotYet(m) => Error::Refused(format!("retryable: not yet: {m}")),
            crate::Failure::Error(e) => e.into(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::Refused(e.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A provider: its name and how it is configured.
pub trait Provider: Sized + Send + 'static {
    /// The name its handshake gives: what state records.
    const NAME: &'static str;
    /// Its build, for the handshake.
    const VERSION: &'static str = "0.0.0";
    /// Whether each [`Lifecycle::update`] leaves an attribute absent from
    /// `desired` as the object has it (it sends only what is there): then
    /// the provider has the `keep` capability, and an Apply update's
    /// `keep` paths (R-164: a write-only secret a run without the
    /// deployment's master proved unchanged) are filled from what Read
    /// answers, or left absent where it answers nothing (a write-only
    /// one). Without it dform sends no `keep`, and such an update needs
    /// the master.
    const KEEP: bool = false;
    /// Configure from the provider block's settings (a program's
    /// `use NAME { .. }`, a secret among them revealed; `{}` when the
    /// program configures it with none, the engine's own keys never
    /// among them): the provider, and the account its credentials reach
    /// when it can tell.
    fn configure(settings: &Json) -> Result<(Self, Option<String>)>;

    /// The location schemes it reads (R-153), which its manifest declares
    /// and dform routes to [`Provider::read`]: `&["vault"]`.
    const SCHEMES: &'static [&'static str] = &[];

    /// The document at `location`, of a scheme it declares, with the
    /// version the source names it by when it keeps versions (R-172); not
    /// there yet is `Failure::NotYet`. Called once the provider is
    /// configured.
    fn read(&self, location: &str) -> std::result::Result<crate::Document, crate::Failure> {
        Err(crate::Error::fatal(format!(
            "{location}: provider {} reads no location",
            Self::NAME
        ))
        .into())
    }

    /// The rows of the data source `pred` (declared with
    /// [`Typed::facts_text`]'s `extern_decl`) for its `+` columns'
    /// `inputs`, in order: each a full row, every column.
    fn query(&self, pred: &str, inputs: &[Json]) -> Result<Vec<Vec<Json>>> {
        let _ = inputs;
        Err(Error::Refused(format!(
            "provider {} answers no extern {pred}",
            Self::NAME
        )))
    }

    /// The bytes of a sensitive computed value it holds (`held`: its type,
    /// remote id and path), for the engine to reveal into the one call
    /// that takes it (R-45). Asked only with the deployment's lease.
    fn reveal(&self, held: &pb::Held) -> Result<Vec<u8>> {
        Err(Error::Refused(format!(
            "reveal {} {}#{}: provider {} holds no secret",
            held.r#type,
            held.remote,
            held.path,
            Self::NAME
        )))
    }
}

/// Where an Apply says how it goes (R-130), as its object's status
/// changes: a provider says so each time it sees a new one (a poll), never
/// on a timer; dform adds the time it has run.
pub struct Progress<'a> {
    address: String,
    sink: backend::Progress<'a>,
    notes: Mutex<Vec<String>>,
}

impl<'a> Progress<'a> {
    /// The progress of the object at `address` (`type["name"]`), told to
    /// `sink`.
    pub fn new(address: impl Into<String>, sink: backend::Progress<'a>) -> Progress<'a> {
        Progress {
            address: address.into(),
            sink,
            notes: Mutex::new(Vec::new()),
        }
    }

    /// What the user should see of the call once it is done (a delete
    /// that wrote a default rather than removing anything): the Apply's
    /// `notes`, printed under the change.
    pub fn note(&self, note: &str) {
        self.notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(note.to_string());
    }

    fn into_notes(self) -> Vec<String> {
        self.notes.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    /// The object's status, as the API says it (`BUILD`): dform prints it
    /// beside the change as it is.
    pub fn status(&self, status: &str) {
        (self.sink)(backend::event(&self.address, Some(status), None));
    }

    /// Something for the log (a retry, and why).
    pub fn message(&self, message: &str) {
        (self.sink)(backend::event(&self.address, None, Some(message)));
    }
}

/// One resource type's lifecycle.
pub trait Lifecycle<P: Provider>: Resource {
    /// Why a program cannot create one, when the API makes them itself (a
    /// device joins a tailnet; it is not posted): Plan refuses a create
    /// with it, naming the address, and a program adopts them instead.
    const NOT_CREATED: Option<&'static str> = None;
    /// The object `remote`, if it exists.
    fn read(p: &P, remote: &str) -> Result<Option<Self>>;
    /// The remote id of the object a program adopts by `given`
    /// (`adopt(r, "web-1")`): what state keeps and every later call is
    /// sent. By default `given` is the id; a type adopted by a name that
    /// is not its id finds the one object of that name here, and refuses
    /// a name two objects have.
    fn adopt(p: &P, given: &str) -> Result<String> {
        let _ = p;
        Ok(given.to_string())
    }
    /// Create it; `key` is unique to this intended creation (pass it to
    /// an API that takes a client token). Its remote id and what it is.
    fn create(p: &P, desired: Self, key: &str, progress: &Progress) -> Result<(String, Self)>;
    /// Change `remote` from `prior` to `desired`.
    fn update(p: &P, remote: &str, prior: Self, desired: Self, progress: &Progress)
    -> Result<Self>;
    fn delete(p: &P, remote: &str, progress: &Progress) -> Result<()>;
    /// Refuse a desired document the provider cannot apply as configured
    /// (a role it connects as, to be rotated), before anything changes:
    /// called by Plan once the provider is configured, and by Apply before
    /// a create, an update or a replace. `desired` is the protocol's
    /// document: a value not known yet is its marker (`{"$null": ..}`).
    fn check(p: &P, desired: &Json) -> Result<()> {
        let _ = (p, desired);
        Ok(())
    }
    /// The object `remote`'s health as of now (R-203), for `dform status`
    /// and nothing else: asked only of a type that declares it
    /// (`#[dform(health)]`). Judge it from what the API says now (a
    /// provider that must poll to answer polls here); never measure time:
    /// degraded is the API having given up, and what may recover by itself
    /// is progressing. `None` answers `unknown`.
    fn health(p: &P, remote: &str) -> Result<Option<pb::Health>> {
        let _ = (p, remote);
        Ok(None)
    }
}

/// An object as the protocol has it: configured and computed.
type Object = (Json, Json);

/// One resource type, its type erased.
trait Kind<P>: Send + Sync {
    fn not_created(&self) -> Option<&'static str>;
    fn read(&self, p: &P, remote: &str) -> Result<Option<Json>>;
    fn adopt(&self, p: &P, given: &str) -> Result<String>;
    fn create(
        &self,
        p: &P,
        desired: Json,
        key: &str,
        progress: &Progress,
    ) -> Result<(String, Json)>;
    fn update(
        &self,
        p: &P,
        remote: &str,
        prior: Json,
        desired: Json,
        progress: &Progress,
    ) -> Result<Json>;
    fn delete(&self, p: &P, remote: &str, progress: &Progress) -> Result<()>;
    fn check(&self, p: &P, desired: &Json) -> Result<()>;
    fn answers_health(&self) -> bool;
    fn health(&self, p: &P, remote: &str) -> Result<Option<pb::Health>>;
}

struct K<R>(std::marker::PhantomData<fn() -> R>);

fn to_json<R: Serialize>(r: &R) -> Result<Json> {
    Ok(serde_json::to_value(r)?)
}

fn from_json<R: DeserializeOwned>(typ: &str, j: Json) -> Result<R> {
    serde_json::from_value(j).map_err(|e| Error::Refused(format!("{typ}: {e}")))
}

impl<P: Provider, R: Lifecycle<P> + 'static> Kind<P> for K<R> {
    fn not_created(&self) -> Option<&'static str> {
        R::NOT_CREATED
    }

    fn read(&self, p: &P, remote: &str) -> Result<Option<Json>> {
        R::read(p, remote)?.map(|r| to_json(&r)).transpose()
    }

    fn adopt(&self, p: &P, given: &str) -> Result<String> {
        R::adopt(p, given)
    }

    fn create(
        &self,
        p: &P,
        desired: Json,
        key: &str,
        progress: &Progress,
    ) -> Result<(String, Json)> {
        let (remote, r) = R::create(p, from_json(R::TYPE, desired)?, key, progress)?;
        Ok((remote, to_json(&r)?))
    }

    fn update(
        &self,
        p: &P,
        remote: &str,
        prior: Json,
        desired: Json,
        progress: &Progress,
    ) -> Result<Json> {
        let r = R::update(
            p,
            remote,
            from_json(R::TYPE, prior)?,
            from_json(R::TYPE, desired)?,
            progress,
        )?;
        to_json(&r)
    }

    fn delete(&self, p: &P, remote: &str, progress: &Progress) -> Result<()> {
        R::delete(p, remote, progress)
    }

    fn check(&self, p: &P, desired: &Json) -> Result<()> {
        R::check(p, desired)
    }

    fn answers_health(&self) -> bool {
        R::HEALTH
    }

    fn health(&self, p: &P, remote: &str) -> Result<Option<pb::Health>> {
        R::health(p, remote)
    }
}

/// The [`Handler`] a typed provider is.
pub struct Typed<P: Provider> {
    kinds: BTreeMap<&'static str, Box<dyn Kind<P>>>,
    facts: String,
    schema: Schema,
    examples: Vec<pb::Example>,
    provider: Mutex<Option<P>>,
}

impl<P: Provider> Default for Typed<P> {
    fn default() -> Typed<P> {
        Typed::new()
    }
}

impl<P: Provider> Typed<P> {
    pub fn new() -> Typed<P> {
        Typed {
            kinds: BTreeMap::new(),
            facts: String::new(),
            schema: Schema::default(),
            examples: Vec::new(),
            provider: Mutex::new(None),
        }
    }

    /// Serve the resource type `R`.
    pub fn resource<R: Lifecycle<P> + 'static>(mut self) -> Typed<P> {
        self.kinds
            .insert(R::TYPE, Box::new(K::<R>(std::marker::PhantomData)));
        self.facts.push_str(&format!(
            "type_provider({:?}, {:?})\n{}\n",
            R::TYPE,
            P::NAME,
            R::FACTS
        ));
        self.schema = Schema::parse(&self.facts, P::NAME)
            .unwrap_or_else(|e| panic!("the derived schema of {} does not parse: {e:#}", R::TYPE));
        self
    }

    /// Schema facts beside the derives', as `.df` text: a data source's
    /// `extern_decl(Pred, "+in, -out")` ([`Provider::query`] answers it)
    /// and the settings a `use` block gives (`provider_setting`).
    pub fn facts_text(mut self, facts: &str) -> Typed<P> {
        self.facts.push_str(facts);
        self.facts.push('\n');
        self.schema = Schema::parse(&self.facts, P::NAME)
            .unwrap_or_else(|e| panic!("the schema facts of {} do not parse: {e:#}", P::NAME));
        self
    }

    /// A document of `R` for `dform provider check` (its Schema's
    /// `examples`): `create` makes one, `update` is it changed in place,
    /// and `required` a path `create` sets that Plan refuses it without
    /// (empty for none). The first example is the object the Apply checks
    /// create, update, replace and delete; each Plan check takes the first
    /// that has what it checks.
    pub fn example<R: Resource>(mut self, create: Json, update: Json, required: &str) -> Typed<P> {
        self.examples.push(pb::Example {
            r#type: R::TYPE.to_string(),
            create: Some(wire::doc(&create)),
            update: Some(wire::doc(&update)),
            required: required.to_string(),
        });
        self
    }

    /// The schema the derives give, as `.df` facts.
    pub fn facts(&self) -> &str {
        &self.facts
    }

    fn kind(&self, typ: &str) -> Result<&dyn Kind<P>> {
        self.kinds
            .get(typ)
            .map(|k| k.as_ref())
            .ok_or_else(|| Error::Refused(format!("provider {} serves no type {typ}", P::NAME)))
    }

    fn with<T>(&self, f: impl FnOnce(&P) -> Result<T>) -> Result<T> {
        let p = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        match p.as_ref() {
            Some(p) => f(p),
            None => Err(Error::Refused(format!(
                "provider {} is not configured yet",
                P::NAME
            ))),
        }
    }

    /// `j`'s attributes split by the schema: computed (an Optional+Computed
    /// one too: the engine compares it only where the program sets it),
    /// else configured. An absent (`null`) one is neither. A sensitive
    /// computed value leaves as its label (`{"$secret": "T/NAME#P"}`), the
    /// object at `name` holding it: the provider keeps the bytes, and
    /// [`Provider::reveal`] answers them (R-45).
    fn split(&self, typ: &str, name: &str, j: Json) -> Object {
        let computed: Vec<String> = self
            .schema
            .attrs
            .iter()
            .filter(|((t, _), s)| t == typ && (s.has("computed") || s.has("optional_computed")))
            .map(|((_, p), _)| p.clone())
            .collect();
        let (mut attrs, mut comp) = (Map::new(), Map::new());
        if let Json::Object(m) = j {
            for (k, v) in m {
                if v.is_null() {
                    continue;
                }
                if computed.contains(&k) {
                    let v = match self.schema.is_sensitive(typ, &k) {
                        true => dform_core::provider::secret_json(&dform_core::value::null_label(
                            typ, name, &k,
                        )),
                        false => v,
                    };
                    comp.insert(k, v);
                } else {
                    attrs.insert(k, v);
                }
            }
        }
        (Json::Object(attrs), Json::Object(comp))
    }

    fn plan(&self, r: pb::PlanRequest) -> Result<pb::PlanResponse> {
        let doc = |v: Option<&pb::Value>| -> Result<Option<Json>> {
            v.map(wire::from_doc).transpose().map_err(Error::from)
        };
        let prior = doc(r.prior.as_ref())?;
        let desired = doc(r.desired.as_ref())?;
        let addr = Address {
            typ: r.r#type.clone(),
            name: r.name.clone(),
        };
        let k = self.kind(&addr.typ)?;
        if let (None, Some(_), Some(why)) = (&prior, &desired, k.not_created()) {
            return Err(Error::Refused(format!("plan {addr}: {why}")));
        }
        if let Some(d) = &desired {
            if let Some(why) = self.schema.missing_required(&addr.typ, d) {
                return Err(Error::Refused(format!("plan {addr}: {why}")));
            }
            // A provider configured later (from a tick's output) checks at
            // its Apply.
            let p = self.provider.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = p.as_ref() {
                k.check(p, d).map_err(|e| e.at(&format!("plan {addr}")))?;
            }
        }
        let changes =
            dform_core::provider::diff(&self.schema, &addr.typ, prior.as_ref(), desired.as_ref());
        let requires_replace = prior.is_some()
            && desired.is_some()
            && changes.iter().any(|c| {
                self.schema
                    .forces_new(&addr.typ, &dform_core::provider::norm_path(&c.path))
            });
        Ok(pb::PlanResponse {
            changes: changes.iter().map(pb::Change::from).collect(),
            requires_replace,
        })
    }

    fn apply(&self, r: pb::ApplyRequest, sink: backend::Progress) -> Result<pb::ApplyResponse> {
        let op = pb::Op::try_from(r.op).unwrap_or(pb::Op::Unspecified);
        if op == pb::Op::EndTick {
            return Ok(pb::ApplyResponse::default());
        }
        if !r.assertions.is_empty() {
            return Err(Error::Refused(format!(
                "apply {}[{:?}]: refinements on secret paths are not checked by this SDK yet",
                r.r#type, r.name
            )));
        }
        let k = self.kind(&r.r#type)?;
        let mut desired = wire::from_doc_or_empty(r.config.as_ref()).map_err(Error::from)?;
        if let Some(p) = r.keep.first().filter(|_| !P::KEEP || op != pb::Op::Update) {
            return Err(Error::Refused(format!(
                "apply {}[{:?}]: keep {p}: this provider cannot leave an attribute as it is",
                r.r#type, r.name
            )));
        }
        let progress = Progress::new(
            dform_core::ir::Address {
                typ: r.r#type.clone(),
                name: r.name.clone(),
            }
            .to_string(),
            sink,
        );
        let found = |p: &P, remote: &str| -> Result<Json> {
            k.read(p, remote)?
                .ok_or_else(|| Error::Refused(format!("{} {remote:?} does not exist", r.r#type)))
        };
        let refuse_create = || match k.not_created() {
            Some(why) => Err(Error::Refused(format!("apply {}: {why}", progress.address))),
            None => Ok(()),
        };
        let (remote, obj) = self.with(|p| match op {
            pb::Op::Create => {
                refuse_create()?;
                k.check(p, &desired)?;
                k.create(p, desired, &r.idempotency_key, &progress)
            }
            pb::Op::Update | pb::Op::Adopt => {
                k.check(p, &desired)?;
                let remote = match op {
                    pb::Op::Adopt => k.adopt(p, &r.remote)?,
                    _ => r.remote.clone(),
                };
                let prior = found(p, &remote)?;
                // `keep`: as Read answers it, else absent (`Provider::KEEP`).
                for path in &r.keep {
                    if let Some(v) = dform_core::provider::get_path(&prior, path) {
                        dform_core::provider::set_path(&mut desired, path, v.clone());
                    }
                }
                let obj = k.update(p, &remote, prior, desired, &progress)?;
                Ok((remote, obj))
            }
            pb::Op::Delete => {
                k.delete(p, &r.remote, &progress)?;
                Ok((String::new(), Json::Null))
            }
            pb::Op::Replace => {
                refuse_create()?;
                k.check(p, &desired)?;
                if !r.create_first {
                    k.delete(p, &r.remote, &progress)?;
                }
                k.create(p, desired, &r.idempotency_key, &progress)
            }
            pb::Op::Unspecified | pb::Op::EndTick => {
                Err(Error::Refused("an Apply call with no op".into()))
            }
        })?;
        let (attrs, computed) = self.split(&r.r#type, &r.name, obj);
        Ok(pb::ApplyResponse {
            remote,
            attrs: Some(wire::doc(&attrs)),
            computed: Some(wire::doc(&computed)),
            elapsed_ms: 0,
            notes: progress.into_notes(),
        })
    }

    fn answer(&self, call: Call, progress: backend::Progress) -> Result<Reply> {
        Ok(match call {
            Call::Handshake(_) => Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: P::NAME.to_string(),
                // `offline`: its Plan with no credentials is its schema's,
                // as with them but for `Lifecycle::check` (R-188).
                capabilities: match P::KEEP {
                    true => vec!["resource", "keep", "offline"],
                    false => vec!["resource", "offline"],
                }
                .into_iter()
                .map(String::from)
                .collect(),
                version: P::VERSION.to_string(),
                settings: Vec::new(),
                health: self
                    .kinds
                    .iter()
                    .filter(|(_, k)| k.answers_health())
                    .map(|(t, _)| t.to_string())
                    .collect(),
            }),
            Call::Configure(r) => {
                let config = wire::from_doc_or_empty(r.config.as_ref()).map_err(Error::from)?;
                // The engine's first Configure of a provider a program
                // configures says so; the settings come at the second.
                if config.get("deferred") == Some(&Json::Bool(true)) {
                    return Ok(Reply::Configure(pb::ConfigureResponse::default()));
                }
                let settings = config.get("settings").cloned().unwrap_or_else(|| json!({}));
                let (p, account) = P::configure(&settings)?;
                *self.provider.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
                Reply::Configure(pb::ConfigureResponse {
                    account,
                    notes: Vec::new(),
                })
            }
            Call::Schema(r) => Reply::Schema(pb::SchemaResponse {
                facts: wire::schema_facts(&self.schema, &r).map_err(Error::from)?,
                externs: self
                    .schema
                    .externs
                    .values()
                    .map(|e| pb::ExternDecl {
                        pred: e.name.clone(),
                        arity: e.args.len() as u32,
                        input: e.args.iter().map(|a| a.input).collect(),
                    })
                    .collect(),
                examples: self.examples.clone(),
                ..pb::SchemaResponse::default()
            }),
            Call::Query(r) => Reply::Query(self.query(r)?),
            Call::Read(r) => {
                let k = self.kind(&r.r#type)?;
                Reply::Read(match self.with(|p| k.read(p, &r.remote))? {
                    Some(j) => {
                        let (attrs, computed) = self.split(&r.r#type, &r.name, j);
                        pb::ReadResponse {
                            found: true,
                            attrs: Some(wire::doc(&attrs)),
                            computed: Some(wire::doc(&computed)),
                        }
                    }
                    None => pb::ReadResponse::default(),
                })
            }
            Call::Plan(r) => Reply::Plan(self.plan(r)?),
            Call::Apply(r) => Reply::Apply(self.apply(r, progress)?),
            Call::Import(r) => {
                let k = self.kind(&r.r#type)?;
                Reply::Import(match self.with(|p| k.read(p, &r.remote))? {
                    Some(j) => {
                        let (attrs, computed) = self.split(&r.r#type, &r.remote, j);
                        pb::ImportResponse {
                            found: true,
                            r#type: r.r#type,
                            name: r.remote,
                            attrs: Some(wire::doc(&attrs)),
                            computed: Some(wire::doc(&computed)),
                        }
                    }
                    None => pb::ImportResponse::default(),
                })
            }
            // The engine's call alone, under the deployment's lease.
            Call::Reveal(r) => {
                let h = r.held.unwrap_or_default();
                if r.lease.is_empty() {
                    return Err(Error::Refused(format!(
                        "reveal {} {}#{}: refused without the deployment's lease (a reveal \
                         is the engine's call)",
                        h.r#type, h.remote, h.path
                    )));
                }
                Reply::Reveal(pb::RevealResponse {
                    value: self.with(|p| p.reveal(&h))?,
                })
            }
            Call::Health(r) => Reply::Health(self.health(r)?),
        })
    }

    /// The rows of an extern, each value as the protocol has it.
    fn query(&self, r: pb::QueryRequest) -> Result<Vec<pb::Row>> {
        let inputs = r
            .inputs
            .iter()
            .map(wire::from_doc)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let rows = self.with(|p| p.query(&r.pred, &inputs))?;
        Ok(rows
            .iter()
            .map(|row| pb::Row {
                values: row
                    .iter()
                    .map(|v| pb::Value::from(&dform_core::provider::json_to_value(v)))
                    .collect(),
            })
            .collect())
    }

    /// Each object's health, as its type's [`Lifecycle::health`] judges
    /// it; `unknown` for one it judges none of.
    fn health(&self, r: pb::HealthRequest) -> Result<pb::HealthResponse> {
        let answers = r
            .objects
            .iter()
            .map(|o| {
                let k = self.kind(&o.r#type)?;
                let judged = match k.answers_health() {
                    true => self.with(|p| k.health(p, &o.remote))?,
                    false => None,
                };
                Ok(judged.unwrap_or_else(|| {
                    backend::health(pb::HealthState::Unknown, "the provider judges none")
                }))
            })
            .collect::<Result<_>>()?;
        Ok(pb::HealthResponse { answers })
    }
}

impl<P: Provider> Handler for Typed<P> {
    fn schemes(&self) -> Vec<String> {
        P::SCHEMES.iter().map(|s| s.to_string()).collect()
    }

    fn read_location(&self, location: &str) -> std::result::Result<Vec<u8>, crate::Failure> {
        self.read_versioned(location).map(|d| d.bytes)
    }

    /// A read before the program has configured the provider waits on it,
    /// as a resource of it would.
    fn read_versioned(
        &self,
        location: &str,
    ) -> std::result::Result<crate::Document, crate::Failure> {
        let p = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        match p.as_ref() {
            Some(p) => p.read(location),
            None => Err(crate::Failure::NotYet(format!(
                "{location}: provider {} is not configured yet",
                P::NAME
            ))),
        }
    }

    fn handle(
        &self,
        call: Call,
        progress: backend::Progress,
    ) -> std::result::Result<Reply, CallError> {
        self.answer(call, progress).map_err(|e| match e {
            Error::Refused(m) => CallError::Refused(m),
            Error::MaybeApplied(m) => CallError::MaybeApplied(m),
        })
    }
}
