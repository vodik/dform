//! The process's connections, one entry per endpoint ([`Route`]): an
//! HTTP/2 connection shared by every request to it, as streams, or the
//! HTTP/1.1 connections kept alive between requests, one request on each
//! at a time. The first connection to an endpoint is made alone: the
//! requests that come meanwhile wait to learn whether the server speaks
//! HTTP/2 (they share that connection, one TLS handshake for all) or
//! HTTP/1.1 (each opens its own when none is idle). Its connections are
//! driven by one runtime on a thread of its own, which the callers, all
//! blocking, wait on ([`Pool::run`]).

use super::Route;
use super::connect::{self, Conn};
use super::tls::{self, Profile};
use crate::plugin::credentials::Credential;
use crate::plugin::host::Error;
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http1;
use rustls::ClientConfig;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// The most HTTP/1.1 connections an endpoint keeps idle: a burst of
/// requests opens as many as it needs, and closes all but these.
const IDLE: usize = 8;

pub(super) struct Pool {
    runtime: tokio::runtime::Handle,
    endpoints: Mutex<HashMap<Route, Arc<Endpoint>>>,
    /// A TLS configuration per credential's TLS, shared by its endpoints.
    tls: Mutex<HashMap<Profile, Arc<ClientConfig>>>,
}

/// The process's pool, its runtime started on first use; an error when
/// its thread cannot start.
pub(super) fn pool() -> Result<&'static Pool, Error> {
    static POOL: OnceLock<Result<Pool, String>> = OnceLock::new();
    POOL.get_or_init(Pool::start)
        .as_ref()
        .map_err(|e| Error::retryable(format!("the HTTP client: {e}")))
}

/// A lock that is never poisoned: each use is one lookup, insert or
/// swap, so a panic leaves the map whole.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Pool {
    fn start() -> Result<Pool, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("its runtime: {e}"))?;
        let runtime = rt.handle().clone();
        std::thread::Builder::new()
            .name("dform-http".into())
            .spawn(move || rt.block_on(std::future::pending::<()>()))
            .map_err(|e| format!("its thread: {e}"))?;
        Ok(Pool {
            runtime,
            endpoints: Mutex::default(),
            tls: Mutex::default(),
        })
    }

    /// `fut` on the pool's runtime, waited for from this thread (which
    /// may be another runtime's blocking thread: no nesting).
    pub(super) fn run<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.runtime.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv()
            .expect("the pool's runtime runs every task to its end: its thread never returns")
    }

    /// Start `fut` on the pool's runtime.
    pub(super) fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(fut);
    }

    /// The endpoint `route` names, with `cred`'s TLS.
    pub(super) fn endpoint(
        &self,
        route: Route,
        cred: Option<&Credential>,
    ) -> Result<Arc<Endpoint>, Error> {
        if let Some(e) = lock(&self.endpoints).get(&route) {
            return Ok(e.clone());
        }
        let tls = match route.https {
            true => Some(self.tls(route.tls, cred)?),
            false => None,
        };
        let made = Arc::new(Endpoint {
            route: route.clone(),
            tls,
            held: Mutex::default(),
            first: tokio::sync::Mutex::new(()),
        });
        Ok(lock(&self.endpoints).entry(route).or_insert(made).clone())
    }

    fn tls(&self, profile: Profile, cred: Option<&Credential>) -> Result<Arc<ClientConfig>, Error> {
        if let Some(c) = lock(&self.tls).get(&profile) {
            return Ok(c.clone());
        }
        let made = tls::config(cred)?;
        Ok(lock(&self.tls).entry(profile).or_insert(made).clone())
    }
}

/// One endpoint's connections.
pub(super) struct Endpoint {
    pub(super) route: Route,
    tls: Option<Arc<ClientConfig>>,
    held: Mutex<Held>,
    /// Held while the first connection is made, and while an HTTP/2 one
    /// is made again.
    first: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Held {
    /// The server's HTTP/2 connection, shared, and which one it is.
    h2: Option<(u64, h2::client::SendRequest<Bytes>)>,
    /// How many HTTP/2 connections were made to it.
    made: u64,
    /// The server spoke HTTP/1.1: a request with none idle opens another.
    http1: bool,
    idle: Vec<http1::SendRequest<Full<Bytes>>>,
}

/// A connection a request is sent on.
pub(super) enum Lease {
    /// `which`: the connection's number, to say it is gone ([`Endpoint::lost`]).
    H2 {
        send: h2::client::SendRequest<Bytes>,
        which: u64,
    },
    /// `reused`: kept alive since an earlier request, so the server may
    /// have closed it meanwhile.
    H1 {
        send: http1::SendRequest<Full<Bytes>>,
        reused: bool,
    },
}

impl Endpoint {
    fn held(&self) -> MutexGuard<'_, Held> {
        lock(&self.held)
    }

    /// A connection to send on: the HTTP/2 one, an idle HTTP/1.1 one, or
    /// a new one (made alone until the server's protocol is known).
    pub(super) async fn lease(&self) -> Result<Lease, Error> {
        if let Some(l) = self.ready() {
            return Ok(l);
        }
        if self.held().http1 {
            return self.open().await;
        }
        let _first = self.first.lock().await;
        match self.ready() {
            Some(l) => Ok(l),
            None => self.open().await,
        }
    }

    /// A connection to it that needs no new one.
    fn ready(&self) -> Option<Lease> {
        let mut held = self.held();
        if let Some((which, send)) = &held.h2 {
            return Some(Lease::H2 {
                send: send.clone(),
                which: *which,
            });
        }
        while let Some(send) = held.idle.pop() {
            if send.is_ready() {
                return Some(Lease::H1 { send, reused: true });
            }
        }
        None
    }

    /// A new connection.
    async fn open(&self) -> Result<Lease, Error> {
        let conn = connect::dial(&self.route, self.tls.clone()).await?;
        let mut held = self.held();
        Ok(match conn {
            Conn::H2(send) => {
                held.made += 1;
                held.h2 = Some((held.made, send.clone()));
                Lease::H2 {
                    send,
                    which: held.made,
                }
            }
            Conn::H1(send) => {
                held.http1 = true;
                Lease::H1 {
                    send,
                    reused: false,
                }
            }
        })
    }

    /// The HTTP/2 connection `which` is gone: the next request makes
    /// another, unless one has already.
    pub(super) fn lost(&self, which: u64) {
        let mut held = self.held();
        if held.h2.as_ref().is_some_and(|(w, _)| *w == which) {
            held.h2 = None;
        }
    }

    /// An HTTP/1.1 connection whose answer was read to its end: kept for
    /// the next request, unless [`IDLE`] are kept already.
    pub(super) fn give_back(&self, send: http1::SendRequest<Full<Bytes>>) {
        let mut held = self.held();
        if !send.is_closed() && held.idle.len() < IDLE {
            held.idle.push(send);
        }
    }
}
