//! The dform provider SDK (R-25, R-13b).
//!
//! A provider written against it does protocol logic only and never
//! touches a socket or a credential: it calls the host
//! ([`host()`]`.http.request(..)`, `.io.read(..)`, `.ssh.exec(..)`,
//! `.secrets.open(..)`, `.log.info(..)`), and dform does the transport,
//! with TLS, the operator's credentials by name, retries and audit. The
//! same source builds two ways:
//!
//! * natively (`cargo build`): an executable dform starts, serving the
//!   protocol over gRPC and calling the host back over the `Host` service
//!   named in its environment (`DFORM_HOST`);
//! * as a component (`cargo build --target wasm32-wasip3 --features
//!   component`, the crate a `cdylib`): dform's wasm host runs it, and the
//!   host's interfaces are its imports (the component model's async ABI).
//!
//! The host's calls that wait on the world ([`Http::request`],
//! [`Io::read`], [`Ssh::exec`], [`sleep`]) are `async`: a [`Handler`]
//! awaits them, one at a time as straight-line code (request, parse,
//! request) or several at once with a join ([`join`], [`join_all`]).
//! Natively each is a thread of its own while it waits; in a component
//! each is a subtask of the host's import, and the instance answers its
//! other calls meanwhile.
//!
//! A provider is a worker and dform its runtime (docs/providers.md, "The
//! host does the transport"): the host owns the connections, the pace,
//! the credentials and the trace, so a provider sends through
//! [`Http`], never a client of its own, names its limits
//! ([`Request::bucket`]) and no rates, and classifies its errors rather
//! than retrying them.
//!
//! [`provider!`] is the entry point for either. A provider is a
//! [`Handler`] (the protocol's calls, answered as dform sends them);
//! [`typed::Typed`] makes one from Rust types: `#[derive(Resource)]` for
//! the schema, [`typed::Lifecycle`] for read, create, update and delete,
//! plan derived from the schema. docs/providers.md is the guide.

pub use dform_core::files::Document;
pub use dform_core::plugin::backend::{CallError, health};
pub use dform_core::plugin::host::{
    Calls, Class, Endpoint, Error, Failure, GitFile, HttpResponse as Response, Level, Run, Scope,
    Target,
};
pub use dform_core::plugin::pb;
pub use dform_sdk_derive::Resource;

pub mod typed;
pub use typed::{Lifecycle, Progress, Provider, Resource, Typed};

#[cfg(not(target_family = "wasm"))]
pub mod native;

#[cfg(all(target_family = "wasm", feature = "component"))]
pub mod component;

/// Several of the host's calls at once (any futures: requests, reads,
/// execs, or a chain of each), their answers together: `join(a, b)`,
/// `join3`, and `join_all` for any number, in their order.
pub use futures::future::{join, join_all, join3};

/// A lock a provider holds across the host's calls (a token being
/// minted, a list being asked once): the waiting call awaits it. Never
/// `std::sync::Mutex` across an `.await`: in a component the instance's
/// calls take turns on one thread, and one waiting on a lock another
/// holds never lets that other finish. In a component the call letting
/// go wakes the waiting one's task from its own, which wit-bindgen allows
/// with its `inter-task-wakeup` feature (this crate's, and dform-wit's
/// guest side): without it the instance traps.
pub mod sync {
    pub use futures::lock::{Mutex, MutexGuard};
}

