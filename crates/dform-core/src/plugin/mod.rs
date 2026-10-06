//! The provider plugin protocol (DESIGN.org "Providers and fact plugins"),
//! as the engine speaks it: `proto/dform/v1/provider.proto`'s calls behind
//! a synchronous, completion-based trait (`backend`), whatever the
//! transport. `link` is one started provider, `providers` the stack's
//! providers and the plan and tick built from their per-resource calls,
//! `source` which provider a spec names, `wire` how values and documents
//! map to the protocol's messages, and `check` the conformance suite.

pub mod backend;
pub mod check;
pub mod link;
pub mod policy;
pub mod providers;
pub mod queue;
pub mod source;
pub mod timed;
pub mod wire;

/// The protocol's messages.
pub use dform_wire as pb;
pub use providers::{Config, Launch, Providers};
