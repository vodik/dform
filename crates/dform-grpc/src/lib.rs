//! The provider protocol over gRPC: a provider is an executable dform
//! spawns (`spawn`), which prints a handshake line and serves
//! `proto/dform/v1/provider.proto` (`client`), on TCP or a unix socket
//! (`transport`).

pub mod client;
pub mod spawn;
pub mod transport;

/// The protocol's messages (`dform-wire`) and its service.
pub mod pb {
    pub use dform_wire::*;
    tonic::include_proto!("dform.v1");
}
