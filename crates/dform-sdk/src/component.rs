//! A provider built as a component (`--target wasm32-wasip3 --features
//! component`): `dform:host/hosted-provider`'s bindings, its exports
//! answered by the provider's [`Handler`] and its imports the host's
//! calls: those that wait on the world as the async ABI's subtasks
//! ([`waits`]), the rest as [`Imports`].
//!
//! What the instance keeps (the handler, its sessions, what it opened) is
//! in statics, never `thread_local!`s: on wasip3 every call of an export
//! is a task of its own, each with its own thread-locals, so a
//! thread-local is the call's. The tasks take turns where one awaits the
//! host, so nothing is held locked across an import.
//!
//! The exports are async (the component model's async ABI, R-130): each
//! awaits the handler's answer, and the instance answers its other calls
//! while one awaits the host. Apply answers at once with a stream of
//! events and a future of its result, and runs the handler in a task of
//! its own, writing each event to the stream as the handler says it. A
//! host that drops the stream (it cancelled the call) is written no more
//! events.

use crate::{CallError, Handler};
use dform_core::plugin::backend::{Call, Reply, Sessions, Within};
use dform_core::plugin::host::{
    self as h, Calls, Endpoint, Failure, GitFile, Handle, HttpRequest, HttpResponse, Level, Opened,
    Run, Target,
};
use dform_core::plugin::pb;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll};

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit/host",
        world: "dform:host/hosted-provider",
        generate_all,
        pub_export_macro: true,
        export_macro_name: "export_provider",
        default_bindings_module: "dform_sdk::component::bindings",
    });
}

dform_wit::convert!(conv, crate::component::bindings::dform::provider::types);

use bindings::dform::host as wh;
use bindings::dform::provider::types as t;

/// A [`Handler`] behind a pointer: its answer a boxed future.
pub trait Serve: Send + Sync {
    fn serve<'a>(&'a self, c: Call, progress: Progress<'a>) -> Answer<'a>;
}

/// A call's answer, being worked out.
pub type Answer<'a> = Pin<Box<dyn Future<Output = Result<Reply, CallError>> + 'a>>;

type Progress<'a> = dform_core::plugin::backend::Progress<'a>;

impl<H: Handler> Serve for H {
    fn serve<'a>(&'a self, c: Call, progress: Progress<'a>) -> Answer<'a> {
        Box::pin(serve(&SESSIONS, self, c, progress))
    }
}

/// `handler`'s answer to `call`, in its session as `sessions` numbers it:
/// the session the host calls made while it runs name, its future polled
/// in the call ([`InCall`]) wherever it is polled.
async fn serve<H: Handler + ?Sized>(
    sessions: &Sessions,
    handler: &H,
    call: Call,
    progress: Progress<'_>,
) -> Result<Reply, CallError> {
    let session = sessions.of(&call);
    let within = Within {
        session,
        ..Within::here()
    };
    let mut reply = InCall::new(within, handler.handle(session, call, progress)).await?;
    if let Reply::Configure(r) = &mut reply {
        r.session = session.0;
    }
    Ok(reply)
}

/// A future polled in a call (`backend::Within`): the thread that polls
/// it is in the call while it does, so the host calls it starts are the
/// call's, whichever thread or task polls it.
struct InCall<F> {
    within: Within,
    inner: Pin<Box<F>>,
}