use dform_core::plugin::backend::{self, Call, Reply, Session, Within};
use dform_core::plugin::host::{CacheAt, Handle, HttpRequest};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A provider: the protocol's calls, each answered as dform sends it,
/// several at once (a session's, and several sessions'). Its answers may
/// await the host ([`host()`]): natively each call is served on a thread
/// of its own, and in a component the instance answers its other calls
/// while one awaits.
///
/// A [`backend::Handler`] (a synchronous one, which awaits nothing) is
/// one as it is.
#[allow(async_fn_in_trait)] // a call's future is polled where it is served, never sent
pub trait Handler: Send + Sync + 'static {
    /// Answer `call`, made in `session`, saying how it goes on `progress`
    /// while it runs. A Configure's session is the one it configures (a new
    /// one when it opens one: its answer names it, whatever the handler
    /// answers); a Handshake's is none.
    async fn handle(
        &self,
        session: Session,
        call: Call,
        progress: backend::Progress<'_>,
    ) -> Result<Reply, CallError>;

    /// Whether the provider is gone (it crashed).
    fn is_dead(&self) -> bool {
        false
    }

    /// The location schemes it reads (R-153): its manifest declares them,
    /// and dform routes a read of one to [`Handler::read_versioned`].
    fn schemes(&self) -> Vec<String> {
        Vec::new()
    }

    /// The document at `location`, of a scheme it declares, with its
    /// version when the source keeps versions (R-172); not there yet is
    /// `Failure::NotYet`.
    async fn read_versioned(&self, location: &str) -> Result<Document, Failure> {
        Err(Error::fatal(format!("{location}: this provider reads no location")).into())
    }

    /// Its signer, when its API wants each request signed with a secret
    /// (`dform_core::plugin::signer`): run by dform apart from it, which
    /// holds what the signer makes. Natively it is served beside the
    /// provider (its manifest says it signs); a wasm provider's is a
    /// component of its own (`world signer-component`), which the
    /// provider's carries (`dform_core::plugin::bundle`).
    fn signer(
        &self,
    ) -> Option<std::sync::Arc<dyn dform_core::plugin::signer::Signer + Send + Sync>> {
        None
    }
}

impl<H: backend::Handler + Send + Sync + 'static> Handler for H {
    async fn handle(
        &self,
        session: Session,
        call: Call,
        progress: backend::Progress<'_>,
    ) -> Result<Reply, CallError> {
        backend::Handler::handle(self, session, call, progress)
    }

    fn is_dead(&self) -> bool {
        backend::Handler::is_dead(self)
    }

    fn schemes(&self) -> Vec<String> {
        backend::Handler::schemes(self)
    }

    async fn read_versioned(&self, location: &str) -> Result<Document, Failure> {
        backend::Handler::read_versioned(self, location)
    }
}

/// Wait on `future` on this thread: for code that is not async (a
/// synchronous [`backend::Handler`]) calling the host. Natively the
/// thread parks until it is answered; in a component the instance waits
/// with it, its other calls too (docs/providers.md).
pub fn block_on<T: 'static>(future: impl Future<Output = T>) -> T {
    #[cfg(not(target_family = "wasm"))]
    return native::block_on(future);
    #[cfg(all(target_family = "wasm", feature = "component"))]
    return wit_bindgen::block_on(future);
    #[cfg(all(target_family = "wasm", not(feature = "component")))]
    absent::block_on(future)
}

/// Wait `d`: a poll's interval, a backoff. Natively on a thread of its
/// own; in a component `wasi:clocks`' `wait-for`, the instance answering
/// its other calls meanwhile.
pub async fn sleep(d: Duration) {
    waits::sleep(d).await
}

// How this build waits on the host: natively a thread per call, in a
// component the import's subtask, built for wasm without the `component`
// feature at once (there is no host to wait on).
#[cfg(not(target_family = "wasm"))]
use native::waits;

#[cfg(all(target_family = "wasm", feature = "component"))]
use component::waits;

#[cfg(all(target_family = "wasm", not(feature = "component")))]
use absent as waits;

/// Built for wasm without the `component` feature: no host, every call
/// answered at once with why.
#[cfg(all(target_family = "wasm", not(feature = "component")))]
mod absent {
    use super::*;

