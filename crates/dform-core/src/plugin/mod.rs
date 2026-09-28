//! The provider plugin protocol (DESIGN.org "Providers and fact plugins"):
//! a provider is an executable dform spawns (`spawn`), which prints a
//! handshake line and serves `proto/dform/v1/provider.proto` over gRPC
//! (`client`), on TCP or a unix socket (`transport`). `wire` maps values
//! and documents to the protocol's messages.

pub mod check;
pub mod providers;
pub mod wire;

pub use dform_grpc::{client, pb, spawn, transport};
pub use providers::{Config, Providers};
