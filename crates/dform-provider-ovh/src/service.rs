//! The gRPC service: the provider (`Ovh`, a `Handler`) behind the
//! protocol, each call on tokio's blocking pool, so an instance's Create,
//! which waits minutes for it to run, holds up no other call. A refusal is
//! `FAILED_PRECONDITION`, a call that may have taken effect
//! `DEADLINE_EXCEEDED`, as dform-grpc's client classifies them.

use crate::ovh::Ovh;
use dform_core::plugin::backend::{self, Call, Handler, Reply};
use dform_grpc::pb;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct Service {
    ovh: Arc<Ovh>,
}

type Answer<T> = Result<Response<T>, Status>;

impl Service {
    pub fn new(ovh: Ovh) -> Service {
        Service { ovh: Arc::new(ovh) }
    }

    #[allow(clippy::result_large_err)] // tonic's own error type
    async fn call<R>(&self, call: impl Into<Call>) -> Answer<R>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into();
        let method = call.method();
        let ovh = self.ovh.clone();
        let reply = tokio::task::spawn_blocking(move || ovh.handle(call, &backend::silent))
            .await
            .map_err(|e| Status::internal(format!("the {method} call panicked: {e}")))?;
        dform_grpc::server::answer(method, reply).map(Response::new)
    }
}

#[tonic::async_trait]
impl pb::provider_server::Provider for Service {
    async fn handshake(&self, req: Request<pb::HandshakeRequest>) -> Answer<pb::HandshakeResponse> {
        self.call(req.into_inner()).await
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Answer<pb::ConfigureResponse> {
        self.call(req.into_inner()).await
    }

    async fn schema(&self, req: Request<pb::SchemaRequest>) -> Answer<pb::SchemaResponse> {
        self.call(req.into_inner()).await
    }

    type QueryStream = tonic::codegen::tokio_stream::Iter<
        std::vec::IntoIter<std::result::Result<pb::Row, Status>>,
    >;

    async fn query(&self, req: Request<pb::QueryRequest>) -> Answer<Self::QueryStream> {
        let rows: Response<Vec<pb::Row>> = self.call(req.into_inner()).await?;
        let rows: Vec<_> = rows.into_inner().into_iter().map(Ok).collect();
        Ok(Response::new(tonic::codegen::tokio_stream::iter(rows)))
    }

    async fn read(&self, req: Request<pb::ReadRequest>) -> Answer<pb::ReadResponse> {
        self.call(req.into_inner()).await
    }

    async fn plan(&self, req: Request<pb::PlanRequest>) -> Answer<pb::PlanResponse> {
        self.call(req.into_inner()).await
    }

    type ApplyStream = dform_grpc::server::ApplyStream;

    /// What the API says of the object as it is made (an instance's
    /// `BUILD`, then `ACTIVE`) streams before the result (R-130).
    async fn apply(&self, req: Request<pb::ApplyRequest>) -> Answer<Self::ApplyStream> {
        let ovh = self.ovh.clone();
        Ok(Response::new(dform_grpc::server::apply_stream(
            req.into_inner(),
            move |call, progress| ovh.handle(call, progress),
        )))
    }

    async fn import(&self, req: Request<pb::ImportRequest>) -> Answer<pb::ImportResponse> {
        self.call(req.into_inner()).await
    }

    async fn reveal(&self, req: Request<pb::RevealRequest>) -> Answer<pb::RevealResponse> {
        self.call(req.into_inner()).await
    }
}

/// Serve as a provider (`dform_grpc::transport`), and exit when stdin
/// closes.
pub fn serve() -> anyhow::Result<()> {
    dform_grpc::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(Service::new(Ovh::new()))
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
        ),
    )
}
