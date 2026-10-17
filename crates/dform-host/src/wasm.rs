//! The component host (the `wasm` feature, experimental): a provider that
//! is a component of `dform:host/hosted-provider` (wit/host), run by
//! wasmtime behind dform-core's `Provider` trait.
//!
//! One instance per provider per run, on a thread of its own: the
//! provider's calls are async (the component model's async ABI, R-130),
//! so every call submitted runs at once on it, each a subtask, as many as
//! the component takes (a synchronous handler takes one at a time). An
//! Apply answers a stream of events, told as they are written, and a
//! future of its result. A trap is the provider crashing (`Crashed`), and
//! the instance answers no call after one. Every call has an epoch deadline,
//! a backstop under the link's own timeout (`plugin::timed`): a call past
//! it is `MaybeApplied` for an Apply, `Refused` otherwise.
//!
//! The run's end stops the instance ([`Wasm::stop`]): every call still in
//! flight in it (one the link gave up on at its timeout) is cancelled,
//! its span ended with the cancellation on it, and the instance's thread
//! is joined, so a call's span, and every span above it, is in the trace
//! the run writes. A component busy in its own code when stopped is
//! interrupted at the epoch's next tick.
//!
//! Grants, not walls: the host's own interfaces are imports any component
//! has; a component that imports `wasi:sockets` or `wasi:http` is refused
//! unless dform.toml grants it, naming both, and one that imports
//! `wasi:filesystem` likewise (it then sees the filesystem as the user
//! does, from `/`). A `wasi:http` request is sent as a `dform:host/http`
//! one is, through the host's `send` ([`Outgoing`]). No environment
//! variable is passed in: a credential is had by name
//! (`dform:host/secrets`), never from the environment.
//!
//! Traced in tracing's idiom, as a native provider is and more: its
//! start is `wasm.compile` (the cache's code, or compiled), `wasm.link`
//! and `wasm.instantiate`, under the provider's start; each call's export
//! runs in the call's `provider.call` span, which it records how long the
//! call took in the provider on (`provider.took`, in ms) and a trap that
//! stopped it (`error.type`): the provider is in dform's process, so its
//! side of the call is no span of its own. A request the component sends
//! through the host is under its call: each export is started on a task
//! the host knows by its call's span while it runs, and an import finds
//! it by the root of its async call stack ([`State::exports`]).

use crate::ratelimit::{Limiter, Paced};
use crate::services::Services;
use anyhow::{Context, Result, anyhow};
use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Session, Ticket};
use dform_core::plugin::bundle;
use dform_core::plugin::host::{self as h, Calls, Grants, Hosting, Manifest};
use dform_core::plugin::link::Link;
use dform_core::plugin::pb;
use dform_core::plugin::signer::Signer;
use dform_core::plugin::timed;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};
use tracing::Instrument;
use wasmtime::component::{
    Access, Accessor, Component, ComponentNamedList, FutureConsumer, GuestTaskId, HasSelf, Lift,
    Linker, Lower, Resource, ResourceTable, Source, StreamConsumer, StreamResult, TypedFunc,
};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut, UpdateDeadline};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

pub mod signer;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/host",
        world: "dform:host/hosted-provider",
        require_store_data_send: true,
        // Opening a credential may perform its signer's needs (a fetch),
        // which are traced in the span of the export it is opened in.
        imports: {
            "dform:host/secrets.open": store,
        },
        with: {
            "wasi:cli": wasmtime_wasi::p3::bindings::cli,
            "wasi:clocks": wasmtime_wasi::p3::bindings::clocks,
            "wasi:random": wasmtime_wasi::p3::bindings::random,
            "wasi:filesystem": wasmtime_wasi::p3::bindings::filesystem,
            "wasi:sockets": wasmtime_wasi::p3::bindings::sockets,
            "wasi:http": wasmtime_wasi_http::p3::bindings::http,
            "dform:host/types.credential": super::Credential,
            "dform:host/types.tunnel": super::Tunnel,
        },
    });
}

dform_wit::convert!(conv, super::bindings::dform::provider::types);

use bindings::dform::host as wh;
use bindings::dform::provider::types as wp;

/// A credential a component opened: the host's handle, never its value.
pub struct Credential {
    handle: h::Handle,
    name: String,
    endpoint: Option<String>,
    namespace: Option<String>,
}

/// A tunnel a component forwarded.
pub struct Tunnel {
    handle: h::Handle,
}

/// How often the epoch ticks, and how many ticks a call may run.
const TICK: Duration = Duration::from_millis(100);
/// The backstop: a call still running after this has its instance
/// interrupted. The link's timeout (`[providers.NAME] timeout`) answers the
/// engine long before; this stops the instance spinning after it.
const DEADLINE: Duration = Duration::from_secs(3600);
/// How long the run's end waits for a stopped instance's thread.
const JOIN: Duration = Duration::from_secs(5);

/// The engine every component runs on, its epoch ticking.
fn engine() -> Result<&'static Engine> {
    static ENGINE: OnceLock<std::result::Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut c = Config::new();
            c.wasm_component_model(true)
                .wasm_component_model_async(true)
                .epoch_interruption(true);
            let e = Engine::new(&c).map_err(|e| format!("{e:#}"))?;
            let ticker = e.clone();
            std::thread::Builder::new()
                .name("dform-wasm-epoch".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(TICK);
                        ticker.increment_epoch();
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(e)
        })
        .as_ref()
        .map_err(|e| anyhow!("start the wasm engine: {e}"))
}

struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    host: Arc<Services>,
    outgoing: Outgoing,
    /// The `provider.call` spans of the calls in flight, by their
    /// export's task: an import finds its call's by the task it is made in
    /// ([`made_in`]), so its spans are the call's children.
    exports: std::collections::HashMap<GuestTaskId, tracing::Span>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            hooks: &mut self.outgoing,
            table: &mut self.table,
            ctx: &mut self.http,
        }
    }
}

/// A component's `wasi:http` outgoing requests, sent as its
/// `dform:host/http` ones are: through its host's `send` (dform's pool,
/// its window, an `io.call` span), never a connection of their own. A
/// `wasi:http` request names no session and no bucket, so the
/// component's are paced in its default bucket, its sessions' together;
/// one that wants its own names it through `dform:host/http`.
struct Outgoing(Arc<Services>);

