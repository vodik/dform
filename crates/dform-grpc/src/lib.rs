//! The process backend: the provider protocol over gRPC. A provider is an
//! executable dform spawns (`spawn`), which prints a handshake line and
//! serves `proto/dform/v1/provider.proto` on TCP or a unix socket
//! (`transport`); `client` implements dform-core's `Provider` over it, and
//! `server` serves a `Handler` (the mock) as such an executable.

pub mod client;
pub mod server;
pub mod spawn;
pub mod transport;

/// The protocol's messages (`dform-wire`) and its service.
pub mod pb {
    pub use dform_wire::*;
    tonic::include_proto!("dform.v1");
}
