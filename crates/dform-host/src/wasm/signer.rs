//! A wasm provider's signer (`dform_core::plugin::signer`): a component of
//! `world signer-component` beside the provider's (`NAME_signer.wasm`
//! beside `NAME.wasm`, as `cargo xtask components` builds them), in a
//! store of its own, linked with nothing: one that imports anything is
//! refused, naming what, so it reaches no network, no file, no clock and
//! no variable; what it needs, the host performs (`crate::signed`). Its
//! calls are one at a time, each under a deadline of [`CALL`].

use super::{engine, load, ticks};
use anyhow::{Context, Result, bail};
use dform_core::plugin::signer::{
    Command, Got, Material, Need, Outcome, Recipe, Request, Signer, TlsClient,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use wasmtime::Store;
use wasmtime::component::Linker;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/host",
        world: "dform:host/signer-component",
    });
}

use bindings::exports::dform::host::signer as w;

/// How long one of a signer's calls may run: it does arithmetic.
const CALL: Duration = Duration::from_secs(10);

/// A signer component, instantiated.
pub struct Component {
    path: PathBuf,
    instance: Mutex<(Store<()>, bindings::SignerComponent)>,
}

/// Where the signer of the provider component at `provider` is:
/// `NAME_signer.wasm` beside `NAME.wasm`.
pub fn beside(provider: &Path) -> PathBuf {
    let stem = provider
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    provider.with_file_name(format!("{stem}_signer.wasm"))
}

impl Component {
    /// The signer component at `path`, linked with nothing.
    pub fn load(path: &Path) -> Result<Component> {
        let engine = engine()?;
        let component =
            load(engine, path).with_context(|| format!("load the signer {}", path.display()))?;
        let imports: Vec<String> = component
            .component_type()
            .imports(engine)
            .map(|(name, _)| name.to_string())
            .collect();
        if !imports.is_empty() {
            bail!(
                "the signer {} imports {}: a signer imports nothing (wit/host's world \
                 signer-component); what it needs, it asks the host for",
                path.display(),
                imports.join(", ")
            );
        }
        let mut store = Store::new(engine, ());
        store.set_epoch_deadline(ticks(CALL));
        let instance =
            bindings::SignerComponent::instantiate(&mut store, &component, &Linker::new(engine))
                .map_err(anyhow::Error::from)
                .with_context(|| format!("instantiate the signer {}", path.display()))?;
        Ok(Component {
            path: path.to_path_buf(),
            instance: Mutex::new((store, instance)),
        })
    }

    /// `f`, one of its calls, under its deadline; a trap is its failure.
    fn call<T>(
        &self,
        f: impl FnOnce(&mut Store<()>, &w::Guest) -> wasmtime::Result<Result<T, String>>,
    ) -> Result<T, String> {
        // A trap leaves the instance as it was: a call's own heap goes.
        let mut held = self.instance.lock().unwrap_or_else(|e| e.into_inner());
        let (store, instance) = &mut *held;
        store.set_epoch_deadline(ticks(CALL));
        f(store, instance.dform_host_signer())
            .map_err(|trap| format!("the signer {} trapped: {trap:#}", self.path.display()))?
    }
}

impl Signer for Component {
    fn needs(&self) -> Result<Vec<Need>, String> {
        let needs = self.call(|s, g| g.call_needs(s))?;
        Ok(needs.into_iter().map(Into::into).collect())
    }

    fn step(&self, got: &[Got]) -> Result<Outcome, String> {
        let got: Vec<w::Got> = got.iter().cloned().map(Into::into).collect();
        self.call(|s, g| g.call_step(s, &got)).map(Into::into)
    }

    fn sign(&self, req: Request, material: &Material, now: u64) -> Result<Request, String> {
        let (req, material): (w::Request, w::Material) = (req.into(), material.clone().into());
        self.call(|s, g| g.call_sign(s, &req, &material, now))
            .map(Into::into)
    }

