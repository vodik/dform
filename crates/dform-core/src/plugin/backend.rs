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
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

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

/// A configured instance of a provider: what Configure answers, and what
/// every call after it names. A provider keeps one configuration per
/// session, so one process serves several (the deployments of a tree,
/// `use aws as east` beside `use aws as west`), their calls in flight
/// together. [`Session::NONE`] is no session: the Handshake's, and a
/// Configure's that opens one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Session(pub u64);

impl Session {
    pub const NONE: Session = Session(0);
}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
    Health(pb::HealthRequest) -> pb::HealthResponse,
}

impl Call {
    /// The session the call is made in: none for a Handshake, and for a
    /// Configure that opens one.
    pub fn session(&self) -> Session {
        Session(match self {
            Call::Handshake(_) => 0,
            Call::Configure(r) => r.session,
            Call::Schema(r) => r.session,
            Call::Query(r) => r.session,
            Call::Read(r) => r.session,
            Call::Plan(r) => r.session,
            Call::Apply(r) => r.session,
            Call::Import(r) => r.session,
            Call::Reveal(r) => r.session,
            Call::Health(r) => r.session,
        })
    }

    /// The call, made in `session` (a Handshake names none).
    pub fn in_session(mut self, session: Session) -> Call {
        let at = match &mut self {
            Call::Handshake(_) => return self,
            Call::Configure(r) => &mut r.session,
            Call::Schema(r) => &mut r.session,
            Call::Query(r) => &mut r.session,
            Call::Read(r) => &mut r.session,
            Call::Plan(r) => &mut r.session,
            Call::Apply(r) => &mut r.session,
            Call::Import(r) => &mut r.session,
            Call::Reveal(r) => &mut r.session,
            Call::Health(r) => &mut r.session,
        };
        *at = session.0;
        self
    }
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

/// An object's health as its provider judges it now (R-203): `state`, and
/// a reason in one line, as the API says it.
pub fn health(state: pb::HealthState, reason: impl Into<String>) -> pb::Health {
    pb::Health {
        state: state as i32,
        reason: reason.into(),
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
/// which calls `handle` from several threads at once (R-142). What serves
/// it numbers the sessions ([`Sessions`]): it keeps one configuration per
/// session ([`Configured`]) and answers each call with its session's.
pub trait Handler {
    /// Answer `call`, made in `session`, saying how it goes on `progress`
    /// while it runs. A Configure's session is the one it configures (a new
    /// one when it opens one: its answer names it, whatever the handler
    /// answers); a Handshake's is none.
    fn handle(&self, session: Session, call: Call, progress: Progress) -> Result<Reply, CallError>;

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

/// The sessions of a [`Handler`], as what serves it numbers them (the gRPC
/// server, the component, the direct and wire backends): a Configure that
/// names none opens the next, from 1.
#[derive(Debug, Default)]
pub struct Sessions {
    last: AtomicU64,
}

impl Sessions {
    /// The session `call` is made in: the one it names, else, for a
    /// Configure, a new one.
    pub fn of(&self, call: &Call) -> Session {
        match call {
            Call::Configure(_) => self.configure(call.session()),
            call => call.session(),
        }
    }

    /// The session a Configure naming `named` configures: that one, else
    /// a new one.
    pub fn configure(&self, named: Session) -> Session {
        match named {
            Session::NONE => Session(self.last.fetch_add(1, Ordering::Relaxed) + 1),
            s => s,
        }
    }

    /// `handler`'s answer to `call`, in its session ([`Sessions::of`]): the
    /// session the HTTP requests it sends while it runs name ([`current`]),
    /// and the one a Configure's answer names.
    pub fn serve<H: Handler + ?Sized>(
        &self,
        handler: &H,
        call: Call,
        progress: Progress,
    ) -> Result<Reply, CallError> {
        let session = self.of(&call);
        let _in = In::enter(session);
        let mut reply = handler.handle(session, call, progress)?;
        if let Reply::Configure(r) = &mut reply {
            r.session = session.0;
        }
        Ok(reply)
    }
}

thread_local! {
    /// The session of the call this thread is answering.
    static CURRENT: Cell<Session> = const { Cell::new(Session::NONE) };
}

/// The session of the call this thread is answering ([`Sessions::serve`]),
/// for the host calls it makes: none outside a call, and on a thread of
/// the provider's own.
pub fn current() -> Session {
    CURRENT.with(|c| c.get())
}

/// This thread answering a call of a session, until dropped.
struct In(Session);

impl In {
    fn enter(session: Session) -> In {
        In(CURRENT.with(|c| c.replace(session)))
    }
}

impl Drop for In {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.0));
    }
}

/// A provider's configuration of each session: what a Configure sets, and
/// what each call after it is answered with. A call of a session not
/// configured (yet) finds none.
#[derive(Debug)]
pub struct Configured<T> {
    by: RwLock<BTreeMap<Session, Arc<T>>>,
}

impl<T> Default for Configured<T> {
    fn default() -> Configured<T> {
        Configured {
            by: Default::default(),
        }
    }
}

impl<T> Configured<T> {
    fn by(&self) -> RwLockReadGuard<'_, BTreeMap<Session, Arc<T>>> {
        self.by.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Configure `session` as `t`: in place of what configured it before.
    pub fn set(&self, session: Session, t: T) -> Arc<T> {
        let t = Arc::new(t);
        self.by
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session, t.clone());
        t
    }

    /// `session`'s configuration, if it has one.
    pub fn get(&self, session: Session) -> Option<Arc<T>> {
        self.by().get(&session).cloned()
    }

    /// `session`'s configuration, else `new()`, which configures it.
    pub fn get_or_set(&self, session: Session, new: impl FnOnce() -> T) -> Arc<T> {
        if let Some(t) = self.get(session) {
            return t;
        }
        self.by
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_insert_with(|| Arc::new(new()))
            .clone()
    }

    /// The configuration of the session opened last: what answers what
    /// names no session (a location read, R-153).
    pub fn latest(&self) -> Option<Arc<T>> {
        self.by().values().next_back().cloned()
    }
}
