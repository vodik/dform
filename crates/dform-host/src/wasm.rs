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
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;
use wasmtime::component::{
    Accessor, Component, FutureConsumer, HasSelf, Linker, Resource, ResourceTable, Source,
    StreamConsumer, StreamResult,
};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/host",
        world: "dform:host/hosted-provider",
        require_store_data_send: true,
        with: {
            "wasi:cli": wasmtime_wasi::p2::bindings::cli,
            "wasi:clocks": wasmtime_wasi::p2::bindings::clocks,
            "wasi:random": wasmtime_wasi::p2::bindings::random,
            "wasi:io": wasmtime_wasi::p2::bindings::io,
            "wasi:filesystem": wasmtime_wasi::p2::bindings::filesystem,
            "wasi:sockets": wasmtime_wasi::p2::bindings::sockets,
            "wasi:http": wasmtime_wasi_http::p2::bindings::http,
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

impl wh::io::Host for State {
    fn read(&mut self, location: String) -> Result<Vec<u8>, wh::types::NotYetOrError> {
        self.host.io_read(&location).map_err(failure)
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

/// What the instance's thread sends back: an event of a call in flight,
/// or a call's answer.
enum Back {
    Event(Ticket, pb::Event),
    Answer(Ticket, Box<Result<Reply, CallError>>),
}

/// A running component provider: its instance on a thread of its own,
/// which takes every call as it is submitted and runs them at once.
pub struct Wasm {
    calls: Option<tokio::sync::mpsc::UnboundedSender<(Ticket, Call)>>,
    back: std::sync::mpsc::Receiver<Back>,
    next: u64,
    /// Why it answers no more calls, once it trapped.
    dead: Arc<Mutex<Option<String>>>,
    /// Calls refused before they were sent (it had trapped).
    refused: Vec<(Ticket, CallError)>,
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
        let (calls, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, back) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let dead = Arc::new(Mutex::new(None));
        let instance = Instance {
            program: program.clone(),
            grants,
            dead: dead.clone(),
            tx,
        };
        std::thread::Builder::new()
            .name("dform-wasm".into())
            .spawn(move || {
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
                rt.block_on(instance.serve(engine, component, rx, ready));
            })
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
            },
            manifest,
        ))
    }
}

/// The instance's side: what it was started as, and where answers go.
struct Instance {
    program: String,
    grants: Grants,
    dead: Arc<Mutex<Option<String>>>,
    tx: std::sync::mpsc::Sender<Back>,
}

impl Instance {
    fn linker(&self, engine: &Engine) -> Result<Linker<State>> {
        let mut linker: Linker<State> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
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
        Ok(State {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            host: Services::new(self.grants.clone()),
        })
    }

    /// Instantiate, say so on `ready`, then run every call `calls` brings
    /// at once, until dform drops the provider or the instance traps.
    async fn serve(
        self,
        engine: &'static Engine,
        component: Component,
        mut calls: tokio::sync::mpsc::UnboundedReceiver<(Ticket, Call)>,
        ready: std::sync::mpsc::Sender<Result<()>>,
    ) {
        let started = async {
            let linker = self.linker(engine)?;
            let mut store = Store::new(engine, self.state()?);
            store.set_epoch_deadline(ticks(DEADLINE));
            let provider =
                bindings::HostedProvider::instantiate_async(&mut store, &component, &linker)
                    .await
                    .map_err(anyhow::Error::from)?;
            Ok::<_, anyhow::Error>((store, provider))
        };
        let (mut store, provider) = match started.await {
            Ok(s) => {
                let _ = ready.send(Ok(()));
                s
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let who = std::cell::RefCell::new(self.program.clone());
        // The calls not answered yet: each answered as crashed if the
        // instance traps.
        let open = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let ran = store
            .run_concurrent(async |a| {
                type Running<'a> =
                    Pin<Box<dyn Future<Output = (Ticket, bool, wasmtime::Result<()>)> + 'a>>;
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
                        got = calls.recv() => match got {
                            Some((t, call)) => {
                                open.borrow_mut().insert(t);
                                running.push(Box::pin(self.one(a, &provider, &who, t, call)));
                            }
                            // dform dropped the provider.
                            None => return None,
                        },
                        (t, apply, r) = next => {
                            open.borrow_mut().remove(&t);
                            if let Err(trap) = r {
                                return Some((t, apply, trap));
                            }
                        }
                    }
                }
            })
            .await;
        let trapped = match ran {
            Ok(None) => return,
            Ok(Some((t, apply, trap))) => Some((t, apply, trap)),
            Err(trap) => {
                let t = open.borrow().iter().next().copied();
                t.map(|t| (t, false, trap))
            }
        };
        let who = who.into_inner();
        if let Some((t, apply, trap)) = trapped {
            open.borrow_mut().remove(&t);
            let _ = self.tx.send(Back::Answer(
                t,
                Box::new(Err(trapped_as(&who, apply, &trap))),
            ));
        }
        let gone = format!("the provider {who} has exited");
        *self.dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(gone.clone());
        for t in open.into_inner() {
            let _ = self.tx.send(Back::Answer(
                t,
                Box::new(Err(CallError::Crashed(gone.clone()))),
            ));
        }
        // A call submitted before the submitter saw it die.
        while let Some((t, _)) = calls.recv().await {
            let _ = self.tx.send(Back::Answer(
                t,
                Box::new(Err(CallError::Crashed(gone.clone()))),
            ));
        }
    }

