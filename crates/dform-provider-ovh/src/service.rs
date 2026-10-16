//! The gRPC service: the provider (`Provider`, a `Handler`) behind
//! dform-grpc's adapter, each call on tokio's blocking pool, so an
//! instance's Create, which waits minutes for it to run, holds up no other
//! call; the adapter numbers the sessions.

use crate::ovh::Provider;
use dform_grpc::pb;

/// Serve as a provider (`dform_grpc::transport`), and exit when stdin
/// closes.
pub fn serve() -> anyhow::Result<()> {
    dform_grpc::transport::serve(
        tonic::transport::Server::builder().add_service(
            pb::provider_server::ProviderServer::new(dform_grpc::server::Adapter::new(
                dform_sdk::native::Blocking(Provider::default()),
            ))
            .max_decoding_message_size(usize::MAX)
            .max_encoding_message_size(usize::MAX),
        ),
    )
}