type Answered = wasmtime_wasi_http::Result<(
    http::Response<wasmtime_wasi_http::WasiBody>,
    Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>,
)>;

impl wasmtime_wasi_http::WasiHttpHooks for Outgoing {
    fn send_request(
        &mut self,
        request: http::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
        _: Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>,
    ) -> Box<dyn Future<Output = Answered> + Send> {
        let host = self.0.clone();
        Box::new(async move {
            let req = Outgoing::request(request, options).await?;
            let sent = tokio::task::spawn_blocking(move || host.send(req, None, None))
                .await
                .map_err(|_| wasmtime_wasi_http::Error::ConnectionTerminated)?;
            let resp = Outgoing::response(sent)?;
            let done: Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send> =
                Box::new(async { Ok(()) });
            Ok((resp, done))
        })
    }
}

impl Outgoing {
    /// The component's request as the host's `send` takes it, its body
    /// read whole; its first-byte timeout the request's.
    async fn request(
        request: http::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
    ) -> wasmtime_wasi_http::Result<h::HttpRequest> {
        use http_body_util::BodyExt;
        let (head, body) = request.into_parts();
        let body = body.collect().await?.to_bytes();
        Ok(h::HttpRequest {
            method: head.method.to_string(),
            url: head.uri.to_string(),
            headers: head
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into()))
                .collect(),
            body: body.to_vec(),
            timeout: options.and_then(|o| o.first_byte_timeout),
            ..h::HttpRequest::default()
        })
    }

    /// The host's answer as `wasi:http` gives it; a failure as the error
    /// of its class.
    fn response(
        sent: Result<h::HttpResponse, h::Error>,
    ) -> wasmtime_wasi_http::Result<http::Response<wasmtime_wasi_http::WasiBody>> {
        use http_body_util::BodyExt;
        use wasmtime_wasi_http::Error as E;
        let r = sent.map_err(|e| match e.class {
            h::Class::Retryable => E::ConnectionRefused,
            h::Class::MaybeApplied => E::ConnectionReadTimeout,
            h::Class::Final => E::HttpRequestDenied,
        })?;
        let mut resp = http::Response::builder().status(r.status);
        for (k, v) in &r.headers {
            resp = resp.header(k, v);
        }
        let body = http_body_util::Full::new(bytes::Bytes::from(r.body))
            .map_err(E::from)
            .boxed_unsync();
        resp.body(body).map_err(|_| E::HttpProtocolError)
    }
}

fn error(e: h::Error) -> wh::types::Error {
    wh::types::Error {
        class: match e.class {
            h::Class::Final => wh::types::ErrorClass::Final,
            h::Class::Retryable => wh::types::ErrorClass::Retryable,
            h::Class::MaybeApplied => wh::types::ErrorClass::MaybeApplied,
        },
        message: e.message,
    }
}

fn failure(f: h::Failure) -> wh::types::NotYetOrError {
    match f {
        h::Failure::NotYet(m) => wh::types::NotYetOrError::NotYet(m),
        h::Failure::Error(e) => wh::types::NotYetOrError::Error(error(e)),
    }
}

fn target(t: wh::ssh::Target) -> h::Target {
    h::Target {
        host: t.host,
        user: t.user,
        port: t.port,
    }
}

impl wh::types::HostCredential for State {
    fn name(&mut self, c: Resource<Credential>) -> String {
        self.table
            .get(&c)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    fn endpoint(&mut self, c: Resource<Credential>) -> Option<String> {
        self.table.get(&c).ok().and_then(|c| c.endpoint.clone())
    }

    fn namespace(&mut self, c: Resource<Credential>) -> Option<String> {
        self.table.get(&c).ok().and_then(|c| c.namespace.clone())
    }

    fn drop(&mut self, c: Resource<Credential>) -> wasmtime::Result<()> {
        self.table.delete(c)?;
        Ok(())
    }
}

impl wh::types::HostTunnel for State {
    fn to(&mut self, t: Resource<Tunnel>) -> wh::types::Endpoint {
        let e = self
            .table
            .get(&t)
            .ok()
            .and_then(|t| self.host.tunnel(t.handle));
        let e = e.unwrap_or(h::Endpoint {
            host: String::new(),
            port: 0,
        });
        wh::types::Endpoint {
            host: e.host,
            port: e.port,
        }
    }

    fn drop(&mut self, t: Resource<Tunnel>) -> wasmtime::Result<()> {
        self.table.delete(t)?;
        Ok(())
    }
}

impl wh::types::Host for State {}

impl wh::secrets::Host for State {}

impl<U> wh::secrets::HostWithStore<U> for HasSelf<State> {
    /// The credential `name`, opened in the span of the call it is
    /// opened in ([`made_in`]): what its signer's needs make, a fetch,
    /// is under that call, as natively.
    fn open(
        mut s: Access<'_, U, Self>,
        name: String,
    ) -> Result<Resource<Credential>, wh::types::Error> {
        let span = made_in(&mut s);
        let state = s.get();
        let o = span.in_scope(|| state.host.open(&name)).map_err(error)?;
        state
            .table
            .push(Credential {
                handle: o.handle,
                name,
                endpoint: o.endpoint,
                namespace: o.namespace,
            })
            .map_err(|e| error(h::Error::fatal(e.to_string())))
    }
}

/// The span an import is made in: the `provider.call` of the call it is
/// made for, the one its export's task was started in ([`State::exports`]),
/// the root of the import's call stack; else the span the instance is in.
fn made_in<U: 'static>(s: &mut Access<'_, U, HasSelf<State>>) -> tracing::Span {
    let root = s
        .as_context_mut()
        .async_call_stack()
        .ok()
        .and_then(|stack| stack.last());
    root.and_then(|root| s.get().exports.get(&root).cloned())
        .unwrap_or_else(tracing::Span::current)
}

/// The host an import calls, and the span it is made in ([`made_in`]).
fn host_in<U: 'static>(a: &Accessor<U, HasSelf<State>>) -> (Arc<Services>, tracing::Span) {
    a.with(|mut s| (s.get().host.clone(), made_in(&mut s)))
}

