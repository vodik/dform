//! The server adapter: a provider that answers one call at a time (a
//! `Handler`, such as the mock) served over gRPC (`transport::serve`).
//! A refusal is `FAILED_PRECONDITION`, a call that may have taken effect
//! `DEADLINE_EXCEEDED`, as the client classifies them back.

use crate::pb;
use dform_core::plugin::backend::{Call, CallError, Handler, Reply};
use tonic::{Request, Response, Status};

/// `handler` behind the protocol's service.
pub struct Adapter<H> {
    handler: H,
}

impl<H: Handler + Send + Sync + 'static> Adapter<H> {
    pub fn new(handler: H) -> Adapter<H> {
        Adapter { handler }
    }

    #[allow(clippy::result_large_err)] // tonic's own error type
    fn call<R>(&self, call: impl Into<Call>) -> Result<Response<R>, Status>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into();
        let method = call.method();
        match self.handler.handle(call) {
            Ok(reply) => R::try_from(reply).map(Response::new).map_err(|other| {
                Status::internal(format!(
                    "a {method} call answered with a {} reply",
                    other.method()
                ))
            }),
            Err(CallError::Refused(m)) => Err(Status::failed_precondition(m)),
            Err(CallError::MaybeApplied(m)) => Err(Status::deadline_exceeded(m)),
            Err(CallError::Crashed(m)) => Err(Status::unavailable(m)),
        }
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

    async fn apply(&self, req: Request<pb::ApplyRequest>) -> Reply_<pb::ApplyResponse> {
        self.call(req.into_inner())
    }

    async fn import(&self, req: Request<pb::ImportRequest>) -> Reply_<pb::ImportResponse> {
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
