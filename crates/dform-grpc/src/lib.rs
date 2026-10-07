//! The process backend: the provider protocol over gRPC. A provider is an
//! executable dform spawns (`spawn`), which prints a handshake line and
//! serves `proto/dform/v1/provider.proto` on TCP or a unix socket
//! (`transport`); `client` implements dform-core's `Provider` over it, and
//! `server` serves a `Handler` (the mock) as such an executable. `host`
//! is the host's types as the additive `Host` service's messages (R-13b).

pub mod client;
pub mod host;
pub mod server;
pub mod spawn;
pub mod transport;

/// The protocol's messages (`dform-wire`) and its service.
#[allow(clippy::result_large_err)] // tonic's own error type
pub mod pb {
    pub use dform_wire::*;
    tonic::include_proto!("dform.v1");
}

/// The host's services beside the protocol (`proto/dform/host/v1`,
/// R-13b): `Host`, which dform serves a native provider, and `Manifest`,
/// which a provider serves.
#[allow(clippy::result_large_err)] // tonic's own error type
pub mod host_pb {
    tonic::include_proto!("dform.host.v1");
}