    /// Run `call`, telling its events and then its answer; a trap is its
    /// outcome, with whether it was an Apply.
    async fn one(
        &self,
        a: &Accessor<State>,
        provider: &bindings::HostedProvider,
        who: &std::cell::RefCell<String>,
        t: Ticket,
        call: Call,
    ) -> (Ticket, bool, wasmtime::Result<()>) {
        let apply = matches!(call, Call::Apply(_));
        let p = provider.dform_provider_provider();
        let answered = async {
            Ok::<_, wasmtime::Error>(match &call {
                Call::Handshake(r) => p
                    .call_handshake(a, conv::to_handshake_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_handshake_response(&r)).map(Reply::Handshake)),
                Call::Configure(r) => p
                    .call_configure(a, conv::to_configure_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_configure_response(&r)).map(Reply::Configure)),
                Call::Schema(r) => p
                    .call_schema(a, conv::to_schema_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_schema_response(&r)).map(Reply::Schema)),
                Call::Query(r) => p
                    .call_query(a, conv::to_query_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_rows(&r)).map(Reply::Query)),
                Call::Read(r) => p
                    .call_read(a, conv::to_read_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_read_response(&r)).map(Reply::Read)),
                Call::Plan(r) => p
                    .call_plan(a, conv::to_plan_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_plan_response(&r)).map(Reply::Plan)),
                Call::Apply(r) if r.op == pb::Op::Unspecified as i32 => {
                    Err(CallError::Refused("an Apply call with no op".to_string()))
                }
                Call::Apply(r) => {
                    let (events, result) = p.call_apply(a, conv::to_apply_request(r)).await?;
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
                    match answer.await {
                        Ok(r) => r
                            .map_err(conv::from_call_error)
                            .and_then(|r| decode(conv::from_apply_response(&r)).map(Reply::Apply)),
                        Err(_) => Err(CallError::Crashed(format!(
                            "the provider {} dropped its Apply's answer",
                            who.borrow()
                        ))),
                    }
                }
                Call::Import(r) => p
                    .call_import(a, conv::to_import_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_import_response(&r)).map(Reply::Import)),
                Call::Reveal(r) => p
                    .call_reveal(a, conv::to_reveal_request(r))
                    .await?
                    .map_err(conv::from_call_error)
                    .and_then(|r| decode(conv::from_reveal_response(&r)).map(Reply::Reveal)),
            })
        };
        match answered.await {
            Ok(answer) => {
                if let Ok(Reply::Handshake(h)) = &answer {
                    *who.borrow_mut() = h.name.clone();
                }
                let _ = self.tx.send(Back::Answer(t, Box::new(answer)));
                (t, apply, Ok(()))
            }
            Err(trap) => (t, apply, Err(trap)),
        }
    }
}

/// A trap of the provider `who` in a call, as the call's failure: an exit
/// or a crash is `Crashed`; past its deadline, an Apply may have taken
/// effect, another call is refused.
fn trapped_as(who: &str, apply: bool, trap: &wasmtime::Error) -> CallError {
    let interrupted = trap
        .downcast_ref::<wasmtime::Trap>()
        .is_some_and(|t| *t == wasmtime::Trap::Interrupt);
    let exit = trap.downcast_ref::<wasmtime_wasi::I32Exit>().map(|e| e.0);
    let why = match exit {
        Some(code) => format!("the provider {who} exited during the call (exit status: {code})"),
        None if interrupted => format!(
            "the provider {who} ran past its deadline ({}s) and was stopped",
            DEADLINE.as_secs()
        ),
        None => format!("the provider {who} trapped: {trap:#}"),
    };
    match (interrupted, apply) {
        (true, true) => CallError::MaybeApplied(why),
        (true, false) => CallError::Refused(why),
        (false, _) => CallError::Crashed(why),
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
            && self
                .calls
                .as_ref()
                .is_some_and(|c| c.send((t, call)).is_ok());
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
}

impl Drop for Wasm {
    /// Its thread ends once it sees no more calls can come; a call still
    /// running in it is left to finish there.
    fn drop(&mut self) {
        self.calls.take();
    }
}

/// The component at `path`, started with `grants` and shaken hands with.
pub fn link(path: &Path, grants: Grants) -> Result<Link> {
    let (w, manifest) = Wasm::start(path, grants.clone())?;
    let mut link = Link::start(path.display().to_string(), Box::new(w))?;
    link.hosting = Some(Hosting {
        host: "wasm",
        manifest: Some(manifest),
        grants,
    });
    Ok(link)
}
