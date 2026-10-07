//! The server adapter: a provider that answers a call at a time (a
//! `Handler`, such as the mock) served over gRPC (`transport::serve`).
//! A refusal is `FAILED_PRECONDITION`, a call that may have taken effect
//! `DEADLINE_EXCEEDED`, as the client classifies them back.
//!
//! Every call runs on tokio's blocking pool, so the calls dform sends at
//! once (a refresh's Reads, a plan's Plans, `--parallel`'s Applies) run at
//! once and a slow one holds up no other (R-142); an Apply's progress
//! events (R-130) stream as it says them, and its stream ends with its
//! result.

use crate::pb;
use dform_core::plugin::backend::{self, Call, CallError, Handler, Reply};
use std::sync::Arc;
use tonic::codegen::tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status};

/// `handler` behind the protocol's service.
pub struct Adapter<H> {
    handler: Arc<H>,
}

/// A failure as its gRPC status.
pub fn status(e: CallError) -> Status {
    match e {
        CallError::Refused(m) => Status::failed_precondition(m),
        CallError::MaybeApplied(m) => Status::deadline_exceeded(m),
        CallError::Crashed(m) => Status::unavailable(m),
    }
}

/// `reply`, as the answer to a `method` call.
#[allow(clippy::result_large_err)] // tonic's own error type
pub fn answer<R>(method: &str, reply: Result<Reply, CallError>) -> Result<R, Status>
where
    R: TryFrom<Reply, Error = Reply>,
{
    R::try_from(reply.map_err(status)?).map_err(|other| {
        Status::internal(format!(
            "a {method} call answered with a {} reply",
            other.method()
        ))
    })
}

/// An Apply's stream: its events, then its result.
pub type ApplyStream = UnboundedReceiverStream<Result<pb::ApplyEvent, Status>>;

/// Run the Apply `req` with `handle` on the blocking pool, its events and
/// then its result (or its failure) on the stream returned.
pub fn apply_stream(
    req: pb::ApplyRequest,
    handle: impl FnOnce(Call, backend::Progress) -> Result<Reply, CallError> + Send + 'static,
) -> ApplyStream {
    use pb::apply_event::Kind;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let progress = |e: pb::Event| {
            let _ = tx.send(Ok(pb::ApplyEvent {
                kind: Some(Kind::Event(e)),
            }));
        };
        let reply = handle(Call::Apply(req), &progress);
        let last = answer::<pb::ApplyResponse>("Apply", reply).map(|r| pb::ApplyEvent {
            kind: Some(Kind::Result(r)),
        });
        let _ = tx.send(last);
    });
    UnboundedReceiverStream::new(rx)
}

/// An Apply's stream that is its result alone (a provider that says
/// nothing while it applies).
pub fn apply_answer(result: Result<pb::ApplyResponse, Status>) -> ApplyStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = tx.send(result.map(|r| pb::ApplyEvent {
        kind: Some(pb::apply_event::Kind::Result(r)),
    }));
    UnboundedReceiverStream::new(rx)
}

impl<H: Handler + Send + Sync + 'static> Adapter<H> {
    pub fn new(handler: H) -> Adapter<H> {
        Adapter::shared(Arc::new(handler))
    }

    /// The adapter of a handler held elsewhere too (its `Files`).
    pub fn shared(handler: Arc<H>) -> Adapter<H> {
        Adapter { handler }
    }

    /// `call` on the blocking pool: the transport's runtime is a single
    /// thread, and a handler blocks (on its own I/O, on the host), so the
    /// calls dform sends at once run at once.
    #[allow(clippy::result_large_err)] // tonic's own error type
    async fn call<R>(&self, call: impl Into<Call>) -> Result<Response<R>, Status>
    where
        R: TryFrom<Reply, Error = Reply> + Send + 'static,
    {
        let call = call.into();
        let method = call.method();
        let handler = self.handler.clone();
        let answered = move || answer(method, handler.handle(call, &backend::silent));
        tokio::task::spawn_blocking(answered)
            .await
            .map_err(|e| Status::internal(format!("the {method} call failed: {e}")))?
            .map(Response::new)
    }
}

type Reply_<T> = Result<Response<T>, Status>;

#[tonic::async_trait]
impl<H: Handler + Send + Sync + 'static> pb::provider_server::Provider for Adapter<H> {
    async fn handshake(&self, req: Request<pb::HandshakeRequest>) -> Reply_<pb::HandshakeResponse> {
        self.call(req.into_inner()).await
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Reply_<pb::ConfigureResponse> {
        self.call(req.into_inner()).await
    }

    async fn schema(&self, req: Request<pb::SchemaRequest>) -> Reply_<pb::SchemaResponse> {
        self.call(req.into_inner()).await
    }

    type QueryStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<pb::Row, Status>>,
    >;

    async fn query(&self, req: Request<pb::QueryRequest>) -> Reply_<Self::QueryStream> {
        let rows: Response<Vec<pb::Row>> = self.call(req.into_inner()).await?;
        let rows: Vec<_> = rows.into_inner().into_iter().map(Ok).collect();
        Ok(Response::new(tonic::codegen::tokio_stream::iter(rows)))
    }

    async fn read(&self, req: Request<pb::ReadRequest>) -> Reply_<pb::ReadResponse> {
        self.call(req.into_inner()).await
    }

    async fn plan(&self, req: Request<pb::PlanRequest>) -> Reply_<pb::PlanResponse> {
        self.call(req.into_inner()).await
    }

    type ApplyStream = ApplyStream;

    async fn apply(&self, req: Request<pb::ApplyRequest>) -> Reply_<Self::ApplyStream> {
        let handler = self.handler.clone();
        Ok(Response::new(apply_stream(
            req.into_inner(),
            move |call, progress| handler.handle(call, progress),
        )))
    }

    async fn import(&self, req: Request<pb::ImportRequest>) -> Reply_<pb::ImportResponse> {
        self.call(req.into_inner()).await
    }

    async fn reveal(&self, req: Request<pb::RevealRequest>) -> Reply_<pb::RevealResponse> {
        self.call(req.into_inner()).await
    }
}

