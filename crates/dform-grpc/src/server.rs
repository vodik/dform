//! The server adapter: a provider that answers one call at a time (a
//! `Handler`, such as the mock) served over gRPC (`transport::serve`).
//! A refusal is `FAILED_PRECONDITION`, a call that may have taken effect
//! `DEADLINE_EXCEEDED`, as the client classifies them back.
//!
//! An Apply runs on tokio's blocking pool, so what it says while it runs
//! (its progress events, R-130) streams as it says it, and a slow one
//! holds up no other call; its stream ends with its result. The other
//! calls are answered where they arrive.

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
        Adapter {
            handler: Arc::new(handler),
        }
    }

    #[allow(clippy::result_large_err)] // tonic's own error type
    fn call<R>(&self, call: impl Into<Call>) -> Result<Response<R>, Status>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into();
        let method = call.method();
        answer(method, self.handler.handle(call, &backend::silent)).map(Response::new)
    }
}

type Reply_<T> = Result<Response<T>, Status>;

#[tonic::async_trait]
impl<H: Handler + Send + Sync + 'static> pb::provider_server::Provider for Adapter<H> {
    async fn handshake(&self, req: Request<pb::HandshakeRequest>) -> Reply_<pb::HandshakeResponse> {
        self.call(req.into_inner())
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Reply_<pb::ConfigureResponse> {
        self.call(req.into_inner())
    }

    async fn schema(&self, req: Request<pb::SchemaRequest>) -> Reply_<pb::SchemaResponse> {
        self.call(req.into_inner())
    }

    type QueryStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<pb::Row, Status>>,
    >;

    async fn query(&self, req: Request<pb::QueryRequest>) -> Reply_<Self::QueryStream> {
        let rows: Response<Vec<pb::Row>> = self.call(req.into_inner())?;
        let rows: Vec<_> = rows.into_inner().into_iter().map(Ok).collect();
        Ok(Response::new(tonic::codegen::tokio_stream::iter(rows)))
    }

    async fn read(&self, req: Request<pb::ReadRequest>) -> Reply_<pb::ReadResponse> {
        self.call(req.into_inner())
    }

    async fn plan(&self, req: Request<pb::PlanRequest>) -> Reply_<pb::PlanResponse> {
        self.call(req.into_inner())
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
        self.call(req.into_inner())
    }

    async fn reveal(&self, req: Request<pb::RevealRequest>) -> Reply_<pb::RevealResponse> {
        self.call(req.into_inner())
    }
}

/// Serve `handler` as a provider: listen, print the handshake line, serve
/// until stdin closes (`transport::serve`).
pub fn serve<H: Handler + Send + Sync + 'static>(handler: H) -> anyhow::Result<()> {
    crate::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(Adapter::new(handler))
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
        ),
    )
}
