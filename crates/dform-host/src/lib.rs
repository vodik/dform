//! dform's side of `dform:host` (wit/host/dform-host.wit, R-13b): the
//! credentialed transports a provider reaches through dform, so that a
//! provider does protocol logic only and never holds a credential.
//!
//! [`services::Services`] is one provider's host (its grants, the
//! credentials it opened, HTTP with TLS by the host, git through gix, SSH
//! behind a trait, its log). [`grpc`] serves it to a native provider as
//! the `Host` service, and [`launch::Launcher`] starts providers with it.
//! With the `wasm` feature (experimental), [`wasm`] runs a component
//! provider in wasmtime with the same interfaces as its imports.

/// The host's git is dform's (`dform_core::git`, R-103).
pub use dform_core::git;
pub mod grpc;
/// The host's HTTP is dform's one client (`dform_core::http`, R-103).
pub use dform_core::http;
pub mod launch;
pub mod services;
pub mod ssh;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use launch::Launcher;

/// Whether this build has the wasm host (`dform version` says).
pub const WASM: bool = cfg!(feature = "wasm");