impl State {
    /// The host's handles of the credential and the tunnel a call names.
    fn handles(
        &self,
        auth: Option<Resource<Credential>>,
        via: Option<Resource<Tunnel>>,
    ) -> Result<(Option<h::Handle>, Option<h::Handle>), wh::types::Error> {
        let bad =
            |e: wasmtime::component::ResourceTableError| error(h::Error::fatal(e.to_string()));
        let auth = match auth {
            Some(a) => Some(self.table.get(&a).map_err(bad)?.handle),
            None => None,
        };
        let via = match via {
            Some(t) => Some(self.table.get(&t).map_err(bad)?.handle),
            None => None,
        };
        Ok((auth, via))
    }
}

/// `f`, a call of the host's that waits on the world, on a thread of the
/// blocking pool: the import is a subtask (it is an `async func`), so the
/// component goes on meanwhile, and its other calls with it. In `span`,
/// the export's the import was made in ([`made_in`]).
async fn waited<T, E>(
    span: tracing::Span,
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    E: From<h::Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || span.in_scope(f))
        .await
        .unwrap_or_else(|e| Err(h::Error::fatal(format!("the host's call failed: {e}")).into()))
}

impl wh::http::Host for State {}

impl<U> wh::http::HostWithStore<U> for HasSelf<State> {
    async fn send(
        a: &Accessor<U, Self>,
        req: wh::http::Request,
        auth: Option<Resource<Credential>>,
        via: Option<Resource<Tunnel>>,
    ) -> Result<wh::http::Response, wh::types::Error> {
        let (host, (auth, via), span) = a.with(|mut s| {
            let span = made_in(&mut s);
            let s = s.get();
            s.handles(auth, via).map(|h| (s.host.clone(), h, span))
        })?;
        let req = h::HttpRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
            timeout: req.timeout_ms.map(|ms| Duration::from_millis(ms.into())),
            bucket: req.bucket,
            session: Session(req.session),
            // Its span's parent is the export's it is sent in.
            call: None,
        };
        let r = waited(span, move || host.send(req, auth, via))
            .await
            .map_err(error)?;
        Ok(wh::http::Response {
            status: r.status,
            headers: r.headers,
            body: r.body,
        })
    }
}

impl<U> wh::ssh::HostWithStore<U> for HasSelf<State> {
    async fn exec(
        a: &Accessor<U, Self>,
        on: wh::ssh::Target,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<wh::ssh::Run, wh::types::NotYetOrError> {
        let (host, span) = host_in(a);
        let r = waited(span, move || {
            host.exec(&target(on), &argv, stdin.as_deref())
        })
        .await
        .map_err(failure)?;
        Ok(wh::ssh::Run {
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }
}

impl wh::ssh::Host for State {
    fn write(
        &mut self,
        on: wh::ssh::Target,
        path: String,
        data: Vec<u8>,
        mode: u32,
    ) -> Result<(), wh::types::Error> {
        self.host
            .write(&target(on), &path, &data, mode)
            .map_err(error)
    }

    fn forward(
        &mut self,
        via: wh::ssh::Target,
        to: wh::types::Endpoint,
    ) -> Result<Resource<Tunnel>, wh::types::Error> {
        let handle = self
            .host
            .forward(
                &target(via),
                &h::Endpoint {
                    host: to.host,
                    port: to.port,
                },
            )
            .map_err(error)?;
        self.table
            .push(Tunnel { handle })
            .map_err(|e| error(h::Error::fatal(e.to_string())))
    }
}

impl wh::io::Host for State {}

impl<U> wh::io::HostWithStore<U> for HasSelf<State> {
    async fn read(
        a: &Accessor<U, Self>,
        location: String,
    ) -> Result<Vec<u8>, wh::types::NotYetOrError> {
        let (host, span) = host_in(a);
        waited(span, move || host.io_read(&location))
            .await
            .map_err(failure)
    }

    async fn read_versioned(
        a: &Accessor<U, Self>,
        location: String,
    ) -> Result<wh::io::Document, wh::types::NotYetOrError> {
        let (host, span) = host_in(a);
        let d = waited(span, move || host.io_read_versioned(&location))
            .await
            .map_err(failure)?;
        Ok(wh::io::Document {
            data: d.bytes,
            version: d.version,
        })
    }
}

impl wh::git::Host for State {
    fn commit(
        &mut self,
        repo: String,
        branch: String,
        files: Vec<wh::git::File>,
        message: String,
    ) -> Result<String, wh::types::Error> {
        let files = files
            .into_iter()
            .map(|f| h::GitFile {
                path: f.path,
                data: f.data,
            })
            .collect();
        self.host
            .git_commit(&repo, &branch, files, &message)
            .map_err(error)
    }
}

impl wh::log::Host for State {
    fn log(&mut self, level: wh::log::Level, message: String) {
        use wh::log::Level as L;
        let level = match level {
            L::Debug => h::Level::Debug,
            L::Info => h::Level::Info,
            L::Warn => h::Level::Warn,
            L::Error => h::Level::Error,
        };
        self.host.log(level, &message);
    }
}

/// Where this process keeps compiled components, in place of the
/// operator's cache ([`compile_cache`]).
static COMPILED_IN: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Keep this process's compiled components in `dir`, not the operator's
/// cache: a test's scratch directory, set before its first component is
/// loaded. `false` when another directory was set already.
pub fn compile_cache(dir: std::path::PathBuf) -> bool {
    COMPILED_IN.get_or_init(|| dir.clone()) == &dir
}

/// The directory compiled components are kept in: the process's own
/// ([`compile_cache`]), else `$XDG_CACHE_HOME/dform/wasm`.
fn compiled_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = COMPILED_IN.get() {
        return Some(d.clone());
    }
    cache_dir().map(|c| c.join("wasm"))
}

/// `$XDG_CACHE_HOME/dform`, else `~/.cache/dform`.
fn cache_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|c| c.join("dform"))
}

/// Where the sources of the component of digest `digest` are written:
/// `$XDG_CACHE_HOME/dform/providers/<digest>/src` (under the process's
/// own directory, [`compile_cache`], when it has one).
fn sources_dir(digest: &str) -> Option<std::path::PathBuf> {
    let root = match COMPILED_IN.get() {
        Some(d) => d.join("providers"),
        None => cache_dir()?.join("providers"),
    };
    Some(root.join(digest).join("src"))
}

/// A component file as read: its bytes, and their digest, by which its
/// compiled code and its sources are kept.
pub struct Read {
    pub bytes: Vec<u8>,
    digest: String,
}

