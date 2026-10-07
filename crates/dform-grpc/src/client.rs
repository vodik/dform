//! The process backend: one running provider, reached over gRPC.
//!
//! The engine is synchronous; each connection owns a single-threaded tokio
//! runtime. `submit` spawns the call on it and returns; `next_completed`
//! drives the runtime until some call has answered, so every call in
//! flight runs at once (a plan's Plan calls, `--parallel`'s Apply calls).
//! An Apply's events (its stream, before its result: R-130) are told as
//! they arrive. A call that fails because the process died says so, with
//! its exit status (`CallError::Crashed`). Dropping the connection cancels
//! every call in flight (the runtime goes, and each call's stream with
//! it).

use crate::pb;
use crate::pb::provider_client::ProviderClient;
use crate::spawn::{self, Env, Program, Started};
use anyhow::{Context, Result};
use dform_core::plugin::Launch;
use dform_core::plugin::backend::{Call, CallError, Provider, Reply, Ticket};
use dform_core::plugin::link::Link;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ExitStatus};
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::Channel;

type Answer = std::result::Result<Reply, tonic::Status>;

/// What a call spawned on the runtime sends back.
enum Back {
    Event(Ticket, pb::Event),
    Answer(Ticket, Answer),
}

pub struct Conn {
    /// What was started, for messages.
    program: String,
    /// The name the provider's handshake gave, once it has.
    name: String,
    rt: tokio::runtime::Runtime,
    client: ProviderClient<Channel>,
    /// The connection, for the provider's `Manifest` service.
    channel: Channel,
    child: Child,
    stdin: Option<ChildStdin>,
    exited: Option<ExitStatus>,
    /// Where it was dialed: a unix socket is removed once it is gone.
    address: String,
    next: u64,
    /// Events and answers arrive here from the calls spawned on `rt`.
    tx: mpsc::UnboundedSender<Back>,
    rx: mpsc::UnboundedReceiver<Back>,
    /// Calls refused before they were sent (the process had exited).
    refused: Vec<(Ticket, CallError)>,
}

impl Conn {
    /// Start the provider `program`, with `env`, and dial it.
    pub fn start(program: &Program, env: &Env) -> Result<Conn> {
        let Started {
            child,
            stdin,
            address,
        } = spawn::start(program, env)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("start the provider client runtime")?;
        let program = program.display();
        let channel = rt
            .block_on(crate::transport::dial(&address))
            .with_context(|| format!("dial provider {program} at {address}"))?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Conn {
            program,
            name: String::new(),
            rt,
            client: ProviderClient::new(channel.clone())
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            channel,
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

    /// Start the provider `program`, with `env`, dial it and shake hands.
    pub fn link(program: &Program, env: &Env) -> Result<Link> {
        let conn = Conn::start(program, env)?;
        Link::start(program.display(), Box::new(conn))
    }

    /// What the provider says it uses (its `Manifest` service, R-13b):
    /// `None` when it does not serve one.
    pub fn manifest(&mut self) -> Option<Vec<String>> {
        let mut c = crate::host_pb::manifest_client::ManifestClient::new(self.channel.clone());
        self.rt
            .block_on(c.manifest(crate::host_pb::ManifestRequest {}))
            .ok()
            .map(|r| r.into_inner().imports)
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

/// One call over gRPC; a server-streamed Query collected, an Apply's
/// events told to `event` as they come.
async fn send(mut c: ProviderClient<Channel>, call: Call, event: impl Fn(pb::Event)) -> Answer {
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
        Call::Apply(r) => {
            use pb::apply_event::Kind;
            let mut stream = c.apply(r).await.map(Response::into_inner)?;
            loop {
                match stream.message().await? {
                    Some(pb::ApplyEvent {
                        kind: Some(Kind::Event(e)),
                    }) => event(e),
                    Some(pb::ApplyEvent {
                        kind: Some(Kind::Result(r)),
                    }) => break Reply::Apply(r),
                    Some(pb::ApplyEvent { kind: None }) => {}
                    None => {
                        return Err(tonic::Status::deadline_exceeded(
                            "its Apply stream ended with no result; the call may have taken effect",
                        ));
                    }
                }
            }
        }
        Call::Import(r) => Reply::Import(c.import(r).await?.into_inner()),
        Call::Reveal(r) => Reply::Reveal(c.reveal(r).await?.into_inner()),
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
            let said = tx.clone();
            let answer = send(client, call, move |e| {
                let _ = said.send(Back::Event(t, e));
            })
            .await;
            let _ = tx.send(Back::Answer(t, answer));
        });
        t
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, std::result::Result<Reply, CallError>) {
        if let Some((t, e)) = self.refused.pop() {
            return (t, Err(e));
        }
        let (t, answer) = loop {
            match self
                .rt
                .block_on(self.rx.recv())
                .expect("internal: the provider client's channel closed")
            {
                Back::Event(t, e) => events(t, e),
                Back::Answer(t, answer) => break (t, answer),
            }
        };
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

/// Every provider a process. The mock is the running executable itself
/// (`dform __provider fake`, `Process::Cli`), so it is always of dform's
/// own build, unless `DFORM_PROVIDER_FAKE` names another executable.
pub enum Process {
    /// The `dform` binary's.
    Cli,
    /// A binary that is not `dform` (a test's): the mock is the
    /// executable at this path (`dform-provider-fake`).
    Mock(PathBuf),
}

impl Process {
    fn mock_program(&self) -> Result<Program> {
        if let Some(p) = std::env::var_os(spawn::FAKE_ENV) {
            return Ok(Program::exe(p));
        }
        match self {
            Process::Cli => Program::this("fake"),
            Process::Mock(exe) => Ok(Program::exe(exe)),
        }
    }
}

impl Launch for Process {
    fn mock(&self) -> Result<Link> {
        Conn::link(&self.mock_program()?, &Env::default())
    }

    fn plugin(&self, exe: &Path) -> Result<Link> {
        Conn::link(&Program::exe(exe), &Env::default())
    }
}
