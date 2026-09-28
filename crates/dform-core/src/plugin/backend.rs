//! A provider as the engine sees it: the protocol's calls without a
//! transport (WORK.org phase 7, "Split the core from the provider
//! transport").
//!
//! A [`Call`] and a [`Reply`] are the protocol's request and response
//! messages themselves (`dform-wire`), with no mapping layer. The trait is
//! synchronous and completion-based: [`Provider::submit`] returns at once
//! with a [`Ticket`], and [`Provider::next_completed`] blocks until some
//! submitted call has an answer. The executor stays single-threaded: it
//! submits while actions are ready and fewer than `--parallel` are in
//! flight, then takes the next completion.
//!
//! Three backends implement it: the process backend (`dform-grpc`, a
//! spawned executable over gRPC: the CLI's), and the direct and wire
//! backends ([`super::queue`], a provider linked in, the second encoding
//! and decoding every message through prost). A provider that answers
//! calls one at a time implements [`Handler`]; the queue and the gRPC
//! server adapter serve one.

use super::pb;

/// The protocol version this dform speaks.
pub const VERSION: u32 = 1;

/// A submitted call, as its backend names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(pub u64);

/// How a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The provider refused the call: nothing changed.
    Refused(String),
    /// The call may have taken effect, but no answer came
    /// (`DEADLINE_EXCEEDED` over gRPC).
    MaybeApplied(String),
    /// The provider died during the call.
    Crashed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Refused(m) | CallError::MaybeApplied(m) | CallError::Crashed(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for CallError {}

/// The protocol's methods: a `Call` variant carries the request, the
/// `Reply` variant of the same name the response. Query's reply is its
/// stream of rows, collected.
macro_rules! methods {
    ($($m:ident($req:ty) -> $resp:ty,)*) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum Call {
            $($m($req),)*
        }

        #[derive(Debug, Clone, PartialEq)]
        pub enum Reply {
            $($m($resp),)*
        }

        impl Call {
            /// The method's name, for messages.
            pub fn method(&self) -> &'static str {
                match self {
                    $(Call::$m(_) => stringify!($m),)*
                }
            }
        }

        impl Reply {
            pub fn method(&self) -> &'static str {
                match self {
                    $(Reply::$m(_) => stringify!($m),)*
                }
            }
        }

        $(
            impl From<$req> for Call {
                fn from(r: $req) -> Call {
                    Call::$m(r)
                }
            }

            impl From<$resp> for Reply {
                fn from(r: $resp) -> Reply {
                    Reply::$m(r)
                }
            }

            impl TryFrom<Reply> for $resp {
                type Error = Reply;
                fn try_from(r: Reply) -> Result<$resp, Reply> {
                    match r {
                        Reply::$m(x) => Ok(x),
                        other => Err(other),
                    }
                }
            }
        )*
    };
}

methods! {
    Handshake(pb::HandshakeRequest) -> pb::HandshakeResponse,
    Configure(pb::ConfigureRequest) -> pb::ConfigureResponse,
    Schema(pb::SchemaRequest) -> pb::SchemaResponse,
    Query(pb::QueryRequest) -> Vec<pb::Row>,
    Read(pb::ReadRequest) -> pb::ReadResponse,
    Plan(pb::PlanRequest) -> pb::PlanResponse,
    Apply(pb::ApplyRequest) -> pb::ApplyResponse,
    Import(pb::ImportRequest) -> pb::ImportResponse,
}

/// A provider, reached through some transport.
pub trait Provider {
    /// Start `call`. Returns at once.
    fn submit(&mut self, call: Call) -> Ticket;

    /// Block until a submitted call has an answer: its ticket and the
    /// answer. Only called with a call in flight.
    fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>);

    /// Whether the provider is gone (its process exited). The end of a
    /// tick skips a provider that is.
    fn is_dead(&mut self) -> bool {
        false
    }
}

/// A provider that answers one call at a time: what the direct and wire
/// backends link in, and what the gRPC server adapter serves.
pub trait Handler {
    fn handle(&self, call: Call) -> Result<Reply, CallError>;
}
