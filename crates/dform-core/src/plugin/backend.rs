//! A provider as the engine sees it: the protocol's calls without a
//! transport (WORK.org phase 7, "Split the core from the provider
//! transport").
//!
//! A [`Call`] and a [`Reply`] are the protocol's request and response
//! messages themselves (`dform-wire`), with no mapping layer. The trait is
//! synchronous and completion-based: [`Provider::submit`] returns at once
//! with a [`Ticket`], and [`Provider::next_completed`] blocks until some
//! submitted call has an answer, telling its caller each [`pb::Event`] a
//! call in flight sends meanwhile (an Apply's progress, R-130). The
//! executor stays single-threaded: it submits while actions are ready and
//! fewer than `--parallel` are in flight, then takes the next completion.
//!
//! Three backends implement it: the process backend (`dform-grpc`, a
//! spawned executable over gRPC: the CLI's), and the direct and wire
//! backends ([`super::queue`], a provider linked in, the second encoding
//! and decoding every message through prost). A provider that answers
//! each call as a function of it implements [`Handler`]; the queue and
//! the gRPC server adapter serve one.

use super::pb;

/// The protocol version this dform speaks.
pub const VERSION: u32 = 1;

/// This build of dform: its version and the git commit it was built from
/// (`build.rs`; `unknown` built outside a git checkout). Its built-in
/// providers' handshakes carry it, and dform refuses one built otherwise.
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("DFORM_COMMIT"));

/// The mock's name (`dform-mock`), as its handshake gives it.
pub const FAKECLOUD: &str = "fakecloud";
/// The Kubernetes provider's name (`dform-k8s`).
pub const KUBERNETES: &str = "k8s";
/// The providers built with dform, by the names their handshakes give:
/// one whose [`BUILD`] differs is stale.
pub const BUILT_IN: [&str; 2] = [FAKECLOUD, KUBERNETES];

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
    Reveal(pb::RevealRequest) -> pb::RevealResponse,
}

/// Where a call says how it is going (R-130): an Apply's provider sends an
/// event each time its own view of the object changes (a poll that saw a
/// new status, a retry), never on a timer. Its answer is the reply, not an
/// event.
pub type Progress<'a> = &'a (dyn Fn(pb::Event) + Sync);

/// A call nobody is told the progress of.
pub fn silent(_: pb::Event) {}

/// An event about `address`: its status word, as the provider's API says
/// it, and a message for the log.
pub fn event(address: impl Into<String>, status: Option<&str>, message: Option<&str>) -> pb::Event {
    pb::Event {
        address: address.into(),
        status: status.map(str::to_string),
        message: message.map(str::to_string),
    }
}

/// Stops a provider's process from another thread than the one calling
/// it ([`Provider::stopper`]).
pub type Stop = Box<dyn FnOnce() + Send>;

/// A provider, reached through some transport.
pub trait Provider {
    /// Start `call`. Returns at once.
    fn submit(&mut self, call: Call) -> Ticket;

    /// Block until a submitted call has an answer: its ticket and the
    /// answer. Each event a call in flight sends meanwhile is given to
    /// `events` with that call's ticket, as it comes. Only called with a
    /// call in flight.
    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>);

    /// Whether the provider is gone (its process exited). The end of a
    /// tick skips a provider that is.
    fn is_dead(&mut self) -> bool {
        false
    }

    /// How to stop the provider while a call to it is stuck: a process
    /// backend's kills its process, which fails the stuck call (its
    /// connection breaks) and so frees the thread waiting in
    /// [`Provider::next_completed`]. `None` for a provider in this
    /// process.
    fn stopper(&self) -> Option<Stop> {
        None
    }
}

/// A provider that answers each call as a function of it: what the direct
/// and wire backends link in, and what the gRPC server adapter serves,
/// which calls `handle` from several threads at once (R-142).
pub trait Handler {
    /// Answer `call`, saying how it goes on `progress` while it runs.
    fn handle(&self, call: Call, progress: Progress) -> Result<Reply, CallError>;

    /// Whether the provider is gone (it crashed).
    fn is_dead(&self) -> bool {
        false
    }

    /// The location schemes it reads (R-153): its manifest declares them,
    /// and dform routes a read of one to [`Handler::read_location`].
    fn schemes(&self) -> Vec<String> {
        Vec::new()
    }

    /// The bytes at `location`, of a scheme it declares; not there yet is
    /// `Failure::NotYet`.
    fn read_location(&self, location: &str) -> Result<Vec<u8>, super::host::Failure> {
        Err(
            super::host::Error::fatal(format!("{location}: this provider reads no location"))
                .into(),
        )
    }

    /// The document at `location` with its version, when the source keeps
    /// versions (a secret manager's, R-172); by default
    /// [`Handler::read_location`]'s bytes, unversioned.
    fn read_versioned(
        &self,
        location: &str,
    ) -> Result<crate::files::Document, super::host::Failure> {
        self.read_location(location)
            .map(crate::files::Document::from)
    }
}
