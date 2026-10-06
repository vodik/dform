//! The component host (the `wasm` feature, experimental): a provider that
//! is a component of `dform:host/hosted-provider` (wit/host), run by
//! wasmtime behind dform-core's `Provider` trait.
//!
//! One instance per provider per run; calls run one at a time on it, in
//! the order they were submitted (wasip2 has no concurrent calls into an
//! instance: `--parallel` serializes here, where on wasip3 one instance
//! takes them all). A trap is the provider crashing (`Crashed`), and the
//! instance answers no call after one. Every call has an epoch deadline,
//! a backstop under the link's own timeout (`plugin::timed`): a call past
//! it is `MaybeApplied` for an Apply, `Refused` otherwise.
//!
//! Grants, not walls: the host's own interfaces are imports any component
//! has; a component that imports `wasi:sockets` or `wasi:http` is refused
//! unless dform.toml grants it, naming both, and one that imports
//! `wasi:filesystem` likewise (it then sees the filesystem as the user
//! does, from `/`). No environment variable is passed in: a credential
//! is had by name (`dform:host/secrets`), never from the environment.

use crate::services::Services;
use anyhow::{Context, Result, anyhow};
use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Ticket};
use dform_core::plugin::host::{self as h, Calls, Grants, Hosting, Manifest};
use dform_core::plugin::link::Link;
use dform_core::plugin::pb;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/host",
        world: "dform:host/hosted-provider",
        with: {
            "wasi:cli": wasmtime_wasi::p2::bindings::cli,
            "wasi:clocks": wasmtime_wasi::p2::bindings::clocks,
            "wasi:random": wasmtime_wasi::p2::bindings::random,
            "wasi:io": wasmtime_wasi::p2::bindings::sync::io,
            "wasi:filesystem": wasmtime_wasi::p2::bindings::sync::filesystem,
            "wasi:sockets": wasmtime_wasi::p2::bindings::sync::sockets,
            "wasi:http": wasmtime_wasi_http::p2::bindings::sync::http,
            "dform:host/types.credential": super::Credential,
            "dform:host/types.tunnel": super::Tunnel,
        },
    });
}

dform_wit::convert!(conv, super::bindings::dform::provider::types);

use bindings::dform::host as wh;

/// A credential a component opened: the host's handle, never its value.
pub struct Credential {
    handle: h::Handle,
    name: String,
    endpoint: Option<String>,
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

/// The engine every component runs on, its epoch ticking.
fn engine() -> Result<&'static Engine> {
    static ENGINE: OnceLock<std::result::Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut c = Config::new();
            c.wasm_component_model(true).epoch_interruption(true);
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
    host: Services,
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
            hooks: wasmtime_wasi_http::default_hooks(),
            table: &mut self.table,
            ctx: &mut self.http,
        }
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

impl wh::secrets::Host for State {
    fn open(&mut self, name: String) -> Result<Resource<Credential>, wh::types::Error> {
        let o = self.host.open(&name).map_err(error)?;
        self.table
            .push(Credential {
                handle: o.handle,
                name,
                endpoint: o.endpoint,
            })
            .map_err(|e| error(h::Error::fatal(e.to_string())))
    }
}

impl wh::http::Host for State {
    fn send(
        &mut self,
        req: wh::http::Request,
        auth: Option<Resource<Credential>>,
        via: Option<Resource<Tunnel>>,
    ) -> Result<wh::http::Response, wh::types::Error> {
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
        let r = self
            .host
            .send(
                h::HttpRequest {
                    method: req.method,
                    url: req.url,
                    headers: req.headers,
                    body: req.body,
                    timeout: req.timeout_ms.map(|ms| Duration::from_millis(ms.into())),
                },
                auth,
                via,
            )
            .map_err(error)?;
        Ok(wh::http::Response {
            status: r.status,
            headers: r.headers,
            body: r.body,
        })
    }
}

