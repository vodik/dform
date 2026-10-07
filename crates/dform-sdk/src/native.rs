//! A provider built natively: the protocol served over gRPC
//! (`dform-grpc`), its `Manifest` beside it, and the host called back over
//! the `Host` service at `DFORM_HOST`.

use crate::Absent;
use anyhow::{Context, Result};
use dform_core::plugin::backend::Handler;
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Handle, HttpRequest, HttpResponse, Level, Opened,
    Run, Target,
};
use dform_grpc::host as conv;
use dform_grpc::host_pb as h;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use tonic::transport::Channel;

/// What a native provider says it uses (it runs as the user: a
/// declaration, not a sandbox).
struct Declared(Vec<String>);

#[tonic::async_trait]
impl h::manifest_server::Manifest for Declared {
    async fn manifest(
        &self,
        _: tonic::Request<h::ManifestRequest>,
    ) -> Result<tonic::Response<h::ManifestResponse>, tonic::Status> {
        Ok(tonic::Response::new(h::ManifestResponse {
            imports: self.0.clone(),
        }))
    }
}

/// Serve `handler` over gRPC, declaring `uses`, until stdin closes.
pub fn serve<H: Handler + Send + Sync + 'static>(handler: H, uses: &[&str]) -> Result<()> {
    use dform_grpc::pb::provider_server::ProviderServer;
    dform_grpc::transport::serve(
        tonic::transport::Server::builder()
            .add_service(
                ProviderServer::new(dform_grpc::server::Adapter::new(handler))
                    .max_decoding_message_size(usize::MAX)
                    .max_encoding_message_size(usize::MAX),
            )
            .add_service(h::manifest_server::ManifestServer::new(Declared(
                uses.iter().map(|s| s.to_string()).collect(),
            ))),
    )
}

/// `provider!`'s `main`.
pub fn main<H: Handler + Send + Sync + 'static>(
    make: impl FnOnce() -> H,
    uses: &[&str],
) -> std::process::ExitCode {
    match serve(make(), uses) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_PKG_NAME"));
            std::process::ExitCode::FAILURE
        }
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
        let r = rpc!(self, open, h::OpenRequest { name: name.into() })?;
        conv::to_error(r.failure)?;
        Ok(Opened {
            handle: r.handle,
            endpoint: r.endpoint,
        })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error> {
        let r = rpc!(self, send, conv::from_request(req, auth, via))?;
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

    fn read(&self, on: &Target, path: &str) -> Result<Vec<u8>, Failure> {
        let r = rpc!(
            self,
            read_file,
            h::ReadFileRequest {
                on: Some(conv::from_target(on)),
                path: path.into(),
            }
        )?;
        conv::to_failure(r.failure)?;
        Ok(r.data)
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

    fn git_read(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error> {
        let r = rpc!(
            self,
            git_read,
            h::GitReadRequest {
                repo: repo.into(),
                rev: rev.into(),
                path: path.into(),
            }
        )?;
        conv::to_error(r.failure)?;
        Ok(r.data)
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
