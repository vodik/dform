//! A provider built natively: the protocol served over gRPC
//! (`dform-grpc`), its `Manifest` beside it, and the host called back over
//! the `Host` service at `DFORM_HOST`.
//!
//! The gRPC adapter answers each call on a thread of its own (tokio's
//! blocking pool), which waits on the handler's answer ([`block_on`]). A
//! host call the handler awaits runs on a thread of its own while it
//! waits ([`waits`]), so the calls it awaits together are in flight
//! together, each in the call ([`Within`]): its session's windows, its
//! span.

use crate::{Absent, Handler};
use anyhow::{Context, Result};
use dform_core::plugin::backend::{self, Within};
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Handle, HttpRequest, HttpResponse, Level, Opened,
    Run, Target,
};
use dform_grpc::host as conv;
use dform_grpc::host_pb as h;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tonic::transport::Channel;

/// Serve `handler` over gRPC, declaring `uses` (it runs as the user: a
/// declaration, not a sandbox) and the location schemes it reads
/// (`Handler::schemes`, served as `Io`, R-153, R-155), until stdin closes.
pub fn serve<H: Handler>(handler: H, uses: &[&str]) -> Result<()> {
    let handler = Arc::new(Blocking(handler));
    let schemes = handler.0.schemes();
    let reads = handler.clone();
    let declares = dform_grpc::server::Declares {
        imports: uses.iter().map(|s| s.to_string()).collect(),
        read: (!schemes.is_empty()).then(|| -> dform_grpc::server::Read {
            Arc::new(move |l: &str| block_on(reads.0.read_versioned(l)))
        }),
        schemes,
        signer: handler.0.signer(),
    };
    dform_grpc::server::serve_declaring(Shared(handler), declares)
}

/// A [`Handler`] as a synchronous one (`backend::Handler`), each call
/// waited on by the thread that makes it: what the gRPC adapter serves,
/// and a provider linked into a process of dform's (the direct and wire
/// backends, a test).
pub struct Blocking<H>(pub H);

impl<H: Handler> backend::Handler for Blocking<H> {
    fn handle(
        &self,
        session: backend::Session,
        call: backend::Call,
        progress: backend::Progress,
    ) -> Result<backend::Reply, backend::CallError> {
        block_on(self.0.handle(session, call, progress))
    }

    fn is_dead(&self) -> bool {
        self.0.is_dead()
    }
}

/// A handler held twice: by the protocol's service and by `Io`.
struct Shared<H>(Arc<H>);

impl<H: backend::Handler> backend::Handler for Shared<H> {
    fn handle(
        &self,
        session: backend::Session,
        call: backend::Call,
        progress: backend::Progress,
    ) -> Result<backend::Reply, backend::CallError> {
        self.0.handle(session, call, progress)
    }

    fn is_dead(&self) -> bool {
        self.0.is_dead()
    }
}

/// `provider!`'s `main`.
pub fn main<H: Handler>(make: impl FnOnce() -> H, uses: &[&str]) -> std::process::ExitCode {
    match serve(make(), uses) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_PKG_NAME"));
            std::process::ExitCode::FAILURE
        }
    }
}

/// `future`'s answer, this thread parked until it is ready. Nests: a
/// synchronous handler served here may wait on the host the same way.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);

    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(t) = future.as_mut().poll(&mut cx) {
            return t;
        }
        std::thread::park();
    }
}

/// `f`'s answer, run on a thread of its own in the call this one is in
/// ([`Within`]): its host calls are the call's. A panic there is the
/// awaiting one's.
async fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let within = Within::here();
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _in = within.enter();
        let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
    });
    match rx.await {
        Ok(Ok(t)) => t,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => unreachable!("a host call's thread answers or panics"),
    }
}

/// The host's calls that wait on the world, each on a thread of its own
/// while it waits.
pub(crate) mod waits {
    use super::*;
    use crate::calls;
    use std::time::Duration;