    pub fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(t) = future.as_mut().poll(&mut cx) {
                return t;
            }
        }
    }

    pub async fn send(
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<Response, Error> {
        calls().send(req, auth, via)
    }

    pub async fn io_read(location: String) -> Result<Vec<u8>, Failure> {
        calls().io_read(&location)
    }

    pub async fn exec(
        on: Target,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Run, Failure> {
        calls().exec(&on, &argv, stdin.as_deref())
    }

    pub async fn sleep(_: Duration) {}
}

/// The host this provider runs under.
static CALLS: OnceLock<Box<dyn Calls>> = OnceLock::new();

/// The transport's calls, connected on first use. Handlers call it at
/// once: nothing here is locked across a call.
fn calls() -> &'static dyn Calls {
    CALLS.get_or_init(connect).as_ref()
}

/// This process's host is `calls`, in place of the one `DFORM_HOST`
/// names: a host in the same process, for a test that runs a provider's
/// code against a fake API through dform's own host
/// (`dform_host::services::Services`: its pool, its pacing). Before the
/// process's first call to the host; `false` when its host is set
/// already (an earlier `set_host`, or a call that connected it).
pub fn set_host(calls: Box<dyn Calls>) -> bool {
    CALLS.set(calls).is_ok()
}

#[cfg(not(target_family = "wasm"))]
fn connect() -> Box<dyn Calls> {
    native::connect()
}

#[cfg(all(target_family = "wasm", feature = "component"))]
fn connect() -> Box<dyn Calls> {
    Box::new(component::Imports)
}

#[cfg(all(target_family = "wasm", not(feature = "component")))]
fn connect() -> Box<dyn Calls> {
    Box::new(Absent(
        "this provider is built for wasm without the `component` feature".into(),
    ))
}

/// No host to call: every call fails saying why.
#[allow(dead_code)] // not on wasm with the `component` feature
pub(crate) struct Absent(pub String);

impl Calls for Absent {
    fn open(&self, _: &str) -> Result<dform_core::plugin::host::Opened, Error> {
        Err(Error::fatal(&self.0))
    }
    fn send(
        &self,
        _: HttpRequest,
        _: Option<Handle>,
        _: Option<Handle>,
    ) -> Result<Response, Error> {
        Err(Error::fatal(&self.0))
    }
    fn io_read(&self, _: &str) -> Result<Vec<u8>, Failure> {
        Err(Error::fatal(&self.0).into())
    }
    fn exec(&self, _: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(Error::fatal(&self.0).into())
    }
    fn write(&self, _: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(Error::fatal(&self.0))
    }
    fn forward(&self, _: &Target, _: &Endpoint) -> Result<Handle, Error> {
        Err(Error::fatal(&self.0))
    }
    fn tunnel(&self, _: Handle) -> Option<Endpoint> {
        None
    }
    fn git_commit(&self, _: &str, _: &str, _: Vec<GitFile>, _: &str) -> Result<String, Error> {
        Err(Error::fatal(&self.0))
    }
    fn log(&self, level: Level, message: &str) {
        eprintln!("{level:?}: {message}");
    }
}

/// The host's interfaces.
pub struct Host {
    pub http: Http,
    pub io: Io,
    pub ssh: Ssh,
    pub git: Git,
    pub secrets: Secrets,
    pub cache: Cache,
    pub log: Log,
}

/// The host this provider runs under.
pub fn host() -> &'static Host {
    static HOST: Host = Host {
        http: Http(()),
        io: Io(()),
        ssh: Ssh(()),
        git: Git(()),
        secrets: Secrets(()),
        cache: Cache(()),
        log: Log(()),
    };
    &HOST
}

/// The account each session's first Configure named
/// (`dform_core::plugin::host::account_of`): what a native build's cache
/// calls name, its host having no other way to tell. A component's host
/// knows the call a cache call is made in, and takes nothing from here.
static ACCOUNTS: Mutex<BTreeMap<Session, String>> = Mutex::new(BTreeMap::new());

