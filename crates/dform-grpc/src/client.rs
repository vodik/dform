//! The process backend: one running provider, reached over gRPC.
//!
//! The engine is synchronous; each connection owns a single-threaded tokio
//! runtime. `submit` spawns the call on it and returns; `next_completed`
//! drives the runtime until some call has answered, so every call in
//! flight runs at once (a plan's Plan calls, `--parallel`'s Apply calls).
//! A call that fails because the process died says so, with its exit
//! status (`CallError::Crashed`).

use crate::pb;
use crate::pb::provider_client::ProviderClient;
use crate::spawn::{self, Env, Started};
use anyhow::{Context, Result};
use dform_core::plugin::Launch;
use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Ticket};
use dform_core::plugin::link::Link;
use std::path::Path;
use std::process::{Child, ChildStdin, ExitStatus};
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::Channel;

type Answer = std::result::Result<Reply, tonic::Status>;

pub struct Conn {
    /// What was started, for messages.
    program: String,
    /// The name the provider's handshake gave, once it has.
    name: String,
    rt: tokio::runtime::Runtime,
    client: ProviderClient<Channel>,
    child: Child,
    stdin: Option<ChildStdin>,
    exited: Option<ExitStatus>,
    /// Where it was dialed: a unix socket is removed once it is gone.
    address: String,
    next: u64,
    /// Answers arrive here from the calls spawned on `rt`.
    tx: mpsc::UnboundedSender<(Ticket, Answer)>,
    rx: mpsc::UnboundedReceiver<(Ticket, Answer)>,
    /// Calls refused before they were sent (the process had exited).
    refused: Vec<(Ticket, CallError)>,
}

impl Conn {
    /// Start the provider at `exe`, with `env`, and dial it.
    pub fn start(exe: &Path, env: &Env) -> Result<Conn> {
        let Started {
            child,
            stdin,
            address,
        } = spawn::start(exe, env)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("start the provider client runtime")?;
        let program = exe.display().to_string();
        let channel = rt
            .block_on(crate::transport::dial(&address))
            .with_context(|| format!("dial provider {program} at {address}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Conn {
            program,
            name: String::new(),
            rt,
            client: ProviderClient::new(channel)
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            child,
            stdin,
            exited: None,
            address,
            next: 0,
            tx,
            rx,
            refused: Vec::new(),
        })
    }

    /// Start the provider at `exe`, with `env`, dial it and shake hands.
    pub fn link(exe: &Path, env: &Env) -> Result<Link> {
        let conn = Conn::start(exe, env)?;
        Link::start(exe.display().to_string(), Box::new(conn))
    }

    /// Whether the process has exited, waiting up to `grace` for it.
    fn exit_status(&mut self, grace: Duration) -> Option<ExitStatus> {
        if let Some(s) = self.exited {
            return Some(s);
        }
        let deadline = std::time::Instant::now() + grace;
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                self.exited = Some(s);
                return Some(s);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn name_or_program(&self) -> &str {
        if self.name.is_empty() {
            &self.program
        } else {
            &self.name
        }
    }

    fn classify(&mut self, s: tonic::Status) -> CallError {
        use tonic::Code;
        let transport = matches!(
            s.code(),
            Code::Unavailable | Code::Unknown | Code::Internal | Code::Cancelled
        );
        let grace = if transport {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        };
        if let Some(status) = self.exit_status(grace) {
            return CallError::Crashed(format!(
                "the provider {} exited during the call ({status})",
                self.name_or_program()
            ));
        }
        match s.code() {
            Code::DeadlineExceeded => CallError::MaybeApplied(s.message().to_string()),
            _ if transport && s.message().is_empty() => CallError::Refused(format!(
                "the provider {} failed: {s}",
                self.name_or_program()
            )),
            _ => CallError::Refused(s.message().to_string()),
        }
    }
}

/// One call over gRPC; a server-streamed Query collected.
async fn send(mut c: ProviderClient<Channel>, call: Call) -> Answer {
    use tonic::Response;
    Ok(match call {
        Call::Handshake(r) => Reply::Handshake(c.handshake(r).await?.into_inner()),
        Call::Configure(r) => Reply::Configure(c.configure(r).await?.into_inner()),
        Call::Schema(r) => Reply::Schema(c.schema(r).await?.into_inner()),
        Call::Query(r) => {
            let mut stream = c.query(r).await.map(Response::into_inner)?;
            let mut rows: Vec<pb::Row> = Vec::new();
            while let Some(row) = stream.message().await? {
                rows.push(row);
            }
            Reply::Query(rows)
        }
        Call::Read(r) => Reply::Read(c.read(r).await?.into_inner()),
        Call::Plan(r) => Reply::Plan(c.plan(r).await?.into_inner()),
        Call::Apply(r) => Reply::Apply(c.apply(r).await?.into_inner()),
        Call::Import(r) => Reply::Import(c.import(r).await?.into_inner()),
    })
}

impl Provider for Conn {
    fn submit(&mut self, call: Call) -> Ticket {
        let t = Ticket(self.next);
        self.next += 1;
        if let Some(status) = self.exited {
            let m = format!(
                "the provider {} has exited ({status})",
                self.name_or_program()
            );
            self.refused.push((t, CallError::Crashed(m)));
            return t;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let _ = tx.send((t, send(client, call).await));
        });
        t
    }

    fn next_completed(&mut self) -> (Ticket, std::result::Result<Reply, CallError>) {
        if let Some((t, e)) = self.refused.pop() {
            return (t, Err(e));
        }
        let (t, answer) = self
            .rt
            .block_on(self.rx.recv())
            .expect("internal: the provider client's channel closed");
        if let Ok(Reply::Handshake(h)) = &answer {
            self.name = h.name.clone();
        }
        (t, answer.map_err(|s| self.classify(s)))
    }

    fn is_dead(&mut self) -> bool {
        self.exit_status(Duration::ZERO).is_some()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Closing stdin asks the provider to exit; it keeps nothing in
        // memory that is not already written.
        self.stdin.take();
        if self.exited.is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        crate::transport::remove(&self.address);
    }
}

/// The CLI's backend: every provider a process, the mock
/// `dform-provider-fake` (`spawn::fake_executable`).
pub struct Process;

impl Launch for Process {
    fn mock(&self) -> Result<Link> {
        Conn::link(&spawn::fake_executable()?, &Env::default())
    }

    fn plugin(&self, exe: &Path) -> Result<Link> {
        Conn::link(exe, &Env::default())
    }
}