/// Serve `handler` as a provider: listen, print the handshake line, serve
/// until stdin closes (`transport::serve`). A handler that reads location
/// schemes declares them in its `Manifest` and serves them as `Io`
/// (R-153, R-155; and as `Files`, its name before).
pub fn serve<H: Handler + Send + Sync + 'static>(handler: H) -> anyhow::Result<()> {
    let schemes = handler.schemes();
    if !schemes.is_empty() {
        let handler = Arc::new(handler);
        let reads = handler.clone();
        return serve_shared(
            handler,
            Declares {
                imports: Vec::new(),
                schemes,
                read: Some(Arc::new(move |l: &str| reads.read_location(l))),
            },
        );
    }
    crate::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(Adapter::new(handler))
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
        ),
    )
}

/// A provider's reader of the location schemes it declares (R-153).
pub type Read =
    Arc<dyn Fn(&str) -> Result<Vec<u8>, dform_core::plugin::host::Failure> + Send + Sync>;

/// What a provider declares beside the protocol (`Manifest`, R-13b): the
/// interfaces it imports, and the location schemes it reads, which it
/// serves as `Io` (R-153, R-155).
#[derive(Clone, Default)]
pub struct Declares {
    pub imports: Vec<String>,
    pub schemes: Vec<String>,
    pub read: Option<Read>,
}

#[tonic::async_trait]
impl crate::host_pb::manifest_server::Manifest for Declares {
    async fn manifest(
        &self,
        _: Request<crate::host_pb::ManifestRequest>,
    ) -> Result<Response<crate::host_pb::ManifestResponse>, Status> {
        Ok(Response::new(crate::host_pb::ManifestResponse {
            imports: self.imports.clone(),
            schemes: self.schemes.clone(),
        }))
    }
}

/// The `Io` service (R-155; `Files` its name before): the declared
/// schemes' reads, on the blocking pool.
#[derive(Clone)]
struct Io(Read);

type ReadStream = tonic::codegen::tokio_stream::Iter<
    std::vec::IntoIter<Result<crate::host_pb::ReadChunk, Status>>,
>;

impl Io {
    async fn read_location(&self, location: String) -> Result<Response<ReadStream>, Status> {
        let read = self.0.clone();
        let chunks = tokio::task::spawn_blocking(move || crate::host::chunks(read(&location)))
            .await
            .map_err(|e| Status::internal(format!("the read failed: {e}")))?;
        Ok(Response::new(tonic::codegen::tokio_stream::iter(
            chunks.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }
}

#[tonic::async_trait]
impl crate::host_pb::io_server::Io for Io {
    type ReadStream = ReadStream;

    async fn read(
        &self,
        r: Request<crate::host_pb::ReadRequest>,
    ) -> Result<Response<ReadStream>, Status> {
        self.read_location(r.into_inner().location).await
    }
}

#[tonic::async_trait]
impl crate::host_pb::files_server::Files for Io {
    type ReadStream = ReadStream;

    async fn read(
        &self,
        r: Request<crate::host_pb::ReadRequest>,
    ) -> Result<Response<ReadStream>, Status> {
        self.read_location(r.into_inner().location).await
    }
}

/// [`serve`], with `Manifest` saying what it `declares`, and `Io` its
/// reader of the schemes it declares.
pub fn serve_declaring<H: Handler + Send + Sync + 'static>(
    handler: H,
    declares: Declares,
) -> anyhow::Result<()> {
    serve_shared(Arc::new(handler), declares)
}

fn serve_shared<H: Handler + Send + Sync + 'static>(
    handler: Arc<H>,
    declares: Declares,
) -> anyhow::Result<()> {
    let io = declares.read.clone().map(|r| {
        crate::host_pb::io_server::IoServer::new(Io(r)).max_encoding_message_size(usize::MAX)
    });
    let files = declares.read.clone().map(|r| {
        crate::host_pb::files_server::FilesServer::new(Io(r)).max_encoding_message_size(usize::MAX)
    });
    crate::transport::serve(
        tonic::transport::Server::builder()
            .add_service(
                pb::provider_server::ProviderServer::new(Adapter::shared(handler))
                    .max_decoding_message_size(usize::MAX)
                    .max_encoding_message_size(usize::MAX),
            )
            .add_service(crate::host_pb::manifest_server::ManifestServer::new(
                declares,
            ))
            .add_optional_service(io)
            .add_optional_service(files),
    )
}