impl Read {
    pub fn of(bytes: Vec<u8>) -> Read {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Read { bytes, digest }
    }

    pub fn file(path: &Path) -> Result<Read> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Read::of(bytes))
    }

    /// The `.df` sources it carries (`bundle::SOURCES`), written out under
    /// the host's cache the first time ([`sources_dir`]): where they are.
    /// One that carries none, or that cannot be written, has none.
    fn sources(&self) -> Option<std::path::PathBuf> {
        let section = bundle::section(&self.bytes, bundle::SOURCES).ok()??;
        let dir = sources_dir(&self.digest)?;
        if dir.exists() {
            return Some(dir);
        }
        let files = bundle::Sources::decode(section).ok()?;
        // Written aside and renamed, so a concurrent run never reads half.
        let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
        for (path, bytes) in &files.0 {
            let to = tmp.join(path);
            std::fs::create_dir_all(to.parent()?).ok()?;
            std::fs::write(&to, bytes).ok()?;
        }
        std::fs::create_dir_all(&tmp).ok()?;
        match std::fs::rename(&tmp, &dir) {
            Ok(()) => Some(dir),
            Err(_) => {
                let _ = std::fs::remove_dir_all(&tmp);
                dir.exists().then_some(dir)
            }
        }
    }
}

/// The component `read` (the file `path`'s, or carried in it), compiled,
/// or as compiled before: compiling a provider takes seconds, so the
/// native code is kept in `$XDG_CACHE_HOME/dform/wasm/<sha256 of the
/// component>.cwasm`, written by this engine (wasmtime refuses one
/// another version wrote, and it is compiled again). A `wasm.compile`
/// span, whose `cached` says which.
#[tracing::instrument(
    target = "dform",
    level = "debug",
    name = "wasm.compile",
    skip_all,
    fields(path = %path.display(), cached = tracing::field::Empty)
)]
fn load(engine: &Engine, read: &Read, path: &Path) -> Result<Component> {
    let dir = compiled_dir();
    let cached = dir
        .as_ref()
        .map(|d| d.join(format!("{}.cwasm", read.digest)));
    if let Some(c) = &cached
        && c.exists()
    {
        // SAFETY: the file is one this host wrote with `serialize` (by
        // its content's digest, in the operator's own cache); wasmtime
        // checks it was compiled by this version and configuration.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, c) } {
            tracing::Span::current().record("cached", true);
            return Ok(component);
        }
    }
    tracing::Span::current().record("cached", false);
    let component = Component::new(engine, &read.bytes)?;
    if let (Some(d), Some(c)) = (&dir, &cached)
        && let Ok(native) = component.serialize()
        && std::fs::create_dir_all(d).is_ok()
    {
        // Written aside and renamed, so a concurrent run never reads half.
        let tmp = c.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, native).is_ok() {
            let _ = std::fs::rename(&tmp, c);
        }
    }
    Ok(component)
}

/// What `component` imports, as a manifest.
pub fn manifest(engine: &Engine, component: &Component) -> Manifest {
    let ty = component.component_type();
    let names: Vec<String> = ty.imports(engine).map(|(n, _)| n.to_string()).collect();
    Manifest::of(names.iter().map(String::as_str), true)
}

/// A call as dform submitted it: its ticket, and the number it is traced
/// by (`backend::Within`, `timed::call_span`), which the host's spans of
/// the requests it sends are under, as a native provider's are.
struct Submitted {
    t: Ticket,
    call: Call,
    number: Option<u64>,
}

/// What the instance's thread sends back: an event of a call in flight,
/// or a call's answer.
enum Back {
    Event(Ticket, pb::Event),
    Answer(Ticket, Box<Result<Reply, CallError>>),
}

/// A running component provider: its instance on a thread of its own,
/// which takes every call as it is submitted and runs them at once.
pub struct Wasm {
    calls: Option<tokio::sync::mpsc::UnboundedSender<Submitted>>,
    back: std::sync::mpsc::Receiver<Back>,
    next: u64,
    /// Why it answers no more calls, once it trapped.
    dead: Arc<Mutex<Option<String>>>,
    /// Calls refused before they were sent (it had trapped).
    refused: Vec<(Ticket, CallError)>,
    /// Stops the instance, from any thread ([`Wasm::stop`]).
    stop: Stopping,
    /// The instance's thread, joined as the provider is dropped.
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How the instance is stopped: a flag its epoch's callback reads (a
/// component busy in its own code), and a wake for its thread (one
/// waiting on its calls).
#[derive(Clone, Default)]
struct Stopping {
    stopped: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
}

impl Stopping {
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

impl Wasm {
    /// Load, admit (its imports against `grants`) and instantiate the
    /// component at `path`, with the signer it carries
    /// ([`signer::of_provider`]), its sources written out ([`Read`]).
    pub fn start(path: &Path, grants: Grants, limiter: Arc<Limiter>) -> Result<(Wasm, Manifest)> {
        let program = path.display().to_string();
        let engine = engine()?;
        let read = Read::file(path)?;
        let signer = signer::of_provider(&read, path)?;
        let component = load(engine, &read, path)
            .with_context(|| format!("load the provider component {program}"))?;
        let mut manifest = manifest(engine, &component);
        manifest.sources = read.sources();
        grants
            .admit(&manifest)
            .map_err(|e| anyhow!("{e} ({program})"))?;
        let (calls, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, back) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let dead = Arc::new(Mutex::new(None));
        let stop = Stopping::default();
        let instance = Instance {
            program: program.clone(),
            started_in: tracing::Span::current(),
            signer,
            grants,
            limiter,
            dead: dead.clone(),
            stop: stop.clone(),
            tx,
        };
        let thread = std::thread::Builder::new()
            .name("dform-wasm".into())
            .spawn(move || instance.run(engine, component, rx, ready))
            .context("start the wasm provider's thread")?;
        started
            .recv()
            .unwrap_or_else(|_| Err(anyhow!("its thread exited")))
            .with_context(|| format!("instantiate the provider component {program}"))?;
        Ok((
            Wasm {
                calls: Some(calls),
                back,
                next: 0,
                dead,
                refused: Vec::new(),
                stop,
                thread: Some(thread),
            },
            manifest,
        ))
    }