impl<F: Future> InCall<F> {
    fn new(within: Within, inner: F) -> InCall<F> {
        InCall {
            within,
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for InCall<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _in = self.within.enter();
        self.inner.as_mut().poll(cx)
    }
}

/// What makes the provider: `provider!`'s expression.
pub type Make = fn() -> Box<dyn Serve>;

/// The provider, made at its first call.
static HANDLER: OnceLock<Box<dyn Serve>> = OnceLock::new();
/// Its sessions, as its Configures open them.
static SESSIONS: LazyLock<Sessions> = LazyLock::new(Sessions::default);
/// The credentials and tunnels it opened, by handle: locked only to
/// insert or look one up, never across an import.
static OPENED: Mutex<Held> = Mutex::new(Held {
    credentials: BTreeMap::new(),
    tunnels: BTreeMap::new(),
    next: 0,
});

fn opened() -> MutexGuard<'static, Held> {
    // An insert or a lookup: a panic leaves the maps whole.
    OPENED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The provider, made at the first call. One that reads files (the
/// `filesystem` feature, granted `wasi:filesystem`) starts in the
/// working directory the host gives (`wasi:cli/environment`'s
/// initial-cwd, which wasi-libc does not read: it starts at `/`); the
/// change of directory is what links `wasi:filesystem`, so a component
/// without the feature imports none and needs no grant.
fn start(make: Make) -> Box<dyn Serve> {
    #[cfg(feature = "filesystem")]
    if let Some(cwd) = bindings::wasi::cli::environment::get_initial_cwd() {
        let _ = std::env::set_current_dir(cwd);
    }
    make()
}

async fn call(make: Make, c: Call, progress: Progress<'_>) -> Result<Reply, t::CallError> {
    let handler = HANDLER.get_or_init(|| start(make));
    handler
        .serve(c, progress)
        .await
        .map_err(|e| conv::to_call_error(&e))
}

fn decode<T>(r: Result<T, String>) -> Result<T, t::CallError> {
    r.map_err(|e| t::CallError::Refused(format!("a malformed request: {e}")))
}

fn wrong(r: Reply) -> t::CallError {
    t::CallError::Refused(format!("answered with a {} reply", r.method()))
}

macro_rules! export {
    ($($name:ident($req:ty => $from:ident) -> $resp:ty: $variant:ident => $to:expr;)*) => {$(
        pub async fn $name(make: Make, r: $req) -> Result<$resp, t::CallError> {
            let req = decode(conv::$from(&r))?;
            match call(make, req.into(), &dform_core::plugin::backend::silent).await? {
                Reply::$variant(x) => Ok(($to)(&x)),
                other => Err(wrong(other)),
            }
        }
    )*};
}

export! {
    handshake(t::HandshakeRequest => from_handshake_request) -> t::HandshakeResponse: Handshake => conv::to_handshake_response;
    configure(t::ConfigureRequest => from_configure_request) -> t::ConfigureResponse: Configure => conv::to_configure_response;
    schema(t::SchemaRequest => from_schema_request) -> t::SchemaResponse: Schema => conv::to_schema_response;
    query(t::QueryRequest => from_query_request) -> Vec<t::Row>: Query => |rows: &Vec<_>| conv::to_rows(rows);
    read(t::ReadRequest => from_read_request) -> t::ReadResponse: Read => conv::to_read_response;
    plan(t::PlanRequest => from_plan_request) -> t::PlanResponse: Plan => conv::to_plan_response;
    import(t::ImportRequest => from_import_request) -> t::ImportResponse: Import => conv::to_import_response;
    reveal(t::RevealRequest => from_reveal_request) -> t::RevealResponse: Reveal => conv::to_reveal_response;
    health(t::HealthRequest => from_health_request) -> t::HealthResponse: Health => conv::to_health_response;
}

/// What an Apply answers: its events, and its result.
pub type Applied = (
    wit_bindgen::StreamReader<t::Event>,
    wit_bindgen::FutureReader<Result<t::ApplyResponse, t::CallError>>,
);

/// Apply `r`: the stream and the future at once, the handler in a task
/// of its own, each event written as it says it, then its result.
pub async fn apply(make: Make, r: t::ApplyRequest) -> Applied {
    let (mut events, said_rx) = bindings::wit_stream::new::<t::Event>();
    let (result, result_rx) =
        bindings::wit_future::new::<Result<t::ApplyResponse, t::CallError>>(|| {
            Err(t::CallError::Crashed(
                "the provider dropped its Apply's answer".into(),
            ))
        });
    wit_bindgen::spawn_local(async move {
        let (said, mut heard) = futures::channel::mpsc::unbounded::<pb::Event>();
        let answered = async move {
            let progress = move |e: pb::Event| {
                // Refused: the stream is gone, and so is what it tells.
                let _ = said.unbounded_send(e);
            };
            let req = decode(conv::from_apply_request(&r))?;
            match call(make, req.into(), &progress).await? {
                Reply::Apply(x) => Ok(conv::to_apply_response(&x)),
                other => Err(wrong(other)),
            }
        };
        let told = async move {
            while let Some(e) = heard.next().await {
                // Refused: the host dropped the stream.
                if events.write_one(conv::to_event(&e)).await.is_some() {
                    break;
                }
            }
        };
        let (answer, ()) = futures::future::join(answered, told).await;
        let _ = result.write(answer).await;
    });
    (said_rx, result_rx)
}

fn error(e: wh::types::Error) -> h::Error {
    h::Error {
        class: match e.class {
            wh::types::ErrorClass::Final => h::Class::Final,
            wh::types::ErrorClass::Retryable => h::Class::Retryable,
            wh::types::ErrorClass::MaybeApplied => h::Class::MaybeApplied,
        },
        message: e.message,
    }
}

fn failure(f: wh::types::NotYetOrError) -> Failure {
    match f {
        wh::types::NotYetOrError::NotYet(m) => Failure::NotYet(m),
        wh::types::NotYetOrError::Error(e) => Failure::Error(error(e)),
    }
}

fn target(t: &Target) -> wh::ssh::Target {
    wh::ssh::Target {
        host: t.host.clone(),
        user: t.user.clone(),
        port: t.port,
    }
}

/// The host's calls that do not wait on the world, as the component's
/// imports; those that do are [`waits`].
pub struct Imports;

struct Held {
    credentials: BTreeMap<Handle, Arc<wh::types::Credential>>,
    tunnels: BTreeMap<Handle, (Arc<wh::types::Tunnel>, Endpoint)>,
    next: Handle,
}

impl Held {
    fn handle(&mut self) -> Handle {
        self.next += 1;
        self.next
    }
}

/// The host's calls that wait on the world: each an `async func` import,
/// a subtask the component awaits while the instance goes on.
pub(crate) mod waits {
    use super::*;
    use std::time::Duration;

    /// Send `req` with the credential and the tunnel it names, held for
    /// the import (the resources are borrowed by it), not the table.
    pub async fn send(
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, h::Error> {
        let (auth, via) = {
            let o = opened();
            (
                auth.and_then(|a| o.credentials.get(&a).cloned()),
                via.and_then(|v| o.tunnels.get(&v)).map(|(t, _)| t.clone()),
            )
        };
        let r = wh::http::send(
            wh::http::Request {
                method: req.method,
                url: req.url,
                headers: req.headers,
                body: req.body,
                timeout_ms: req
                    .timeout
                    .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
                bucket: req.bucket,
                session: req.session.0,
                idempotent: req.idempotent,
            },
            auth.as_deref(),
            via.as_deref(),
        )
        .await
        .map_err(error)?;
        Ok(HttpResponse {
            status: r.status,
            headers: r.headers,
            body: r.body,
        })
    }

    pub async fn io_read(location: String) -> Result<Vec<u8>, Failure> {
        wh::io::read(location).await.map_err(failure)
    }

    pub async fn exec(
        on: Target,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Run, Failure> {
        let r = wh::ssh::exec(target(&on), argv, stdin)
            .await
            .map_err(failure)?;
        Ok(Run {
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }

    pub async fn sleep(d: Duration) {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        bindings::wasi::clocks::monotonic_clock::wait_for(nanos).await
    }
}

impl Calls for Imports {
    fn open(&self, name: &str) -> Result<Opened, h::Error> {
        let c = wh::secrets::open(name).map_err(error)?;
        let (endpoint, namespace) = (c.endpoint(), c.namespace());
        let mut o = opened();
        let handle = o.handle();
        o.credentials.insert(handle, Arc::new(c));
        Ok(Opened {
            handle,
            endpoint,
            namespace,
        })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, h::Error> {
        wit_bindgen::block_on(waits::send(req, auth, via))
    }

    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        wit_bindgen::block_on(waits::io_read(location.to_string()))
    }

    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let d =
            wit_bindgen::block_on(wh::io::read_versioned(location.to_string())).map_err(failure)?;
        Ok(dform_core::files::Document {
            bytes: d.data,
            version: d.version,
        })
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        wit_bindgen::block_on(waits::exec(
            on.clone(),
            argv.to_vec(),
            stdin.map(<[u8]>::to_vec),
        ))
    }

    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), h::Error> {
        wh::ssh::write(&target(on), path, data, mode).map_err(error)
    }

    fn forward(&self, via: &Target, to: &Endpoint) -> Result<Handle, h::Error> {
        let t = wh::ssh::forward(
            &target(via),
            &wh::types::Endpoint {
                host: to.host.clone(),
                port: to.port,
            },
        )
        .map_err(error)?;
        let mut o = opened();
        let handle = o.handle();
        o.tunnels.insert(handle, (Arc::new(t), to.clone()));
        Ok(handle)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        opened().tunnels.get(&h).map(|(_, e)| e.clone())
    }

    fn git_commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, h::Error> {
        let files: Vec<wh::git::File> = files
            .into_iter()
            .map(|f| wh::git::File {
                path: f.path,
                data: f.data,
            })
            .collect();
        wh::git::commit(repo, branch, &files, message).map_err(error)
    }

    /// `cache.get`: the host knows the call it is made in, so `at` says
    /// only the scope.
    fn cache_get(&self, at: &h::CacheAt, key: &str) -> Option<Vec<u8>> {
        wh::cache::get(scope(at.scope), key)
    }

    fn cache_put(&self, at: &h::CacheAt, key: &str, value: Vec<u8>) {
        wh::cache::put(scope(at.scope), key, &value)
    }

    fn log(&self, level: Level, message: &str) {
        use wh::log::Level as L;
        wh::log::log(
            match level {
                Level::Debug => L::Debug,
                Level::Info => L::Info,
                Level::Warn => L::Warn,
                Level::Error => L::Error,
            },
            message,
        );
    }
}

/// A cache scope, as the host's import takes it.
fn scope(s: h::Scope) -> wh::cache::Scope {
    match s {
        h::Scope::Session => wh::cache::Scope::Session,
        h::Scope::Run => wh::cache::Scope::Run,
        h::Scope::Account => wh::cache::Scope::Account,
        h::Scope::Provider => wh::cache::Scope::Provider,
    }
}