impl wh::ssh::Host for State {
    fn exec(
        &mut self,
        on: wh::ssh::Target,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<wh::ssh::Run, wh::types::NotYetOrError> {
        let r = self
            .host
            .exec(&target(on), &argv, stdin.as_deref())
            .map_err(failure)?;
        Ok(wh::ssh::Run {
            status: r.status,
            stdout: r.stdout,
            stderr: r.stderr,
        })
    }

    fn read(
        &mut self,
        on: wh::ssh::Target,
        path: String,
    ) -> Result<Vec<u8>, wh::types::NotYetOrError> {
        self.host.read(&target(on), &path).map_err(failure)
    }

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

impl wh::git::Host for State {
    fn read(
        &mut self,
        repo: String,
        rev: String,
        path: String,
    ) -> Result<Vec<u8>, wh::types::Error> {
        self.host.git_read(&repo, &rev, &path).map_err(error)
    }

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

/// The component at `path`, compiled, or as compiled before: compiling a
/// provider takes seconds, so the native code is kept in
/// `$XDG_CACHE_HOME/dform/wasm/<sha256 of the file>.cwasm`, written by
/// this engine (wasmtime refuses one another version wrote, and it is
/// compiled again).
fn load(engine: &Engine, path: &Path) -> Result<Component> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|c| c.join("dform").join("wasm"));
    let cached = dir.as_ref().map(|d| d.join(format!("{digest}.cwasm")));
    if let Some(c) = &cached
        && c.exists()
    {
        // SAFETY: the file is one this host wrote with `serialize` (by
        // its content's digest, in the operator's own cache); wasmtime
        // checks it was compiled by this version and configuration.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, c) } {
            return Ok(component);
        }
    }
    let component = Component::new(engine, &bytes)?;
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

/// A running component provider.
pub struct Wasm {
    store: Store<State>,
    provider: bindings::HostedProvider,
    queued: VecDeque<(Ticket, Call)>,
    next: u64,
    /// Why it answers no more calls, once it trapped.
    dead: Option<String>,
    program: String,
    /// The name its handshake gave, once it has: messages name it.
    name: String,
}

impl Wasm {
    /// Load, admit (its imports against `grants`) and instantiate the
    /// component at `path`.
    pub fn start(path: &Path, grants: Grants) -> Result<(Wasm, Manifest)> {
        let program = path.display().to_string();
        let engine = engine()?;
        let component =
            load(engine, path).with_context(|| format!("load the provider component {program}"))?;
        let manifest = manifest(engine, &component);
        grants
            .admit(&manifest)
            .map_err(|e| anyhow!("{e} ({program})"))?;
        let mut linker: Linker<State> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)?;
        wh::types::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::secrets::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::http::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::ssh::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::git::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        wh::log::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;
        let mut wasi = WasiCtx::builder();
        wasi.inherit_stdout().inherit_stderr();
        if grants.allow.contains("wasi:filesystem") {
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
        if grants.allow.contains("wasi:sockets") {
            wasi.inherit_network().allow_ip_name_lookup(true);
        } else {
            wasi.allow_tcp(false)
                .allow_udp(false)
                .allow_ip_name_lookup(false);
        }
        let state = State {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            host: Services::new(grants),
        };
        let mut store = Store::new(engine, state);
        store.set_epoch_deadline(ticks(DEADLINE));
        let provider = bindings::HostedProvider::instantiate(&mut store, &component, &linker)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("instantiate the provider component {program}"))?;
        Ok((
            Wasm {
                store,
                provider,
                queued: VecDeque::new(),
                next: 0,
                dead: None,
                program,
                name: String::new(),
            },
            manifest,
        ))
    }

    fn run(&mut self, call: Call) -> Result<Reply, CallError> {
        if let Some(why) = &self.dead {
            return Err(CallError::Crashed(why.clone()));
        }
        let apply = matches!(call, Call::Apply(_));
        self.store.set_epoch_deadline(ticks(DEADLINE));
        let p = self.provider.dform_provider_provider();
        let s = &mut self.store;
        let out: wasmtime::Result<Result<Reply, CallError>> = (|| {
            Ok(match &call {
                Call::Handshake(r) => p
                    .call_handshake(&mut *s, conv::to_handshake_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_handshake_response(&r)).map(Reply::Handshake)),
                Call::Configure(r) => p
                    .call_configure(&mut *s, &conv::to_configure_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_configure_response(&r)).map(Reply::Configure)),
                Call::Schema(r) => p
                    .call_schema(&mut *s, &conv::to_schema_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_schema_response(&r)).map(Reply::Schema)),
                Call::Query(r) => p
                    .call_query(&mut *s, &conv::to_query_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_rows(&r)).map(Reply::Query)),
                Call::Read(r) => p
                    .call_read(&mut *s, &conv::to_read_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_read_response(&r)).map(Reply::Read)),
                Call::Plan(r) => p
                    .call_plan(&mut *s, &conv::to_plan_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_plan_response(&r)).map(Reply::Plan)),
                Call::Apply(r) => {
                    if r.op == pb::Op::Unspecified as i32 {
                        return Ok(Err(CallError::Refused(
                            "an Apply call with no op".to_string(),
                        )));
                    }
                    p.call_apply(&mut *s, &conv::to_apply_request(r))?
                        .map_err(conv::from_call_error)
                        .and_then(|r| decode(conv::from_apply_response(&r)).map(Reply::Apply))
                }
                Call::Import(r) => p
                    .call_import(&mut *s, &conv::to_import_request(r))?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_import_response(&r)).map(Reply::Import)),
            })
        })();
        match out {
            Ok(answer) => {
                if let Ok(Reply::Handshake(h)) = &answer {
                    self.name = h.name.clone();
                }
                answer
            }
            Err(trap) => {
                let who = if self.name.is_empty() {
                    self.program.clone()
                } else {
                    self.name.clone()
                };
                let interrupted = trap
                    .downcast_ref::<wasmtime::Trap>()
                    .is_some_and(|t| *t == wasmtime::Trap::Interrupt);
                let exit = trap.downcast_ref::<wasmtime_wasi::I32Exit>().map(|e| e.0);
                let why = match exit {
                    Some(code) => {
                        format!("the provider {who} exited during the call (exit status: {code})")
                    }
                    None if interrupted => format!(
                        "the provider {who} ran past its deadline ({}s) and was stopped",
                        DEADLINE.as_secs()
                    ),
                    None => format!("the provider {who} trapped: {trap:#}"),
                };
                self.dead = Some(format!("the provider {who} has exited"));
                Err(match (interrupted, apply) {
                    (true, true) => CallError::MaybeApplied(why),
                    (true, false) => CallError::Refused(why),
                    (false, _) => CallError::Crashed(why),
                })
            }
        }
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
        self.queued.push_back((t, call));
        t
    }

    fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>) {
        let (t, call) = self
            .queued
            .pop_front()
            .expect("internal: no call in flight");
        (t, self.run(call))
    }

    fn is_dead(&mut self) -> bool {
        self.dead.is_some()
    }
}

/// The component at `path`, started with `grants` and shaken hands with.
pub fn link(path: &Path, grants: Grants) -> Result<Link> {
    let (w, manifest) = Wasm::start(path, grants.clone())?;
    let program = path.display().to_string();
    h::observe(
        &program,
        Hosting {
            host: "wasm",
            manifest: Some(manifest),
            grants,
        },
    );
    Link::start(program, Box::new(w))
}