/// `call`, made in `session`, is about to be answered: a Configure's
/// account is its session's, when it is the session's first.
#[cfg_attr(target_family = "wasm", allow(dead_code))] // a component's host knows
pub(crate) fn configuring(session: Session, call: &Call) {
    let Call::Configure(r) = call else { return };
    let config = r
        .config
        .as_ref()
        .and_then(|c| dform_core::plugin::wire::from_doc(c).ok())
        .unwrap_or_default();
    ACCOUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session)
        .or_insert_with(|| dform_core::plugin::host::account_of(&config));
}

/// `dform:host/cache`: what the provider keeps from one call to the next,
/// kept by the host. The provider says what a thing is (its key, which
/// names what it is of: `openapi https://10.0.0.1:6443`) and how widely
/// it holds ([`Scope`]), never where, how much or how long: a get may
/// miss whatever was put, and the provider answers the same either way,
/// only slower (docs/providers.md, "The host's cache").
pub struct Cache(());

impl Cache {
    /// What is kept at `key` in `scope`, if anything.
    pub fn get(&self, scope: Scope, key: &str) -> Option<Vec<u8>> {
        calls().cache_get(&Cache::at(scope), key)
    }

    /// The text kept at `key` in `scope`, if any.
    pub fn get_text(&self, scope: Scope, key: &str) -> Option<String> {
        self.get(scope, key).and_then(|b| String::from_utf8(b).ok())
    }

    /// Keep `value` at `key` in `scope`, in place of what was.
    pub fn put(&self, scope: Scope, key: &str, value: impl Into<Vec<u8>>) {
        calls().cache_put(&Cache::at(scope), key, value.into())
    }

    /// Where a get or a put is made: in the call this is, its session's
    /// account.
    fn at(scope: Scope) -> CacheAt {
        let session = Within::here().session;
        let account = ACCOUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .cloned()
            .unwrap_or_default();
        CacheAt {
            scope,
            session,
            account,
        }
    }
}

/// A credential by name, held for the provider or granted by the
/// operator: the host applies it; its value never reaches the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    handle: Handle,
    name: String,
    endpoint: Option<String>,
    namespace: Option<String>,
}

impl Credential {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it is for, when it says (a kubeconfig's server).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// The namespace it defaults to, when it says (a kubeconfig
    /// context's).
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
}

/// A forwarded port: requests go `via` it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    handle: Handle,
    to: Endpoint,
}

impl Tunnel {
    pub fn to(&self) -> &Endpoint {
        &self.to
    }
}

/// `dform:host/secrets`.
pub struct Secrets(());

impl Secrets {
    /// The credential `name` (`KIND:NAME`): one held for this provider
    /// (the name a setting it declares a credential is configured with,
    /// [`typed::Typed::credential`]), else one dform.toml grants
    /// (`[providers.NAME] credentials`); refused, naming both, when not.
    pub fn open(&self, name: &str) -> Result<Credential, Error> {
        let o = calls().open(name)?;
        Ok(Credential {
            handle: o.handle,
            name: name.to_string(),
            endpoint: o.endpoint,
            namespace: o.namespace,
        })
    }
}

/// An HTTP request to send through the host.
#[derive(Debug, Clone, Default)]
pub struct Request {
    req: HttpRequest,
    auth: Option<Handle>,
    via: Option<Handle>,
}

impl Request {
    pub fn new(method: &str, url: impl Into<String>) -> Request {
        Request {
            req: HttpRequest {
                method: method.to_string(),
                url: url.into(),
                ..HttpRequest::default()
            },
            ..Request::default()
        }
    }

