//! A provider built as a component (`--target wasm32-wasip2 --features
//! component`): `dform:host/hosted-provider`'s bindings, its exports
//! answered by the provider's [`Handler`] and its imports the host's
//! calls ([`Imports`]).

use dform_core::plugin::backend::{Call, Handler, Reply};
use dform_core::plugin::host::{
    self as h, Calls, Endpoint, Failure, GitFile, Handle, HttpRequest, HttpResponse, Level, Opened,
    Run, Target,
};
use std::cell::OnceCell;
use std::collections::BTreeMap;

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

fn call(make: fn() -> Box<dyn Handler>, c: Call) -> Result<Reply, t::CallError> {
    HANDLER.with(|cell| {
        cell.get_or_init(make)
            .handle(c)
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
        pub fn $name(make: fn() -> Box<dyn Handler>, r: $req) -> Result<$resp, t::CallError> {
            let req = decode(conv::$from(&r))?;
            match call(make, req.into())? {
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
    apply(t::ApplyRequest => from_apply_request) -> t::ApplyResponse: Apply => conv::to_apply_response;
    import(t::ImportRequest => from_import_request) -> t::ImportResponse: Import => conv::to_import_response;
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
/// tunnels it opened are held here, by handle.
#[derive(Default)]
pub struct Imports {
    credentials: BTreeMap<Handle, wh::types::Credential>,
    tunnels: BTreeMap<Handle, (wh::types::Tunnel, Endpoint)>,
    next: Handle,
}

// SAFETY: a component is single-threaded; the resource handles are never
// touched from another thread.
unsafe impl Send for Imports {}

impl Imports {
    fn handle(&mut self) -> Handle {
        self.next += 1;
        self.next
    }
}

impl Calls for Imports {
    fn open(&mut self, name: &str) -> Result<Opened, h::Error> {
        let c = wh::secrets::open(name).map_err(error)?;
        let endpoint = c.endpoint();
        let handle = self.handle();
        self.credentials.insert(handle, c);
        Ok(Opened { handle, endpoint })
    }

    fn send(
        &mut self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, h::Error> {
        let auth = auth.and_then(|a| self.credentials.get(&a));
        let via = via.and_then(|v| self.tunnels.get(&v)).map(|(t, _)| t);
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

    fn exec(&mut self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        let r = wh::ssh::exec(&target(on), argv, stdin).map_err(failure)?;
        Ok(Run {
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }

    fn read(&mut self, on: &Target, path: &str) -> Result<Vec<u8>, Failure> {
        wh::ssh::read(&target(on), path).map_err(failure)
    }

    fn write(&mut self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), h::Error> {
        wh::ssh::write(&target(on), path, data, mode).map_err(error)
    }

    fn forward(&mut self, via: &Target, to: &Endpoint) -> Result<Handle, h::Error> {
        let t = wh::ssh::forward(
            &target(via),
            &wh::types::Endpoint {
                host: to.host.clone(),
                port: to.port,
            },
        )
        .map_err(error)?;
        let handle = self.handle();
        self.tunnels.insert(handle, (t, to.clone()));
        Ok(handle)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        self.tunnels.get(&h).map(|(_, e)| e.clone())
    }

    fn git_read(&mut self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, h::Error> {
        wh::git::read(repo, rev, path).map_err(error)
    }

    fn git_commit(
        &mut self,
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

    fn log(&mut self, level: Level, message: &str) {
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
