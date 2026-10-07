//! The `Host` gRPC service (`proto/dform/host/v1/host.proto`): one
//! provider's [`Services`] served on a unix socket of its own, which dform
//! names in the provider's environment (`DFORM_HOST`). It runs on a thread
//! of its own, so a provider calling back while dform waits on its answer
//! is served; each call runs on the blocking pool (the host's HTTP, git and
//! SSH clients block), at once with the provider's others (R-142).

use crate::services::Services;
use anyhow::{Context, Result};
use dform_core::plugin::host::Calls;
use dform_grpc::host as conv;
use dform_grpc::host_pb as h;
use std::path::PathBuf;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The environment variable naming where the host listens.
pub const ENV: &str = "DFORM_HOST";

struct Service(Arc<Services>);

impl Service {
    #[allow(clippy::result_large_err)] // tonic's own error type
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Services) -> T + Send + 'static,
    ) -> Result<Response<T>, Status> {
        let s = self.0.clone();
        tokio::task::spawn_blocking(move || f(&s))
            .await
            .map(Response::new)
            .map_err(|e| Status::internal(format!("the host call failed: {e}")))
    }
}

type R<T> = Result<Response<T>, Status>;

#[tonic::async_trait]
impl h::host_server::Host for Service {
    async fn open(&self, r: Request<h::OpenRequest>) -> R<h::OpenResponse> {
        let name = r.into_inner().name;
        self.with(move |s| match s.open(&name) {
            Ok(o) => h::OpenResponse {
                failure: None,
                handle: o.handle,
                endpoint: o.endpoint,
            },
            Err(e) => h::OpenResponse {
                failure: Some(conv::from_error(e)),
                ..Default::default()
            },
        })
        .await
    }

    async fn send(&self, r: Request<h::SendRequest>) -> R<h::SendResponse> {
        let (req, auth, via) = conv::request(r.into_inner());
        self.with(move |s| conv::from_response(s.send(req, auth, via)))
            .await
    }

    type ReadStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<h::ReadChunk, Status>>,
    >;

    async fn read(&self, r: Request<h::ReadRequest>) -> R<Self::ReadStream> {
        let location = r.into_inner().location;
        let chunks = self
            .with(move |s| conv::chunks(s.files_read(&location)))
            .await?
            .into_inner();
        Ok(Response::new(tonic::codegen::tokio_stream::iter(
            chunks.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }

    async fn exec(&self, r: Request<h::ExecRequest>) -> R<h::ExecResponse> {
        let r = r.into_inner();
        let on = conv::target(r.on);
        self.with(move |s| conv::from_run(s.exec(&on, &r.argv, r.stdin.as_deref())))
            .await
    }

    async fn read_file(&self, r: Request<h::ReadFileRequest>) -> R<h::ReadFileResponse> {
        let r = r.into_inner();
        // `ssh.read`, folded into `files.read` (R-153).
        let on = conv::target(r.on);
        let port = on.port.map(|p| format!(":{p}")).unwrap_or_default();
        let location = format!("ssh://{}@{}{port}{}", on.user, on.host, r.path);
        self.with(move |s| match s.files_read(&location) {
            Ok(data) => h::ReadFileResponse {
                failure: None,
                data,
            },
            Err(f) => h::ReadFileResponse {
                failure: Some(conv::from_failure(f)),
                data: Vec::new(),
            },
        })
        .await
    }

    async fn write_file(&self, r: Request<h::WriteFileRequest>) -> R<h::WriteFileResponse> {
        let r = r.into_inner();
        let on = conv::target(r.on);
        self.with(move |s| h::WriteFileResponse {
            failure: s
                .write(&on, &r.path, &r.data, r.mode)
                .err()
                .map(conv::from_error),
        })
        .await
    }

    async fn forward(&self, r: Request<h::ForwardRequest>) -> R<h::ForwardResponse> {
        let r = r.into_inner();
        let to = conv::endpoint(&r);
        let via = conv::target(r.via);
        self.with(move |s| match s.forward(&via, &to) {
            Ok(tunnel) => h::ForwardResponse {
                failure: None,
                tunnel,
            },
            Err(e) => h::ForwardResponse {
                failure: Some(conv::from_error(e)),
                tunnel: 0,
            },
        })
        .await
    }

    async fn git_read(&self, r: Request<h::GitReadRequest>) -> R<h::GitReadResponse> {
        let r = r.into_inner();
        // `git.read`, folded into `files.read` (R-153).
        let repo = r.repo.split_once("://").map_or(r.repo.as_str(), |(_, x)| x);
        let location = format!(
            "git+https://{}//{}?ref={}",
            repo.trim_end_matches('/'),
            r.path,
            r.rev
        );
        self.with(move |s| match s.files_read(&location) {
            Ok(data) => h::GitReadResponse {
                failure: None,
                data,
            },
            Err(f) => h::GitReadResponse {
                failure: Some(conv::from_failure(f)),
                data: Vec::new(),
            },
        })
        .await
    }

    async fn git_commit(&self, r: Request<h::GitCommitRequest>) -> R<h::GitCommitResponse> {
        let r = r.into_inner();
        self.with(move |s| {
            match s.git_commit(&r.repo, &r.branch, conv::files(r.files), &r.message) {
                Ok(commit) => h::GitCommitResponse {
                    failure: None,
                    commit,
                },
                Err(e) => h::GitCommitResponse {
                    failure: Some(conv::from_error(e)),
                    commit: String::new(),
                },
            }
        })
        .await
    }

    async fn log(&self, r: Request<h::LogRequest>) -> R<h::LogResponse> {
        let r = r.into_inner();
        self.with(move |s| {
            s.log(conv::level(r.level), &r.message);
            h::LogResponse {}
        })
        .await
    }
}

/// A running `Host` service: stopped, and its socket removed, when
/// dropped.
pub struct Served {
    /// `unix:///PATH`, what the provider's `DFORM_HOST` says.
    pub address: String,
    socket: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Serve `services` on a socket of its own.
pub fn serve(services: Services) -> Result<Served> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let socket = std::env::temp_dir().join(format!(
        "dform-host-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (ready, listening) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let path = socket.clone();
    let service = Service(Arc::new(services));
    let thread = std::thread::Builder::new()
        .name("dform-host".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match tokio::net::UnixListener::bind(&path) {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                let _ = tonic::transport::Server::builder()
                    .add_service(
                        h::host_server::HostServer::new(service)
                            .max_decoding_message_size(usize::MAX)
                            .max_encoding_message_size(usize::MAX),
                    )
                    .serve_with_incoming_shutdown(
                        tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
                        async {
                            let _ = stopped.await;
                        },
                    )
                    .await;
            });
        })
        .context("start the host service")?;
    listening
        .recv()
        .context("start the host service")?
        .with_context(|| format!("listen on {}", socket.display()))?;
    Ok(Served {
        address: format!("unix://{}", socket.display()),
        socket,
        stop: Some(stop),
        thread: Some(thread),
    })
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}