    pub fn get(url: impl Into<String>) -> Request {
        Request::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Request {
        Request::new("POST", url)
    }

    pub fn put(url: impl Into<String>) -> Request {
        Request::new("PUT", url)
    }

    pub fn delete(url: impl Into<String>) -> Request {
        Request::new("DELETE", url)
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.req.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Request {
        self.req.body = body.into();
        self
    }

    /// A JSON body, with its content type.
    pub fn json<T: serde::Serialize>(self, body: &T) -> Request {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        self.header("content-type", "application/json").body(bytes)
    }

    /// The host applies `c` (a header, a client certificate).
    pub fn auth(mut self, c: &Credential) -> Request {
        self.auth = Some(c.handle);
        self
    }

    /// Through the tunnel `t`.
    pub fn via(mut self, t: &Tunnel) -> Request {
        self.via = Some(t.handle);
        self
    }

    pub fn timeout(mut self, d: Duration) -> Request {
        self.req.timeout = Some(d);
        self
    }

    /// The limit it counts against, in the API's terms: an action
    /// (`"ec2.describe"`), a method, a verb and resource, or one an
    /// object has (`"route53.change:ZONE"`). dform paces each bucket by
    /// what the server answers it (a 429, `Retry-After`, a timeout), so a
    /// provider names its limits and declares no rates. None: the
    /// provider's default bucket.
    pub fn bucket(mut self, label: impl Into<String>) -> Request {
        self.req.bucket = Some(label.into());
        self
    }
}

/// `dform:host/http`: a provider's one way to the network. dform holds the
/// connection (one per endpoint, HTTP/2 where the server offers it),
/// paces the request in its bucket's window, applies the credential and
/// traces it under the call it is sent for.
pub struct Http(());

impl Http {
    /// Send `req`; a status the server answered is a response, not an
    /// error. It names the call it is sent for (`backend::Within`): its
    /// session, so dform paces each session's buckets apart, and its trace
    /// number, so the request's span is the call's child. Several are sent
    /// at once by awaiting them together ([`join_all`]): the host's window
    /// paces them, so a provider asks for what it needs (every document
    /// of an index, every object of a list) and dform decides how many go
    /// together.
    pub async fn request(&self, req: Request) -> Result<Response, Error> {
        let within = Within::here();
        let http = HttpRequest {
            session: within.session,
            call: within.call,
            ..req.req
        };
        waits::send(http, req.auth, req.via).await
    }
}

/// `dform:host/io` (R-153, R-155): a location read by the host, whatever
/// its scheme (`ssh://USER@HOST/PATH`, `https://..`,
/// `git+https://HOST/OWNER/REPO/PATH?ref=TAG`, `s3://BUCKET/KEY`), as
/// dform.toml grants it (`[providers.NAME] reads`). Read-only: a write is
/// the provider's apply.
pub struct Io(());

impl Io {
    /// The bytes at `location`; not there yet is `Failure::NotYet`.
    pub async fn read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        waits::io_read(location.to_string()).await
    }
}

/// `dform:host/ssh`, keys from the operator's agent. A host's file is
/// read through [`Io`].
pub struct Ssh(());

impl Ssh {
    /// Run `argv` (no shell) on `on`.
    pub async fn exec(&self, on: &Target, argv: &[&str]) -> Result<Run, Failure> {
        waits::exec(
            on.clone(),
            argv.iter().map(|s| s.to_string()).collect(),
            None,
        )
        .await
    }

    pub async fn exec_with_stdin(
        &self,
        on: &Target,
        argv: &[&str],
        stdin: &[u8],
    ) -> Result<Run, Failure> {
        let argv = argv.iter().map(|s| s.to_string()).collect();
        waits::exec(on.clone(), argv, Some(stdin.to_vec())).await
    }

    pub fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error> {
        calls().write(on, path, data, mode)
    }

    /// A tunnel through `via` to `host:port`, for [`Request::via`].
    pub fn forward(&self, via: &Target, host: &str, port: u16) -> Result<Tunnel, Error> {
        let to = Endpoint {
            host: host.to_string(),
            port,
        };
        let handle = calls().forward(via, &to)?;
        Ok(Tunnel { handle, to })
    }
}

/// `dform:host/git`. A repository's file is read through [`Io`].
pub struct Git(());

impl Git {
    pub fn commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error> {
        calls().git_commit(repo, branch, files, message)
    }
}

/// `dform:host/log`: shown with dform's own output.
pub struct Log(());

impl Log {
    pub fn log(&self, level: Level, message: &str) {
        calls().log(level, message)
    }