    /// Stop the instance: every call in flight in it answered as
    /// cancelled, its span ended, and the thread let go of the store.
    fn stop(&mut self) {
        self.stop.stop();
        self.calls.take();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + JOIN;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = thread.join();
    }
}

/// The instance's side: what it was started as, and where answers go.
struct Instance {
    program: String,
    /// The provider's start, until it is instantiated: its
    /// instantiation's spans are under it, its calls' under theirs.
    started_in: tracing::Span,
    /// Its signer, which its host's credentials its provider makes are
    /// made and applied with.
    signer: Arc<dyn Signer + Send + Sync>,
    grants: Grants,
    /// What its host paces its requests by ([`crate::ratelimit::Paced`]).
    limiter: Arc<Limiter>,
    dead: Arc<Mutex<Option<String>>>,
    /// Whether the run's end stopped it, and the wake that says so.
    stop: Stopping,
    tx: std::sync::mpsc::Sender<Back>,
}

impl Instance {
    fn linker(&self, engine: &Engine) -> Result<Linker<State>> {
        let mut linker: Linker<State> = Linker::new(engine);
        // WASI 0.3, what a wasm32-wasip3 build links (wit/host's world).
        // A `wasi:http` request goes through [`Outgoing`].
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        wh::types::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::secrets::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::http::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::io::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::ssh::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::git::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::log::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        Ok(linker)
    }

    fn state(&self) -> Result<State> {
        let mut wasi = WasiCtx::builder();
        wasi.inherit_stdout().inherit_stderr();
        if self.grants.allow.contains("wasi:filesystem") {
            wasi.preopened_dir("/", "/", FsPerms::ReadWrite)
                .map_err(anyhow::Error::from)
                .context("preopen / for wasi:filesystem")?;
            // A relative path (`--world w.json`) is dform's working
            // directory's, as for a native provider (the SDK changes to
            // it: wasi-libc starts at `/`).
            if let Ok(cwd) = std::env::current_dir() {
                wasi.initial_cwd(cwd.display().to_string());
            }
        }
        if self.grants.allow.contains("wasi:sockets") {
            wasi.inherit_network().allow_ip_name_lookup(true);
        } else {
            wasi.allow_tcp(false)
                .allow_udp(false)
                .allow_ip_name_lookup(false);
        }
        let host = Services::paced(self.grants.clone(), self.limiter.clone())
            .with_signer(self.signer.clone());
        let host = Arc::new(host);
        Ok(State {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            outgoing: Outgoing(host.clone()),
            host,
            exports: Default::default(),
        })
    }

