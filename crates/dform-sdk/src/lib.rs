//! The dform provider SDK (R-25, R-13b).
//!
//! A provider written against it does protocol logic only and never
//! touches a socket or a credential: it calls the host
//! ([`host()`]`.http.request(..)`, `.ssh.exec(..)`, `.git.read(..)`,
//! `.secrets.open(..)`, `.log.info(..)`), and dform does the transport,
//! with TLS, the operator's credentials by name, retries and audit. The
//! same source builds two ways:
//!
//! * natively (`cargo build`): an executable dform starts, serving the
//!   protocol over gRPC and calling the host back over the `Host` service
//!   named in its environment (`DFORM_HOST`);
//! * as a component (`cargo build --target wasm32-wasip2 --features
//!   component`, the crate a `cdylib`): dform's wasm host runs it, and the
//!   host's interfaces are its imports. wasip2 is the target that exists on
//!   stable; wasip3 is the intent and changes nothing here.
//!
//! [`provider!`] is the entry point for either. A provider is a
//! [`Handler`] (the protocol's calls, one at a time); [`typed::Typed`]
//! makes one from Rust types: `#[derive(Resource)]` for the schema,
//! [`typed::Lifecycle`] for read, create, update and delete, plan derived
//! from the schema. docs/providers.md is the guide.

pub use dform_core::plugin::backend::{CallError, Handler};
pub use dform_core::plugin::host::{
    Class, Endpoint, Error, Failure, GitFile, HttpResponse as Response, Level, Run, Target,
};
pub use dform_core::plugin::pb;
pub use dform_sdk_derive::Resource;

pub mod typed;
pub use typed::{Lifecycle, Provider, Resource, Typed};

#[cfg(not(target_family = "wasm"))]
pub mod native;

#[cfg(all(target_family = "wasm", feature = "component"))]
pub mod component;

use dform_core::plugin::host::{Calls, Handle, HttpRequest};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// The host this provider runs under.
static CALLS: OnceLock<Mutex<Box<dyn Calls>>> = OnceLock::new();

/// The transport's calls, connected on first use.
fn calls() -> MutexGuard<'static, Box<dyn Calls>> {
    CALLS
        .get_or_init(|| Mutex::new(connect()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(target_family = "wasm"))]
fn connect() -> Box<dyn Calls> {
    native::connect()
}

#[cfg(all(target_family = "wasm", feature = "component"))]
fn connect() -> Box<dyn Calls> {
    Box::new(component::Imports::default())
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
    fn open(&mut self, _: &str) -> Result<dform_core::plugin::host::Opened, Error> {
        Err(Error::fatal(&self.0))
    }
    fn send(
        &mut self,
        _: HttpRequest,
        _: Option<Handle>,
        _: Option<Handle>,
    ) -> Result<Response, Error> {
        Err(Error::fatal(&self.0))
    }
    fn exec(&mut self, _: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(Error::fatal(&self.0).into())
    }
    fn read(&mut self, _: &Target, _: &str) -> Result<Vec<u8>, Failure> {
        Err(Error::fatal(&self.0).into())
    }
    fn write(&mut self, _: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(Error::fatal(&self.0))
    }
    fn forward(&mut self, _: &Target, _: &Endpoint) -> Result<Handle, Error> {
        Err(Error::fatal(&self.0))
    }
    fn tunnel(&self, _: Handle) -> Option<Endpoint> {
        None
    }
    fn git_read(&mut self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, Error> {
        Err(Error::fatal(&self.0))
    }
    fn git_commit(&mut self, _: &str, _: &str, _: Vec<GitFile>, _: &str) -> Result<String, Error> {
        Err(Error::fatal(&self.0))
    }
    fn log(&mut self, level: Level, message: &str) {
        eprintln!("{level:?}: {message}");
    }
}

/// The host's interfaces.
pub struct Host {
    pub http: Http,
    pub ssh: Ssh,
    pub git: Git,
    pub secrets: Secrets,
    pub log: Log,
}

/// The host this provider runs under.
pub fn host() -> &'static Host {
    static HOST: Host = Host {
        http: Http(()),
        ssh: Ssh(()),
        git: Git(()),
        secrets: Secrets(()),
        log: Log(()),
    };
    &HOST
}

/// A credential the operator granted, by name: the host applies it; its
/// value never reaches the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    handle: Handle,
    name: String,
    endpoint: Option<String>,
}

impl Credential {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it is for, when it says (a kubeconfig's server).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
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
    /// The credential `name` (`KIND:NAME`), as dform.toml grants it
    /// (`[providers.NAME] credentials`); refused, naming both, when not.
    pub fn open(&self, name: &str) -> Result<Credential, Error> {
        let o = calls().open(name)?;
        Ok(Credential {
            handle: o.handle,
            name: name.to_string(),
            endpoint: o.endpoint,
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
}

/// `dform:host/http`.
pub struct Http(());

impl Http {
    /// Send `req`; a status the server answered is a response, not an
    /// error.
    pub fn request(&self, req: Request) -> Result<Response, Error> {
        calls().send(req.req, req.auth, req.via)
    }
}

/// `dform:host/ssh`, keys from the operator's agent.
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

    pub fn read(&self, on: &Target, path: &str) -> Result<Vec<u8>, Failure> {
        calls().read(on, path)
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

/// `dform:host/git`.
pub struct Git(());

impl Git {
    pub fn read(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error> {
        calls().git_read(repo, rev, path)
    }

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

            fn make() -> ::std::boxed::Box<dyn $crate::Handler> {
                ::std::boxed::Box::new($make)
            }

            struct Exported;

            impl $crate::component::bindings::exports::dform::provider::provider::Guest for Exported {
                fn handshake(r: t::HandshakeRequest) -> ::std::result::Result<t::HandshakeResponse, t::CallError> {
                    $crate::component::handshake(make, r)
                }
                fn configure(r: t::ConfigureRequest) -> ::std::result::Result<t::ConfigureResponse, t::CallError> {
                    $crate::component::configure(make, r)
                }
                fn schema(r: t::SchemaRequest) -> ::std::result::Result<t::SchemaResponse, t::CallError> {
                    $crate::component::schema(make, r)
                }
                fn query(r: t::QueryRequest) -> ::std::result::Result<::std::vec::Vec<t::Row>, t::CallError> {
                    $crate::component::query(make, r)
                }
                fn read(r: t::ReadRequest) -> ::std::result::Result<t::ReadResponse, t::CallError> {
                    $crate::component::read(make, r)
                }
                fn plan(r: t::PlanRequest) -> ::std::result::Result<t::PlanResponse, t::CallError> {
                    $crate::component::plan(make, r)
                }
                fn apply(r: t::ApplyRequest) -> ::std::result::Result<t::ApplyResponse, t::CallError> {
                    $crate::component::apply(make, r)
                }
                fn import(r: t::ImportRequest) -> ::std::result::Result<t::ImportResponse, t::CallError> {
                    $crate::component::import(make, r)
                }
            }

            $crate::component::bindings::export_provider!(Exported with_types_in $crate::component::bindings);
        }
    };
}