    pub fn debug(&self, message: &str) {
        self.log(Level::Debug, message)
    }

    pub fn info(&self, message: &str) {
        self.log(Level::Info, message)
    }

    pub fn warn(&self, message: &str) {
        self.log(Level::Warn, message)
    }

    pub fn error(&self, message: &str) {
        self.log(Level::Error, message)
    }
}

/// A provider's entry point, for either build: `provider!(EXPR)` where
/// EXPR makes the [`Handler`] (a [`Typed`] one, or any), optionally with
/// `uses = ["dform:host/http", ..]`, what a native build declares it uses
/// (a component's imports say it themselves).
///
/// Natively it defines `pub fn main() -> ExitCode`, which serves the
/// provider over gRPC (call it from `src/main.rs`); built for wasm with the
/// crate's `component` feature it exports the component's
/// `dform:provider/provider`, the crate a `cdylib`.
#[macro_export]
macro_rules! provider {
    ($make:expr $(, uses = [$($u:literal),* $(,)?])? $(,)?) => {
        /// Serve the provider over gRPC until stdin closes.
        #[cfg(not(target_family = "wasm"))]
        pub fn main() -> ::std::process::ExitCode {
            $crate::native::main(|| $make, &[$($($u),*)?])
        }

        /// A component has no `main`: dform's wasm host runs its library.
        #[cfg(target_family = "wasm")]
        pub fn main() -> ::std::process::ExitCode {
            eprintln!("this provider is a component: dform runs it (provider.wasm), not wasi:cli");
            ::std::process::ExitCode::FAILURE
        }

        #[cfg(all(target_family = "wasm", feature = "component"))]
        mod __dform_component {
            #[allow(unused_imports)]
            use super::*;
            use $crate::component::bindings::dform::provider::types as t;

            fn make() -> ::std::boxed::Box<dyn $crate::component::Serve> {
                ::std::boxed::Box::new($make)
            }

            struct Exported;

            impl $crate::component::bindings::exports::dform::provider::provider::Guest for Exported {
                async fn handshake(r: t::HandshakeRequest) -> ::std::result::Result<t::HandshakeResponse, t::CallError> {
                    $crate::component::handshake(make, r).await
                }
                async fn configure(r: t::ConfigureRequest) -> ::std::result::Result<t::ConfigureResponse, t::CallError> {
                    $crate::component::configure(make, r).await
                }
                async fn schema(r: t::SchemaRequest) -> ::std::result::Result<t::SchemaResponse, t::CallError> {
                    $crate::component::schema(make, r).await
                }
                async fn query(r: t::QueryRequest) -> ::std::result::Result<::std::vec::Vec<t::Row>, t::CallError> {
                    $crate::component::query(make, r).await
                }
                async fn read(r: t::ReadRequest) -> ::std::result::Result<t::ReadResponse, t::CallError> {
                    $crate::component::read(make, r).await
                }
                async fn plan(r: t::PlanRequest) -> ::std::result::Result<t::PlanResponse, t::CallError> {
                    $crate::component::plan(make, r).await
                }
                async fn apply(r: t::ApplyRequest) -> $crate::component::Applied {
                    $crate::component::apply(make, r).await
                }
                async fn import(r: t::ImportRequest) -> ::std::result::Result<t::ImportResponse, t::CallError> {
                    $crate::component::import(make, r).await
                }
                async fn reveal(r: t::RevealRequest) -> ::std::result::Result<t::RevealResponse, t::CallError> {
                    $crate::component::reveal(make, r).await
                }
                async fn health(r: t::HealthRequest) -> ::std::result::Result<t::HealthResponse, t::CallError> {
                    $crate::component::health(make, r).await
                }
            }

            $crate::component::bindings::export_provider!(Exported with_types_in $crate::component::bindings);
        }
    };
}