    /// The instance's thread: a runtime of its own, serving it there
    /// ([`Instance::serve`]).
    fn run(
        self,
        engine: &'static Engine,
        component: Component,
        calls: tokio::sync::mpsc::UnboundedReceiver<Submitted>,
        ready: std::sync::mpsc::Sender<Result<()>>,
    ) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready.send(Err(anyhow!("start its runtime: {e}")));
                return;
            }
        };
        rt.block_on(self.serve(engine, component, calls, ready));
    }

    /// Instantiate, say so on `ready`, then run every call `calls` brings
    /// at once, until dform drops the provider or the instance traps.
    async fn serve(
        mut self,
        engine: &'static Engine,
        component: Component,
        calls: tokio::sync::mpsc::UnboundedReceiver<Submitted>,
        ready: std::sync::mpsc::Sender<Result<()>>,
    ) {
        // Let go of the start's span once instantiated: held for the
        // instance's life, it would end only as the run exits, after its
        // trace is written.
        let within = std::mem::replace(&mut self.started_in, tracing::Span::none());
        let started = self.instantiate(engine, &component).instrument(within);
        let (store, provider) = match started.await {
            Ok(s) => {
                let _ = ready.send(Ok(()));
                s
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        self.answer_all(store, provider, calls).await;
    }

    /// The component linked and instantiated in a store of its own, its
    /// calls' epoch deadline set: a `wasm.instantiate` span, after its
    /// `wasm.link`.
    async fn instantiate(
        &self,
        engine: &'static Engine,
        component: &Component,
    ) -> Result<(Store<State>, bindings::HostedProvider)> {
        let linked = self.link(engine, component)?;
        self.instance_of(engine, &linked).await
    }

    /// The component's imports resolved against the host's: a
    /// `wasm.link` span.
    #[tracing::instrument(target = "dform", level = "debug", name = "wasm.link", skip_all)]
    fn link(
        &self,
        engine: &'static Engine,
        component: &Component,
    ) -> Result<bindings::HostedProviderPre<State>> {
        let pre = self.linker(engine)?.instantiate_pre(component)?;
        Ok(bindings::HostedProviderPre::new(pre)?)
    }

    /// An instance of the component `linked`, in a store of its own: a
    /// `wasm.instantiate` span.
    #[tracing::instrument(target = "dform", level = "debug", name = "wasm.instantiate", skip_all)]
    async fn instance_of(
        &self,
        engine: &'static Engine,
        linked: &bindings::HostedProviderPre<State>,
    ) -> Result<(Store<State>, bindings::HostedProvider)> {
        let mut store = Store::new(engine, self.state()?);
        // Checked each tick the component runs its own code: past the
        // deadline, or stopped, it is interrupted.
        let (stop, until) = (self.stop.clone(), Instant::now() + DEADLINE);
        store.epoch_deadline_callback(move |_| {
            Ok(match stop.stopped() || Instant::now() >= until {
                true => UpdateDeadline::Interrupt,
                false => UpdateDeadline::Continue(1),
            })
        });
        store.set_epoch_deadline(1);
        let provider = linked.instantiate_async(&mut store).await?;
        Ok((store, provider))
    }

    /// Run every call `calls` brings at once, until dform drops the
    /// provider (`calls` ends), stops it ([`Instance::cancelled`]) or the
    /// instance traps; then answer every call left as crashed
    /// ([`Instance::died`]).
    async fn answer_all(
        &self,
        mut store: Store<State>,
        provider: bindings::HostedProvider,
        mut calls: tokio::sync::mpsc::UnboundedReceiver<Submitted>,
    ) {
        let who = std::cell::RefCell::new(self.program.clone());
        // The calls not answered yet, each with its span: each answered as
        // crashed if the instance traps.
        let open = std::cell::RefCell::new(BTreeMap::new());
        let ran = store
            .run_concurrent(async |a| self.run_calls(a, &provider, &who, &open, &mut calls).await)
            .await;
        let trapped = match ran {
            _ if self.stop.stopped() => {
                return self.cancelled(&who.into_inner(), open.into_inner());
            }
            Ok(Ended::Dropped) => return,
            Ok(Ended::Stopped) => unreachable!("stopped is checked first"),
            Ok(Ended::Trapped(trapped)) => Some(trapped),
            Err(trap) => {
                let t = open.borrow().keys().next().copied();
                t.map(|t| (t, false, trap))
            }
        };
        self.died(&who.into_inner(), trapped, open.into_inner(), calls)
            .await;
    }

    /// The run's end stopped the instance: each call still `open` answered
    /// as cancelled (an Apply's as one that may have taken effect), its
    /// span ended with the cancellation on it, and no call taken again.
    fn cancelled(&self, who: &str, open: BTreeMap<Ticket, Open>) {
        let why = format!("the provider {who} was stopped as the run ended");
        *self.dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(why.clone());
        for (t, Open { span, apply }) in open {
            span.record("error.type", "cancelled");
            let e = match apply {
                true => CallError::MaybeApplied(why.clone()),
                false => CallError::Refused(why.clone()),
            };
            let _ = self.tx.send(Back::Answer(t, Box::new(Err(e))));
        }
    }

    /// Each call `calls` brings run as it comes, all at once, in its
    /// span ([`call_span`]), each noted in `open` until it is
    /// answered, until `calls` ends, the instance is stopped, or a call
    /// traps (still `open`).
    async fn run_calls(
        &self,
        a: &Accessor<State>,
        provider: &bindings::HostedProvider,
        who: &std::cell::RefCell<String>,
        open: &std::cell::RefCell<BTreeMap<Ticket, Open>>,
        calls: &mut tokio::sync::mpsc::UnboundedReceiver<Submitted>,
    ) -> Ended {
        type Running<'a> = Pin<Box<dyn Future<Output = Ran> + 'a>>;
        let mut running: Vec<Running> = Vec::new();
        loop {
            let next = std::future::poll_fn(|cx| {
                for i in 0..running.len() {
                    if let Poll::Ready(r) = running[i].as_mut().poll(cx) {
                        drop(running.swap_remove(i));
                        return Poll::Ready(r);
                    }
                }
                Poll::Pending
            });
            tokio::select! {
                () = self.stop.wake.notified() => return Ended::Stopped,
                got = calls.recv() => match got {
                    Some(c) => {
                        let span = call_span(&c);
                        let apply = matches!(c.call, Call::Apply(_));
                        open.borrow_mut().insert(c.t, Open { span: span.clone(), apply });
                        running.push(Box::pin(self.one(a, provider, who, c).instrument(span)));
                    }
                    // dform dropped the provider.
                    None => return Ended::Dropped,
                },
                Ran { t, apply, answer } = next => {
                    let answer = match answer {
                        Ok(answer) => answer,
                        Err(trap) => return Ended::Trapped((t, apply, trap)),
                    };
                    // The instance lets go of its span before its answer
                    // is told: the span ends as the run takes it.
                    open.borrow_mut().remove(&t);
                    let _ = self.tx.send(Back::Answer(t, Box::new(answer)));
                }
            }
        }
    }

    /// The instance trapped (in the call `trapped` names, when one did):
    /// that call answered with its trap, recorded on its span,
    /// every call still `open` and every call submitted since as crashed,
    /// and no call taken again.
    async fn died(
        &self,
        who: &str,
        trapped: Option<(Ticket, bool, wasmtime::Error)>,
        mut open: BTreeMap<Ticket, Open>,
        mut calls: tokio::sync::mpsc::UnboundedReceiver<Submitted>,
    ) {
        if let Some((t, apply, trap)) = trapped {
            if let Some(o) = open.remove(&t) {
                Trapped::of(&trap).record(&o.span);
            }
            let _ = self.tx.send(Back::Answer(
                t,
                Box::new(Err(trapped_as(who, apply, &trap))),
            ));
        }
        let gone = format!("the provider {who} has exited");
        *self.dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(gone.clone());
        let crashed = |t| Back::Answer(t, Box::new(Err(CallError::Crashed(gone.clone()))));
        for t in open.into_keys() {
            let _ = self.tx.send(crashed(t));
        }
        // A call submitted before the submitter saw it die.
        while let Some(Submitted { t, .. }) = calls.recv().await {
            let _ = self.tx.send(crashed(t));
        }
    }

    /// The export `f` called with `arg`, its task known by the span it is
    /// called in (the call's `provider.call`) while it runs
    /// ([`State::exports`]).
    async fn exported<P, R>(
        a: &Accessor<State>,
        f: TypedFunc<(P,), (R,)>,
        arg: P,
    ) -> wasmtime::Result<R>
    where
        (P,): ComponentNamedList + Lower + Send + Sync + 'static,
        (R,): ComponentNamedList + Lift + Send + Sync + 'static,
    {
        let started = a.with(|mut s| f.start_call_concurrent(s.as_context_mut(), (arg,)))?;
        let task = started.task();
        Instance::traced(a, task, Some(tracing::Span::current()));
        let answered = f.finish_call_concurrent(a, started).await;
        Instance::traced(a, task, None);
        Ok(answered?.0)
    }

    /// The export's task `task` known by the span `span`, or (`None`) no
    /// longer.
    fn traced(a: &Accessor<State>, task: GuestTaskId, span: Option<tracing::Span>) {
        a.with(|mut s| {
            let exports = &mut s.get().exports;
            match span {
                Some(span) => exports.insert(task, span),
                None => exports.remove(&task),
            }
        });
    }

    /// Run `call`, telling its events, how long it took in the provider
    /// recorded on the span it runs in (its call's, `provider.took`, in
    /// ms): its answer, or the trap that stopped it, which
    /// [`Instance::run_calls`] tells once the instance let go of the span.
    async fn one(
        &self,
        a: &Accessor<State>,
        provider: &bindings::HostedProvider,
        who: &std::cell::RefCell<String>,
        Submitted { t, call, .. }: Submitted,
    ) -> Ran {
        let apply = matches!(call, Call::Apply(_));
        let began = std::time::Instant::now();
        let answer = self.answer(a, provider, who, t, &call).await;
        let took = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::Span::current().record("provider.took", took);
        if let Ok(Ok(Reply::Handshake(h))) = &answer {
            *who.borrow_mut() = h.name.clone();
        }
        Ran { t, apply, answer }
    }

    /// `call`'s answer, from the export that serves it; a trap is the
    /// error.
    async fn answer(
        &self,
        a: &Accessor<State>,
        provider: &bindings::HostedProvider,
        who: &std::cell::RefCell<String>,
        t: Ticket,
        call: &Call,
    ) -> wasmtime::Result<Result<Reply, CallError>> {
        let p = provider.dform_provider_provider();
        Ok(match call {
            Call::Handshake(r) => answered(
                Instance::exported(a, p.func_handshake(), conv::to_handshake_request(r)).await?,
                conv::from_handshake_response,
                Reply::Handshake,
            ),
            Call::Configure(r) => answered(
                Instance::exported(a, p.func_configure(), conv::to_configure_request(r)).await?,
                conv::from_configure_response,
                Reply::Configure,
            ),
            Call::Schema(r) => answered(
                Instance::exported(a, p.func_schema(), conv::to_schema_request(r)).await?,
                conv::from_schema_response,
                Reply::Schema,
            ),
            Call::Query(r) => answered(
                Instance::exported(a, p.func_query(), conv::to_query_request(r)).await?,
                |rows: &Vec<wp::Row>| conv::from_rows(rows),
                Reply::Query,
            ),
            Call::Read(r) => answered(
                Instance::exported(a, p.func_read(), conv::to_read_request(r)).await?,
                conv::from_read_response,
                Reply::Read,
            ),
            Call::Plan(r) => answered(
                Instance::exported(a, p.func_plan(), conv::to_plan_request(r)).await?,
                conv::from_plan_response,
                Reply::Plan,
            ),
            Call::Apply(r) if r.op == pb::Op::Unspecified as i32 => {
                Err(CallError::Refused("an Apply call with no op".to_string()))
            }
            Call::Apply(r) => return self.apply(a, p, who, t, r).await,
            Call::Import(r) => answered(
                Instance::exported(a, p.func_import(), conv::to_import_request(r)).await?,
                conv::from_import_response,
                Reply::Import,
            ),
            Call::Reveal(r) => answered(
                Instance::exported(a, p.func_reveal(), conv::to_reveal_request(r)).await?,
                conv::from_reveal_response,
                Reply::Reveal,
            ),
            Call::Health(r) => answered(
                Instance::exported(a, p.func_health(), conv::to_health_request(r)).await?,
                conv::from_health_response,
                Reply::Health,
            ),
        })
    }

    /// An Apply's answer: its events told as the component writes them,
    /// then its result once it resolves its future.
    async fn apply(
        &self,
        a: &Accessor<State>,
        p: &bindings::exports::dform::provider::provider::Guest,
        who: &std::cell::RefCell<String>,
        t: Ticket,
        r: &pb::ApplyRequest,
    ) -> wasmtime::Result<Result<Reply, CallError>> {
        let f = p.func_apply();
        let started = a.with(|mut s| {
            f.start_call_concurrent(s.as_context_mut(), (conv::to_apply_request(r),))
        })?;
        let task = started.task();
        Instance::traced(a, task, Some(tracing::Span::current()));
        let (events, result) = f.finish_call_concurrent(a, started).await?.0;
        let (done, answer) = tokio::sync::oneshot::channel();
        a.with(|mut s| -> wasmtime::Result<()> {
            events.pipe(
                &mut s,
                Events {
                    t,
                    tx: self.tx.clone(),
                },
            )?;
            result.pipe(&mut s, Done(Some(done)))?;
            Ok(())
        })?;
        let answered = answer.await;
        Instance::traced(a, task, None);
        Ok(match answered {
            Ok(r) => r
                .map_err(conv::from_call_error)
                .and_then(|r| decode(conv::from_apply_response(&r)).map(Reply::Apply)),
            Err(_) => Err(CallError::Crashed(format!(
                "the provider {} dropped its Apply's answer",
                who.borrow()
            ))),
        })
    }
}

/// A call in flight in the instance: the span it runs in, and whether it
/// is an Apply.
struct Open {
    span: tracing::Span,
    apply: bool,
}

/// Why the instance stopped taking calls.
enum Ended {
    /// dform dropped the provider.
    Dropped,
    /// The run's end stopped it ([`Wasm::stop`]).
    Stopped,
    /// A call trapped: its ticket, whether it was an Apply, the trap.
    Trapped((Ticket, bool, wasmtime::Error)),
}

/// A call's run in the instance: its ticket, whether it was an Apply,
/// and its answer, or the trap that stopped the instance.
struct Ran {
    t: Ticket,
    apply: bool,
    answer: wasmtime::Result<Result<Reply, CallError>>,
}

/// An export's answer as the engine takes it: the component's error as
/// the call's, its value decoded by `from` and made the reply `reply`.
fn answered<W, T>(
    r: Result<W, wp::CallError>,
    from: impl FnOnce(&W) -> std::result::Result<T, String>,
    reply: fn(T) -> Reply,
) -> Result<Reply, CallError> {
    r.map_err(conv::from_call_error)
        .and_then(|w| decode(from(&w)).map(reply))
}

/// How a trap stopped the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trapped {
    /// It exited, with this status.
    Exit(i32),
    /// It ran past its epoch deadline ([`DEADLINE`]).
    Deadline,
    /// Any other trap: a crash.
    Crash,
}