    fn refresh(&self, material: &Material, now: u64) -> Result<Outcome, String> {
        let material: w::Material = material.clone().into();
        self.call(|s, g| g.call_refresh(s, &material, now))
            .map(Into::into)
    }
}

impl From<w::Request> for Request {
    fn from(r: w::Request) -> Request {
        Request {
            method: r.method,
            url: r.url,
            headers: r.headers,
            body: r.body,
        }
    }
}

impl From<Request> for w::Request {
    fn from(r: Request) -> w::Request {
        w::Request {
            method: r.method,
            url: r.url,
            headers: r.headers,
            body: r.body,
        }
    }
}

impl From<w::Need> for Need {
    fn from(n: w::Need) -> Need {
        match n {
            w::Need::Env(vars) => Need::Env(vars),
            w::Need::File(path) => Need::File(path),
            w::Need::Exec(c) => Need::Exec(Command {
                argv: c.argv,
                env: c.env,
            }),
            w::Need::Fetch(r) => Need::Fetch(r.into()),
            w::Need::Given => Need::Given,
        }
    }
}

impl From<Got> for w::Got {
    fn from(g: Got) -> w::Got {
        match g {
            Got::Env(vars) => w::Got::Env(vars),
            Got::File(bytes) => w::Got::File(bytes),
            Got::Exec(out) => w::Got::Exec(out),
            Got::Fetch(r) => w::Got::Fetch(w::Response {
                status: r.status,
                headers: r.headers,
                body: r.body,
                at: r.at,
            }),
            Got::Given(values) => w::Got::Given(values),
        }
    }
}

impl From<w::Recipe> for Recipe {
    fn from(r: w::Recipe) -> Recipe {
        match r {
            w::Recipe::Bearer(t) => Recipe::Bearer(t),
            w::Recipe::Headers(h) => Recipe::Headers(h),
            w::Recipe::Signed => Recipe::Signed,
            w::Recipe::TlsClient(t) => Recipe::TlsClient(TlsClient {
                cert: t.cert,
                key: t.key,
            }),
            w::Recipe::Proxy(p) => Recipe::Proxy(p),
            w::Recipe::Ca(pem) => Recipe::Ca(pem),
            w::Recipe::Insecure => Recipe::Insecure,
            w::Recipe::ServerName(n) => Recipe::ServerName(n),
        }
    }
}

impl From<Recipe> for w::Recipe {
    fn from(r: Recipe) -> w::Recipe {
        match r {
            Recipe::Bearer(t) => w::Recipe::Bearer(t),
            Recipe::Headers(h) => w::Recipe::Headers(h),
            Recipe::Signed => w::Recipe::Signed,
            Recipe::TlsClient(t) => w::Recipe::TlsClient(w::TlsClient {
                cert: t.cert,
                key: t.key,
            }),
            Recipe::Proxy(p) => w::Recipe::Proxy(p),
            Recipe::Ca(pem) => w::Recipe::Ca(pem),
            Recipe::Insecure => w::Recipe::Insecure,
            Recipe::ServerName(n) => w::Recipe::ServerName(n),
        }
    }
}

impl From<w::Material> for Material {
    fn from(m: w::Material) -> Material {
        Material {
            bytes: m.bytes,
            recipe: m.recipe.into_iter().map(Into::into).collect(),
            expires: m.expires,
            endpoint: m.endpoint,
            namespace: m.namespace,
        }
    }
}

impl From<Material> for w::Material {
    fn from(m: Material) -> w::Material {
        w::Material {
            bytes: m.bytes,
            recipe: m.recipe.into_iter().map(Into::into).collect(),
            expires: m.expires,
            endpoint: m.endpoint,
            namespace: m.namespace,
        }
    }
}

impl From<w::Outcome> for Outcome {
    fn from(o: w::Outcome) -> Outcome {
        match o {
            w::Outcome::More(needs) => Outcome::More(needs.into_iter().map(Into::into).collect()),
            w::Outcome::Material(m) => Outcome::Material(m.into()),
        }
    }
}
