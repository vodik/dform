//! A provider built as a component (`--target wasm32-wasip2 --features
//! component`): `dform:host/hosted-provider`'s bindings, its exports
//! answered by the provider's [`Handler`] and its imports the host's
//! calls ([`Imports`]).
//!
//! The exports are async (the component model's async ABI, R-130): Apply
//! answers at once with a stream of events and a future of its result,
//! and runs the handler in a task of its own. The handler is synchronous,
//! so the instance does nothing else while it runs, and its events are
//! written as it returns, before its result; an async handler would
//! write each as it said it. A host that drops the stream (it cancelled
//! the call) is written no more events.

use dform_core::plugin::backend::{Call, Handler, Progress, Reply};
use dform_core::plugin::host::{
    self as h, Calls, Endpoint, Failure, GitFile, Handle, HttpRequest, HttpResponse, Level, Opened,
    Run, Target,
};
use dform_core::plugin::pb;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::sync::Mutex;

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

thread_local! {
    /// The provider, made at its first call (a component is
    /// single-threaded).
    static HANDLER: OnceCell<Box<dyn Handler>> = const { OnceCell::new() };
}

/// The provider, made at the first call, in the working directory the
/// host gives (`wasi:cli/environment`'s initial-cwd, which wasi-libc does
/// not read: it starts at `/`).
fn start(make: fn() -> Box<dyn Handler>) -> Box<dyn Handler> {
    if let Some(cwd) = bindings::wasi::cli::environment::initial_cwd() {
        let _ = std::env::set_current_dir(cwd);
    }
    make()
}

fn call(
    make: fn() -> Box<dyn Handler>,
    c: Call,
    progress: Progress,
) -> Result<Reply, t::CallError> {
    HANDLER.with(|cell| {
        cell.get_or_init(|| start(make))
            .handle(c, progress)
            .map_err(|e| conv::to_call_error(&e))
    })
}

fn decode<T>(r: Result<T, String>) -> Result<T, t::CallError> {
    r.map_err(|e| t::CallError::Refused(format!("a malformed request: {e}")))
}

fn wrong(r: Reply) -> t::CallError {
    t::CallError::Refused(format!("answered with a {} reply", r.method()))
}

macro_rules! export {
    ($($name:ident($req:ty => $from:ident) -> $resp:ty: $variant:ident => $to:expr;)*) => {$(
        pub async fn $name(make: fn() -> Box<dyn Handler>, r: $req) -> Result<$resp, t::CallError> {
            let req = decode(conv::$from(&r))?;
            match call(make, req.into(), &dform_core::plugin::backend::silent)? {
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
/// of its own writing them.
pub async fn apply(make: fn() -> Box<dyn Handler>, r: t::ApplyRequest) -> Applied {
    let (mut events, said_rx) = bindings::wit_stream::new::<t::Event>();
    let (result, result_rx) =
        bindings::wit_future::new::<Result<t::ApplyResponse, t::CallError>>(|| {
            Err(t::CallError::Crashed(
                "the provider dropped its Apply's answer".into(),
            ))
        });
    wit_bindgen::spawn(async move {
        let said = std::sync::Mutex::new(Vec::<pb::Event>::new());
        let progress = |e: pb::Event| said.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        let answer = decode(conv::from_apply_request(&r)).and_then(|req| {
            match call(make, req.into(), &progress)? {
                Reply::Apply(x) => Ok(conv::to_apply_response(&x)),
                other => Err(wrong(other)),
            }
        });
        for e in said.into_inner().unwrap_or_else(|e| e.into_inner()) {
            // Refused: the host dropped the stream.
            if events.write_one(conv::to_event(&e)).await.is_some() {
                break;
            }
        }
        drop(events);
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

/// The host's calls as the component's imports. The credentials and
/// tunnels it opened are held here, by handle, locked only to insert or
/// look one up.
#[derive(Default)]
pub struct Imports(Mutex<Held>);

#[derive(Default)]
struct Held {
    credentials: BTreeMap<Handle, wh::types::Credential>,
    tunnels: BTreeMap<Handle, (wh::types::Tunnel, Endpoint)>,
    next: Handle,
}

// SAFETY: a component is single-threaded; the resource handles are never
// touched from another thread.
unsafe impl Send for Held {}

impl Imports {
    fn opened(&self) -> std::sync::MutexGuard<'_, Held> {
        // An insert or a lookup: a panic leaves the maps whole.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Held {
    fn handle(&mut self) -> Handle {
        self.next += 1;
        self.next
    }
}

impl Calls for Imports {
    fn open(&self, name: &str) -> Result<Opened, h::Error> {
        let c = wh::secrets::open(name).map_err(error)?;
        let endpoint = c.endpoint();
        let mut o = self.opened();
        let handle = o.handle();
        o.credentials.insert(handle, c);
        Ok(Opened { handle, endpoint })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, h::Error> {
        // Held across the import, which borrows the resources: a
        // component is single-threaded, and an instance is not entered
        // again while it waits on a synchronous import.
        let o = self.opened();
        let auth = auth.and_then(|a| o.credentials.get(&a));
        let via = via.and_then(|v| o.tunnels.get(&v)).map(|(t, _)| t);
        let r = wh::http::send(
            &wh::http::Request {
                method: req.method,
                url: req.url,
                headers: req.headers,
                body: req.body,
                timeout_ms: req
                    .timeout
                    .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
            },
            auth,
            via,
        )
        .map_err(error)?;
        Ok(HttpResponse {
            status: r.status,
            headers: r.headers,
            body: r.body,
        })
    }

    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        wh::io::read(location).map_err(failure)
    }

    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let d = wh::io::read_versioned(location).map_err(failure)?;
        Ok(dform_core::files::Document {
            bytes: d.data,
            version: d.version,
        })
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        let r = wh::ssh::exec(&target(on), argv, stdin).map_err(failure)?;
        Ok(Run {
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        })
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
        let mut o = self.opened();
        let handle = o.handle();
        o.tunnels.insert(handle, (t, to.clone()));
        Ok(handle)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        self.opened().tunnels.get(&h).map(|(_, e)| e.clone())
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
