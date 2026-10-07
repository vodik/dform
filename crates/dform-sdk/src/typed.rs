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
    /// Its `type_attr` (and `type_list_key`, `type_replace`, `type_retry`)
    /// facts, as `.df` text.
    const FACTS: &'static str;
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
    /// Configure from the provider block's settings (a program's
    /// `use NAME { .. }`, a secret among them revealed; `{}` when the
    /// program configures it with none, the engine's own keys never
    /// among them): the provider, and the account its credentials reach
    /// when it can tell.
    fn configure(settings: &Json) -> Result<(Self, Option<String>)>;
}

/// Where an Apply says how it goes (R-130), as its object's status
/// changes: a provider says so each time it sees a new one (a poll), never
/// on a timer; dform adds the time it has run.
pub struct Progress<'a> {
    address: String,
    sink: backend::Progress<'a>,
}

impl<'a> Progress<'a> {
    /// The progress of the object at `address` (`type["name"]`), told to
    /// `sink`.
    pub fn new(address: impl Into<String>, sink: backend::Progress<'a>) -> Progress<'a> {
        Progress {
            address: address.into(),
            sink,
        }
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
    /// The object `remote`, if it exists.
    fn read(p: &P, remote: &str) -> Result<Option<Self>>;
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
}

/// An object as the protocol has it: configured and computed.
type Object = (Json, Json);

/// One resource type, its type erased.
trait Kind<P>: Send + Sync {
    fn read(&self, p: &P, remote: &str) -> Result<Option<Json>>;
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
}

struct K<R>(std::marker::PhantomData<fn() -> R>);

fn to_json<R: Serialize>(r: &R) -> Result<Json> {
    Ok(serde_json::to_value(r)?)
}

fn from_json<R: DeserializeOwned>(typ: &str, j: Json) -> Result<R> {
    serde_json::from_value(j).map_err(|e| Error::Refused(format!("{typ}: {e}")))
}

impl<P: Provider, R: Lifecycle<P> + 'static> Kind<P> for K<R> {
    fn read(&self, p: &P, remote: &str) -> Result<Option<Json>> {
        R::read(p, remote)?.map(|r| to_json(&r)).transpose()
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
    /// else configured. An absent (`null`) one is neither.
    fn split(&self, typ: &str, j: Json) -> Object {
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
        if let Some(d) = &desired {
            for ((t, path), spec) in &self.schema.attrs {
                if t == &addr.typ
                    && spec.has("required")
                    && dform_core::provider::get_path(d, path).is_none()
                {
                    return Err(Error::Refused(format!(
                        "plan {addr}: required attribute {path} is not set"
                    )));
                }
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
        let desired = wire::from_doc_or_empty(r.config.as_ref()).map_err(Error::from)?;
        let progress = Progress::new(
            dform_core::ir::Address {
                typ: r.r#type.clone(),
                name: r.name.clone(),
            }
            .to_string(),
            sink,
        );
        let progress = &progress;
        let found = |p: &P, remote: &str| -> Result<Json> {
            k.read(p, remote)?
                .ok_or_else(|| Error::Refused(format!("{} {remote:?} does not exist", r.r#type)))
        };
        let (remote, obj) = self.with(|p| match op {
            pb::Op::Create => {
                k.check(p, &desired)?;
                k.create(p, desired, &r.idempotency_key, progress)
            }
            pb::Op::Update | pb::Op::Adopt => {
                k.check(p, &desired)?;
                let prior = found(p, &r.remote)?;
                let obj = k.update(p, &r.remote, prior, desired, progress)?;
                Ok((r.remote.clone(), obj))
            }
            pb::Op::Delete => {
                k.delete(p, &r.remote, progress)?;
                Ok((String::new(), Json::Null))
            }
            pb::Op::Replace => {
                k.check(p, &desired)?;
                if !r.create_first {
                    k.delete(p, &r.remote, progress)?;
                }
                k.create(p, desired, &r.idempotency_key, progress)
            }
            pb::Op::Unspecified | pb::Op::EndTick => {
                Err(Error::Refused("an Apply call with no op".into()))
            }
        })?;
        let (attrs, computed) = self.split(&r.r#type, obj);
        Ok(pb::ApplyResponse {
            remote,
            attrs: Some(wire::doc(&attrs)),
            computed: Some(wire::doc(&computed)),
            elapsed_ms: 0,
            notes: Vec::new(),
        })
    }

    fn answer(&self, call: Call, progress: backend::Progress) -> Result<Reply> {
        Ok(match call {
            Call::Handshake(_) => Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: P::NAME.to_string(),
                capabilities: vec!["resource".to_string()],
                version: P::VERSION.to_string(),
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
                Reply::Configure(pb::ConfigureResponse { account })
            }
            Call::Schema(r) => Reply::Schema(pb::SchemaResponse {
                facts: wire::schema_facts(&self.schema, &r).map_err(Error::from)?,
                examples: self.examples.clone(),
                ..pb::SchemaResponse::default()
            }),
            Call::Query(r) => {
                return Err(Error::Refused(format!(
                    "provider {} answers no extern {}",
                    P::NAME,
                    r.pred
                )));
            }
            Call::Read(r) => {
                let k = self.kind(&r.r#type)?;
                Reply::Read(match self.with(|p| k.read(p, &r.remote))? {
                    Some(j) => {
                        let (attrs, computed) = self.split(&r.r#type, j);
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
                        let (attrs, computed) = self.split(&r.r#type, j);
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
            // A typed provider keeps no secret for another to read.
            Call::Reveal(r) => {
                let h = r.held.unwrap_or_default();
                return Err(Error::Refused(format!(
                    "reveal {} {}#{}: provider {} holds no secret",
                    h.r#type,
                    h.remote,
                    h.path,
                    P::NAME
                )));
            }
        })
    }
}

impl<P: Provider> Handler for Typed<P> {
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