impl Trapped {
    fn of(trap: &wasmtime::Error) -> Trapped {
        if let Some(exit) = trap.downcast_ref::<wasmtime_wasi::I32Exit>() {
            return Trapped::Exit(exit.0);
        }
        match trap.downcast_ref::<wasmtime::Trap>() {
            Some(wasmtime::Trap::Interrupt) => Trapped::Deadline,
            _ => Trapped::Crash,
        }
    }

    /// As a call's span's `error.type` names it.
    fn as_str(self) -> &'static str {
        match self {
            Trapped::Exit(_) => "exit",
            Trapped::Deadline => "deadline",
            Trapped::Crash => "trap",
        }
    }

    /// The trap on the span of the call it stopped: an error of its type.
    fn record(self, span: &tracing::Span) {
        span.record("error.type", self.as_str());
    }
}

/// The span a call runs in in the component: its `provider.call` (by
/// the call's trace number), which the run ends as it takes the answer.
/// None when the call is not traced.
fn call_span(call: &Submitted) -> tracing::Span {
    call.number
        .and_then(timed::call_span)
        .unwrap_or_else(tracing::Span::none)
}

/// A trap of the provider `who` in a call, as the call's failure: an exit
/// or a crash is `Crashed`; past its deadline, an Apply may have taken
/// effect, another call is refused.
fn trapped_as(who: &str, apply: bool, trap: &wasmtime::Error) -> CallError {
    let trapped = Trapped::of(trap);
    let why = match trapped {
        Trapped::Exit(code) => {
            format!("the provider {who} exited during the call (exit status: {code})")
        }
        Trapped::Deadline => format!(
            "the provider {who} ran past its deadline ({}s) and was stopped",
            DEADLINE.as_secs()
        ),
        Trapped::Crash => format!("the provider {who} trapped: {trap:#}"),
    };
    match (trapped, apply) {
        (Trapped::Deadline, true) => CallError::MaybeApplied(why),
        (Trapped::Deadline, false) => CallError::Refused(why),
        (Trapped::Exit(_) | Trapped::Crash, _) => CallError::Crashed(why),
    }
}

