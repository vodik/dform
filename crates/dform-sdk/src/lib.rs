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
//!   host's interfaces are its imports, those that wait on the world
//!   async (the component model's async ABI): [`Http::request_all`] keeps
//!   several requests in flight from one call with no thread.
//!
//! A provider is a worker and dform its runtime (docs/providers.md, "The
//! host does the transport"): the host owns the connections, the pace,
//! the credentials and the trace, so a provider sends through
//! [`Http`], never a client of its own, names its limits
//! ([`Request::bucket`]) and no rates, and classifies its errors rather
//! than retrying them.
//!
//! [`provider!`] is the entry point for either. A provider is a
//! [`Handler`] (the protocol's calls, answered at once as dform sends
//! them); [`typed::Typed`] makes one from Rust types: `#[derive(Resource)]`
//! for the schema, [`typed::Lifecycle`] for read, create, update and
//! delete, plan derived from the schema. docs/providers.md is the guide.

pub use dform_core::files::Document;
pub use dform_core::plugin::backend::{CallError, Handler, health};
pub use dform_core::plugin::host::{
    Calls, Class, Endpoint, Error, Failure, GitFile, HttpResponse as Response, Level, Run, Target,
};
pub use dform_core::plugin::pb;
pub use dform_sdk_derive::Resource;

pub mod typed;
pub use typed::{Lifecycle, Progress, Provider, Resource, Typed};

#[cfg(not(target_family = "wasm"))]
pub mod native;

#[cfg(all(target_family = "wasm", feature = "component"))]
pub mod component;

use dform_core::plugin::host::{Handle, HttpRequest};
use std::sync::OnceLock;
use std::time::Duration;

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
        log: Log(()),
    };
    &HOST
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
    /// number, so the request's span is the call's child (a thread of the
    /// provider's own names neither, unless it took the call along).
    pub fn request(&self, req: Request) -> Result<Response, Error> {
        let within = dform_core::plugin::backend::Within::here();
        let http = HttpRequest {
            session: within.session,
            call: within.call,
            ..req.req
        };
        calls().send(http, req.auth, req.via)
    }

    /// Send each of `reqs` at once; their answers in their order. Natively
    /// each on a thread of its own ([`at_once`]); in a component each a
    /// subtask of the host's `send`, all in flight from the one call with
    /// no thread. Either way the host's window paces them: a provider
    /// submits what it needs (every document of an index, every object
    /// of a list) and lets dform decide how many go together.
    pub fn request_all(
        &self,
        reqs: impl IntoIterator<Item = Request>,
    ) -> Vec<Result<Response, Error>> {
        send_all(self, reqs.into_iter().collect())
    }
}

#[cfg(not(target_family = "wasm"))]
fn send_all(http: &Http, reqs: Vec<Request>) -> Vec<Result<Response, Error>> {
    at_once(reqs.into_iter().map(|r| move || http.request(r)))
}

#[cfg(all(target_family = "wasm", feature = "component"))]
fn send_all(_: &Http, reqs: Vec<Request>) -> Vec<Result<Response, Error>> {
    let within = dform_core::plugin::backend::Within::here();
    component::send_all(
        reqs.into_iter()
            .map(|r| {
                let http = HttpRequest {
                    session: within.session,
                    call: within.call,
                    ..r.req
                };
                (http, r.auth, r.via)
            })
            .collect(),
    )
}

#[cfg(all(target_family = "wasm", not(feature = "component")))]
fn send_all(http: &Http, reqs: Vec<Request>) -> Vec<Result<Response, Error>> {
    reqs.into_iter().map(|r| http.request(r)).collect()
}

/// Each of `asks` at once, on a thread of its own in the call this thread
/// answers, so its requests are the call's (its session's windows, its
/// span: `backend::Within`); their answers in their order. A thread a
/// handler starts by itself is in no call.
pub fn at_once<T, F>(asks: impl IntoIterator<Item = F>) -> Vec<T>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    let within = dform_core::plugin::backend::Within::here();
    std::thread::scope(|scope| {
        let threads: Vec<_> = asks
            .into_iter()
            .map(|ask| {
                scope.spawn(move || {
                    let _in = within.enter();
                    ask()
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| match t.join() {
                Ok(answer) => answer,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    })
}

/// `dform:host/io` (R-153, R-155): a location read by the host, whatever
/// its scheme (`ssh://USER@HOST/PATH`, `https://..`,
/// `git+https://HOST/OWNER/REPO/PATH?ref=TAG`, `s3://BUCKET/KEY`), as
/// dform.toml grants it (`[providers.NAME] reads`). Read-only: a write is
/// the provider's apply.
pub struct Io(());

impl Io {
    /// The bytes at `location`; not there yet is `Failure::NotYet`.
    pub fn read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        calls().io_read(location)
    }
}

/// `dform:host/ssh`, keys from the operator's agent. A host's file is
/// read through [`Io`].
pub struct Ssh(());

impl Ssh {
    /// Run `argv` (no shell) on `on`.
    pub fn exec(&self, on: &Target, argv: &[&str]) -> Result<Run, Failure> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        calls().exec(on, &argv, None)
    }

    pub fn exec_with_stdin(
        &self,
        on: &Target,
        argv: &[&str],
        stdin: &[u8],
    ) -> Result<Run, Failure> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        calls().exec(on, &argv, Some(stdin))
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

            fn make() -> ::std::boxed::Box<dyn $crate::Handler + ::std::marker::Send + ::std::marker::Sync> {
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
