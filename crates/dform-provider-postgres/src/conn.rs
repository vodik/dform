//! The connection: one client per configured provider, made on first use
//! and made again when the server closed it, on a runtime of the
//! provider's own (the SDK calls a lifecycle function from a thread of
//! the gRPC server's, where no runtime may be entered: each call is
//! spawned here and waited for). Every statement is the simple query
//! protocol's, its rows as text.

use crate::config::Settings;
use crate::tls::Tls;
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_postgres::{Client, SimpleQueryMessage};

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("postgres")
            .enable_all()
            .build()
            .expect("the provider's runtime starts")
    })
}

/// `fut` on the provider's runtime, waited for from this thread.
pub fn block<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv()
        .expect("the provider's runtime answers every call")
}

/// A row: each column by name, its text (`None` for NULL).
pub type Row = BTreeMap<String, Option<String>>;

/// How a statement failed.
#[derive(Debug)]
pub enum Failed {
    /// The server refused it: nothing changed.
    Refused(String),
    /// The connection went while it was in flight: it may have taken
    /// effect.
    Lost(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Refused(m) | Failed::Lost(m) => f.write_str(m),
        }
    }
}

/// A server's refusal as it says it, never the statement (which may
/// carry a verifier): `ERROR: role "x" already exists (42710)`.
fn failed(e: tokio_postgres::Error) -> Failed {
    match e.as_db_error() {
        Some(db) => Failed::Refused(format!(
            "{}: {} ({})",
            db.severity(),
            db.message(),
            db.code().code()
        )),
        None if e.is_closed() => Failed::Lost(format!("the connection closed: {e}")),
        None => Failed::Lost(e.to_string()),
    }
}

/// An open connection.
pub struct Session {
    client: Client,
    /// What carries it through the Kubernetes API, kept while it is open.
    _forwarder: Option<kube::api::Portforwarder>,
}

impl Session {
    /// `sql`'s rows.
    pub fn query(self: &Arc<Self>, sql: String) -> Result<Vec<Row>, Failed> {
        let s = self.clone();
        block(async move {
            let messages = s.client.simple_query(&sql).await.map_err(failed)?;
            let mut rows = Vec::new();
            for m in messages {
                if let SimpleQueryMessage::Row(r) = m {
                    let mut row = Row::new();
                    for (i, c) in r.columns().iter().enumerate() {
                        row.insert(c.name().to_string(), r.get(i).map(String::from));
                    }
                    rows.push(row);
                }
            }
            Ok(rows)
        })
    }

    /// Run `sql`, one statement or several (run as one transaction unless
    /// one of them cannot be: CREATE DATABASE).
    pub fn execute(self: &Arc<Self>, sql: String) -> Result<(), Failed> {
        let s = self.clone();
        block(async move { s.client.batch_execute(&sql).await.map_err(failed) })
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

/// The connection a provider makes when it is first asked something.
pub struct Pool {
    settings: Settings,
    open: Mutex<Option<Arc<Session>>>,
}

impl Pool {
    pub fn new(settings: Settings) -> Pool {
        Pool {
            settings,
            open: Mutex::new(None),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The open session, connecting when there is none (or it closed).
    /// A failure to connect is retryable: nothing was sent.
    pub fn session(&self) -> Result<Arc<Session>> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = open.as_ref().filter(|s| !s.is_closed()) {
            return Ok(s.clone());
        }
        let settings = self.settings.clone();
        let s = block(async move {
            tokio::time::timeout(CONNECT_TIMEOUT, connect(&settings))
                .await
                .unwrap_or_else(|_| Err(anyhow!("no answer in {CONNECT_TIMEOUT:?}")))
        })
        .map_err(|e| anyhow!("retryable: connect to {}: {e:#}", self.settings.account()))?;
        let s = Arc::new(s);
        *open = Some(s.clone());
        Ok(s)
    }
}

/// Connect as `s` says (boxed: the compiler's `Send` check of an async fn
/// that awaits tokio-postgres's and kube's futures fails on their
/// higher-ranked lifetimes; a boxed one is checked where it is made).
fn connect(s: &Settings) -> Pin<Box<dyn Future<Output = Result<Session>> + Send + '_>> {
    Box::pin(connecting(s))
}

/// A handshake's answer: the client and the connection that carries it.
type Handshaken<S> =
    Result<(Client, tokio_postgres::Connection<S, crate::tls::Stream<S>>), tokio_postgres::Error>;

/// `config`'s handshake over `stream`, boxed for the same reason.
fn handshake<S>(
    config: tokio_postgres::Config,
    stream: S,
    tls: Tls,
) -> Pin<Box<dyn Future<Output = Handshaken<S>> + Send>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    Box::pin(async move { config.connect_raw(stream, tls).await })
}

async fn connecting(s: &Settings) -> Result<Session> {
    let mut config = tokio_postgres::Config::new();
    config
        .user(&s.user)
        .dbname(&s.database)
        .application_name("dform")
        .ssl_mode(match s.sslmode {
            crate::config::SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            _ => tokio_postgres::config::SslMode::Require,
        });
    if let Some(p) = &s.password {
        config.password(p.as_bytes());
    }
    let tls = Tls::new(s.sslmode, &s.host, s.root_cert.as_deref())?;
    let (client, forwarder) = match &s.forward {
        None => {
            let tcp = tokio::net::TcpStream::connect((s.host.as_str(), s.port))
                .await
                .with_context(|| format!("{}:{}", s.host, s.port))?;
            let _ = tcp.set_nodelay(true);
            let (client, conn) = handshake(config, tcp, tls)
                .await
                .map_err(|e| anyhow!("{}", failed(e)))?;
            tokio::spawn(conn);
            (client, None)
        }
        Some(f) => {
            let opened = crate::forward::open(f, s.port).await?;
            let (client, conn) = handshake(config, opened.stream, tls)
                .await
                .map_err(|e| anyhow!("through pod {}: {}", opened.pod, failed(e)))?;
            tokio::spawn(conn);
            (client, Some(opened.forwarder))
        }
    };
    Ok(Session {
        client,
        _forwarder: forwarder,
    })
}
