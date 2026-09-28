//! One running provider and blocking calls to it.
//!
//! The engine is synchronous; each connection owns a single-threaded tokio
//! runtime and blocks on its calls. A call that fails because the process
//! died says so, with its exit status (`CallError::Crashed`).

use super::pb;
use super::pb::provider_client::ProviderClient;
use super::spawn::{self, Started};
use anyhow::{Context, Result, bail};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::{Child, ChildStdin, ExitStatus};
use std::time::Duration;
use tonic::transport::Channel;

/// How an Apply call failed.
#[derive(Debug)]
pub enum CallError {
    /// The provider refused the call: nothing changed.
    Refused(String),
    /// The call may have taken effect, but no answer came
    /// (`DEADLINE_EXCEEDED`).
    MaybeApplied(String),
    /// The provider process died during the call.
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

pub struct Conn {
    /// The name the provider's handshake gave: what state records.
    pub name: String,
    pub capabilities: Vec<String>,
    /// What was started, for messages.
    pub program: String,
    rt: tokio::runtime::Runtime,
    client: ProviderClient<Channel>,
    child: RefCell<Child>,
    stdin: RefCell<Option<ChildStdin>>,
    exited: Cell<Option<ExitStatus>>,
    /// Where it was dialed: a unix socket is removed once it is gone.
    address: String,
}

impl Conn {
    /// Start the provider at `exe`, dial it and shake hands.
    pub fn start(exe: &Path) -> Result<Conn> {
        let Started {
            child,
            stdin,
            address,
        } = spawn::start(exe)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("start the provider client runtime")?;
        let program = exe.display().to_string();
        let channel = rt
            .block_on(super::transport::dial(&address))
            .with_context(|| format!("dial provider {program} at {address}"))?;
        let mut conn = Conn {
            name: String::new(),
            capabilities: Vec::new(),
            program,
            rt,
            client: ProviderClient::new(channel)
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            child: RefCell::new(child),
            stdin: RefCell::new(stdin),
            exited: Cell::new(None),
            address,
        };
        let hs = conn.call(|mut c| async move {
            c.handshake(pb::HandshakeRequest {
                protocol_version: spawn::VERSION,
            })
            .await
        })?;
        if hs.protocol_version != spawn::VERSION {
            bail!(
                "provider {} speaks protocol version {}; this dform speaks {}",
                conn.program,
                hs.protocol_version,
                spawn::VERSION
            );
        }
        conn.name = hs.name;
        conn.capabilities = hs.capabilities;
        Ok(conn)
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Whether the process has exited, waiting up to `grace` for it.
    fn exit_status(&self, grace: Duration) -> Option<ExitStatus> {
        if let Some(s) = self.exited.get() {
            return Some(s);
        }
        let deadline = std::time::Instant::now() + grace;
        loop {
            if let Ok(Some(s)) = self.child.borrow_mut().try_wait() {
                self.exited.set(Some(s));
                return Some(s);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether the process is gone.
    pub fn is_dead(&self) -> bool {
        self.exit_status(Duration::ZERO).is_some()
    }

    fn classify(&self, s: tonic::Status) -> CallError {
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

    fn name_or_program(&self) -> &str {
        if self.name.is_empty() {
            &self.program
        } else {
            &self.name
        }
    }

    /// One call, its failure classified.
    pub fn try_call<T, F, Fut>(&self, f: F) -> std::result::Result<T, CallError>
    where
        F: FnOnce(ProviderClient<Channel>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<tonic::Response<T>, tonic::Status>>,
    {
        if let Some(status) = self.exited.get() {
            return Err(CallError::Crashed(format!(
                "the provider {} has exited ({status})",
                self.name_or_program()
            )));
        }
        self.rt
            .block_on(f(self.client.clone()))
            .map(tonic::Response::into_inner)
            .map_err(|s| self.classify(s))
    }

    /// One call; any failure is an error carrying the provider's message.
    pub fn call<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: FnOnce(ProviderClient<Channel>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<tonic::Response<T>, tonic::Status>>,
    {
        self.try_call(f).map_err(anyhow::Error::new)
    }

    /// Plan calls, all in flight at once; the answers in order.
    pub fn plan_all(
        &self,
        reqs: Vec<pb::PlanRequest>,
    ) -> Vec<std::result::Result<pb::PlanResponse, CallError>> {
        if let Some(status) = self.exited.get() {
            let m = format!(
                "the provider {} has exited ({status})",
                self.name_or_program()
            );
            return reqs
                .iter()
                .map(|_| Err(CallError::Crashed(m.clone())))
                .collect();
        }
        let client = self.client.clone();
        let mut answers = self.rt.block_on(async move {
            let mut set = tokio::task::JoinSet::new();
            for (i, req) in reqs.into_iter().enumerate() {
                let mut c = client.clone();
                set.spawn(async move { (i, c.plan(req).await) });
            }
            let mut out = Vec::new();
            while let Some(r) = set.join_next().await {
                out.push(r.expect("a Plan call panicked"));
            }
            out
        });
        answers.sort_by_key(|(i, _)| *i);
        answers
            .into_iter()
            .map(|(_, r)| {
                r.map(tonic::Response::into_inner)
                    .map_err(|s| self.classify(s))
            })
            .collect()
    }

    /// A server-streamed Query, collected.
    pub fn query(&self, req: pb::QueryRequest) -> Result<Vec<pb::Row>> {
        let mut stream = self.call(|mut c| async move { c.query(req).await })?;
        let mut rows = Vec::new();
        loop {
            match self.rt.block_on(stream.message()) {
                Ok(Some(r)) => rows.push(r),
                Ok(None) => return Ok(rows),
                Err(s) => return Err(anyhow::Error::new(self.classify(s))),
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Closing stdin asks the provider to exit; it keeps nothing in
        // memory that is not already written.
        self.stdin.borrow_mut().take();
        let mut child = self.child.borrow_mut();
        if self.exited.get().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        super::transport::remove(&self.address);
    }
}