    pub async fn send(
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error> {
        on_a_thread(move || calls().send(req, auth, via)).await
    }

    pub async fn io_read(location: String) -> Result<Vec<u8>, Failure> {
        on_a_thread(move || calls().io_read(&location)).await
    }

    pub async fn exec(
        on: Target,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Run, Failure> {
        on_a_thread(move || calls().exec(&on, &argv, stdin.as_deref())).await
    }

    pub async fn sleep(d: Duration) {
        on_a_thread(move || std::thread::sleep(d)).await
    }
}

/// The host at `DFORM_HOST`, or none (run outside dform).
pub fn connect() -> Box<dyn Calls> {
    match std::env::var("DFORM_HOST") {
        Ok(addr) => match Grpc::dial(&addr) {
            Ok(g) => Box::new(g),
            Err(e) => Box::new(Absent(format!("the host at {addr}: {e:#}"))),
        },
        Err(_) => Box::new(Absent(
            "no host: DFORM_HOST is not set (a provider calls the host only when dform runs it)"
                .into(),
        )),
    }
}

/// The `Host` service, called from a provider's handlers, at once: each
/// call runs on this client's own runtime, on a clone of its client, and
/// is waited for from the handler's thread. Only the tunnels it forwarded
/// are behind a lock, taken to insert or look one up.
pub struct Grpc {
    rt: tokio::runtime::Runtime,
    client: h::host_client::HostClient<Channel>,
    tunnels: Mutex<BTreeMap<Handle, Endpoint>>,
}

impl Grpc {
    pub fn dial(addr: &str) -> Result<Grpc> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .context("start the host client's runtime")?;
        let addr = addr.to_string();
        let channel = run(&rt, async move { dform_grpc::transport::dial(&addr).await })?;
        Ok(Grpc {
            rt,
            client: h::host_client::HostClient::new(channel)
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            tunnels: Mutex::new(BTreeMap::new()),
        })
    }

    fn tunnels(&self) -> std::sync::MutexGuard<'_, BTreeMap<Handle, Endpoint>> {
        // An insert or a lookup: a panic leaves the map whole.
        self.tunnels.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(
            h::host_client::HostClient<Channel>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send>,
        > + Send
        + 'static,
    ) -> Result<T, Error> {
        let c = self.client.clone();
        run(&self.rt, f(c))
            .map(tonic::Response::into_inner)
            .map_err(|s| Error::retryable(format!("the host: {}", s.message())))
    }
}

/// `fut` on `rt`, waited for from this thread (which may be another
/// runtime's: no nesting).
fn run<T: Send + 'static, E: Send + 'static>(
    rt: &tokio::runtime::Runtime,
    fut: impl Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, E> {
    let (tx, rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv()
        .expect("the host client's runtime answers every call")
}

macro_rules! rpc {
    ($self:ident, $method:ident, $req:expr) => {{
        let req = $req;
        $self.call(move |mut c| Box::pin(async move { c.$method(req).await }))
    }};
}

impl Calls for Grpc {
    fn open(&self, name: &str) -> Result<Opened, Error> {
        // Named by its call: what the host does to open it (a signer's
        // fetch) is the call's.
        let call = Within::here().call;
        let req = dform_grpc::traced::request(h::OpenRequest { name: name.into() }, call);
        let r = rpc!(self, open, req)?;
        conv::to_error(r.failure)?;
        Ok(Opened {
            handle: r.handle,
            endpoint: r.endpoint,
            namespace: r.namespace,
        })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error> {
        let call = req.call;
        let req = dform_grpc::traced::request(conv::from_request(req, auth, via), call);
        let r = rpc!(self, send, req)?;
        conv::response(r)
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        let r = rpc!(
            self,
            exec,
            h::ExecRequest {
                on: Some(conv::from_target(on)),
                argv: argv.to_vec(),
                stdin: stdin.map(<[u8]>::to_vec),
            }
        )?;
        conv::run(r)
    }

    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        let req = h::ReadRequest {
            location: location.into(),
        };
        let chunks = self.call(move |mut c| {
            Box::pin(async move {
                let mut s = c.read(req).await?.into_inner();
                let mut out = Vec::new();
                while let Some(chunk) = s.message().await? {
                    out.push(chunk);
                }
                Ok(tonic::Response::new(out))
            })
        })?;
        conv::read(chunks)
    }

    /// `Host.ReadVersioned` (R-172), else `Read` of a dform built before
    /// it: unversioned.
    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let req = h::ReadRequest {
            location: location.into(),
        };
        let chunks = self.call(move |mut c| {
            Box::pin(async move {
                let mut s = match c.read_versioned(req.clone()).await {
                    Err(e) if e.code() == tonic::Code::Unimplemented => {
                        c.read(req).await?.into_inner()
                    }
                    r => r?.into_inner(),
                };
                let mut out = Vec::new();
                while let Some(chunk) = s.message().await? {
                    out.push(chunk);
                }
                Ok(tonic::Response::new(out))
            })
        })?;
        conv::read_versioned(chunks)
    }

    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error> {
        let r = rpc!(
            self,
            write_file,
            h::WriteFileRequest {
                on: Some(conv::from_target(on)),
                path: path.into(),
                data: data.to_vec(),
                mode,
            }
        )?;
        conv::to_error(r.failure)
    }

    fn forward(&self, via: &Target, to: &Endpoint) -> Result<Handle, Error> {
        let r = rpc!(
            self,
            forward,
            h::ForwardRequest {
                via: Some(conv::from_target(via)),
                host: to.host.clone(),
                port: to.port.into(),
            }
        )?;
        conv::to_error(r.failure)?;
        self.tunnels().insert(r.tunnel, to.clone());
        Ok(r.tunnel)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        self.tunnels().get(&h).cloned()
    }

    fn git_commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error> {
        let r = rpc!(
            self,
            git_commit,
            h::GitCommitRequest {
                repo: repo.into(),
                branch: branch.into(),
                files: conv::from_files(files),
                message: message.into(),
            }
        )?;
        conv::to_error(r.failure)?;
        Ok(r.commit)
    }

    fn log(&self, level: Level, message: &str) {
        let req = h::LogRequest {
            level: conv::from_level(level),
            message: message.into(),
        };
        if self
            .call(move |mut c| Box::pin(async move { c.log(req).await }))
            .is_err()
        {
            eprintln!("{level:?}: {message}");
        }
    }
}