/// An Apply's events, as the component writes them, told with its ticket.
struct Events {
    t: Ticket,
    tx: std::sync::mpsc::Sender<Back>,
}

impl<D> StreamConsumer<D> for Events {
    type Item = wp::Event;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, wp::Event>,
        _: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        while source.remaining(store.as_context_mut()) > 0 {
            let mut one = None;
            source.read(store.as_context_mut(), &mut one)?;
            let Some(e) = one else { break };
            if self
                .tx
                .send(Back::Event(self.t, conv::from_event(&e)))
                .is_err()
            {
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// An Apply's result, as the component resolves its future.
struct Done(Option<tokio::sync::oneshot::Sender<Result<wp::ApplyResponse, wp::CallError>>>);

impl<D> FutureConsumer<D> for Done {
    type Item = Result<wp::ApplyResponse, wp::CallError>;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut v = None;
        source.read(store, &mut v)?;
        if let (Some(v), Some(tx)) = (v, self.0.take()) {
            let _ = tx.send(v);
        }
        Poll::Ready(Ok(()))
    }
}

fn ticks(d: Duration) -> u64 {
    (d.as_millis() / TICK.as_millis()).max(1) as u64
}

/// An answer the WIT could carry and the proto cannot: a malformed tree.
fn decode<T>(r: std::result::Result<T, String>) -> Result<T, CallError> {
    r.map_err(|e| CallError::Refused(format!("the provider answered a malformed value: {e}")))
}

impl Provider for Wasm {
    fn submit(&mut self, call: Call) -> Ticket {
        let t = Ticket(self.next);
        self.next += 1;
        let dead = self.dead.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let sent = dead.is_none()
            && self.calls.as_ref().is_some_and(|c| {
                let number = dform_core::plugin::backend::Within::here().call;
                c.send(Submitted { t, call, number }).is_ok()
            });
        if !sent {
            let why = dead.unwrap_or_else(|| "the provider has exited".into());
            self.refused.push((t, CallError::Crashed(why)));
        }
        t
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        if let Some((t, e)) = self.refused.pop() {
            return (t, Err(e));
        }
        loop {
            match self.back.recv() {
                Ok(Back::Event(t, e)) => events(t, e),
                Ok(Back::Answer(t, r)) => return (t, *r),
                Err(_) => {
                    panic!("internal: the wasm provider's thread is gone with a call in flight")
                }
            }
        }
    }

    fn is_dead(&mut self) -> bool {
        self.dead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The run's end, while a call is stuck in the instance: it is
    /// stopped ([`Wasm::stop`]'s first half), which answers the call and
    /// frees the thread waiting on it.
    fn stopper(&self) -> Option<dform_core::plugin::backend::Stop> {
        let stop = self.stop.clone();
        Some(Box::new(move || stop.stop()))
    }
}

impl Drop for Wasm {
    /// The instance stopped and its thread joined ([`Wasm::stop`]): a
    /// call still running in it is cancelled, its span ended.
    fn drop(&mut self) {
        self.stop();
    }
}

/// The component at `path`, started with `grants` and shaken hands with.
pub fn link(path: &Path, grants: Grants) -> Result<Link> {
    let limiter = Arc::new(Limiter::new(&grants.provider));
    let (w, manifest) = Wasm::start(path, grants.clone(), limiter.clone())?;
    let mut link = Link::start(path.display().to_string(), Box::new(Paced::new(w, limiter)))?;
    link.hosting = Some(Hosting {
        host: "wasm",
        manifest: Some(manifest),
        grants,
    });
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dform_core::http::fake::Server;
    use http_body_util::BodyExt;
    use wasmtime_wasi_http::WasiHttpHooks;

    /// A component's `wasi:http` requests go through its host's `send`:
    /// paced by its window (two at first contact, then wider as the
    /// server answers), kept-alive connections of the pool.
    #[test]
    fn a_components_wasi_http_requests_are_paced_by_its_host() {
        let server = Server::http1();
        server.slow(Duration::from_millis(200));
        let limiter = Arc::new(Limiter::new("component"));
        let host = Arc::new(Services::paced(Grants::none("component"), limiter.clone()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let statuses = rt.block_on(async {
            let sent: Vec<_> = (0..4)
                .map(|i| {
                    let req = http::Request::get(server.url(&format!("/{i}")))
                        .body(
                            http_body_util::Empty::new()
                                .map_err(wasmtime_wasi_http::Error::from)
                                .boxed_unsync(),
                        )
                        .unwrap();
                    let answered =
                        Outgoing(host.clone()).send_request(req, None, Box::new(async { Ok(()) }));
                    tokio::spawn(Box::into_pin(answered))
                })
                .collect();
            let mut statuses = Vec::new();
            for s in sent {
                let (resp, _) = s.await.unwrap().unwrap();
                statuses.push(resp.status().as_u16());
            }
            statuses
        });
        assert_eq!(statuses, [200; 4]);
        assert_eq!(server.most_at_once(), 2, "the window at first contact");
        assert!(
            server.connections() <= 2,
            "kept alive: {}",
            server.connections()
        );
        assert!(limiter.window(Session::NONE, None).0 > 2, "it grew");
    }
}
